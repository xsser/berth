//! The GUI's protocol state (integrate.md §1–§4), free of winit and wgpu so
//! it can be driven by tests: workspaces and sessions (listed once, then
//! kept current from `WorkspaceUpdated` / `SessionUpdated` /
//! `SessionRemoved` / `AgentChanged` / `Exited`), the focused session's
//! [`SessionView`], sidebar preview subscriptions, history prefetch, the
//! paste pipeline, notification decisions and the list of errors shown to
//! the user.
//!
//! Subscription rules that follow from the daemon's semantics:
//! - `Unsubscribe` drops *every* subscription of this connection on that
//!   session, the `Attach` included, so it is never sent for the focused
//!   session; focusing a session that has a preview sends `Unsubscribe`
//!   first and `Attach` after it (requests are handled in order).
//! - Leaving a session sends `Detach`; its card re-subscribes a preview if
//!   it is visible.
//! - The answer to `Attach` is the first full screen and the new `seq`
//!   baseline; other screens for the session are ignored until it arrives.
//!
//! Card details are fetched on demand and cached: a hovered card's newest
//! events (`ListEvents`, again after the session's agent changed) and, for
//! a visible dormant card whose agent can be resumed, the command
//! `Revive { ResumeAgent }` would run (`ResumeCommand`, again when the
//! session's agent id, transcript or cwd changed). Answers are matched to
//! the request that is still wanted by id. A berthd from before these
//! requests answers them with an undecodable-message error: then they are
//! no longer sent on this connection, with one info notice.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use berth_core::{
    AgentKind, DaemonMsg, Dims, Event, EventEntry, LineSnapshot, Request, ReviveMode, ScreenUpdate,
    SessionId, SessionMeta, StyleTable, SubscribeMode, Workspace, WorkspaceId,
};

use crate::client::Client;
use crate::notify::{self, Policy};
use crate::paste::{self, JobReply, PasteJob};
use crate::selection;
use crate::session_view::SessionView;

pub const PREVIEW: SubscribeMode = SubscribeMode::Preview { rows: 3, max_hz: 4 };
pub const PREVIEW_ROWS: usize = 3;
pub const RESIZE_DEBOUNCE: Duration = Duration::from_millis(50);
/// A card must stay out of view this long before its preview is dropped
/// (no subscribe/unsubscribe churn while scrolling).
pub const UNSUBSCRIBE_AFTER: Duration = Duration::from_secs(1);
const INFO_TTL: Duration = Duration::from_secs(6);
/// Events shown on a hovered card.
pub const HOVER_EVENTS: u32 = 5;
const MAX_NOTICES: usize = 6;
const FALLBACK_DIMS: Dims = Dims { cols: 80, rows: 24 };

/// The connection as the controller sees it.
pub trait Outbound {
    /// Queue a request; returns its id.
    fn send(&mut self, req: Request) -> Result<u32>;
}

impl Outbound for Client {
    fn send(&mut self, req: Request) -> Result<u32> {
        Client::send(self, req)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    /// Stays until dismissed.
    Error,
    /// Expires after a few seconds.
    Info,
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub kind: NoticeKind,
    pub text: String,
    /// How many times this text was reported in a row.
    pub count: u32,
    pub at: Instant,
}

#[derive(Clone, Debug, Default)]
pub struct Preview {
    pub lines: Vec<LineSnapshot>,
    pub styles: StyleTable,
}

/// A card's newest events (`ListEvents`), for its hover details.
#[derive(Clone, Debug, Default)]
pub struct RecentEvents {
    /// Newest first, as berthd sends them; `None` until the first answer.
    pub events: Option<Vec<EventEntry>>,
    /// Why the last request failed.
    pub error: Option<String>,
    /// The agent changed since the request was sent.
    stale: bool,
    /// The request whose answer is wanted.
    in_flight: Option<u32>,
}

/// What `Revive { ResumeAgent }` would run for a session (display only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumePreview {
    pub cwd: PathBuf,
    /// The argv, or why berthd would refuse.
    pub command: std::result::Result<Vec<String>, String>,
}

/// What a resume command is computed from.
type ResumeKey = (AgentKind, Option<String>, Option<PathBuf>, PathBuf);

fn resume_key(m: &SessionMeta) -> ResumeKey {
    (
        m.agent.kind.clone(),
        m.agent.external_id.clone(),
        m.agent.transcript_path.clone(),
        m.cwd.clone(),
    )
}

/// A dormant card offers "Resume" when its agent left an id to resume.
pub fn resumable(m: &SessionMeta) -> bool {
    !m.is_live() && m.agent.kind.is_agent() && m.agent.external_id.is_some()
}

#[derive(Clone, Debug)]
struct ResumeEntry {
    key: ResumeKey,
    preview: Option<ResumePreview>,
    in_flight: Option<u32>,
}

/// A question the UI must ask before acting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Confirm {
    /// ⌘W on a live session running an agent or a command.
    Kill {
        session: SessionId,
        title: String,
        what: String,
    },
    /// ⌘W on a dormant / restored session: its history is deleted.
    Delete { session: SessionId, title: String },
}

/// Traffic seen since the last [`Controller::take_counters`] (`--stats`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// `Screen` updates applied to the focused view.
    pub screens: u64,
    /// Sidebar `Preview` updates, and how many sessions they came from.
    pub previews: u64,
    pub preview_sessions: HashSet<SessionId>,
}

/// Something only the app can do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Notify { title: String, body: String },
}

#[derive(Clone, Debug)]
enum Awaiting {
    ListWorkspaces,
    ListSessions,
    CreateWorkspace {
        then_session: bool,
    },
    CreateSession,
    Attach,
    Subscribe,
    Fetch(SessionId),
    /// `ListEvents` for a hovered card.
    Events(SessionId),
    /// `ResumeCommand` for a dormant card.
    Resume(SessionId),
    /// Only errors matter; the label says what failed.
    Command(&'static str),
}

struct Sub {
    last_seen: Instant,
}

pub struct Controller {
    connected: bool,
    workspaces: Vec<Workspace>,
    sessions: HashMap<SessionId, SessionMeta>,
    previews: HashMap<SessionId, Preview>,
    focused: Option<SessionId>,
    view: Option<SessionView>,
    /// Session attached on the current connection.
    attached_to: Option<SessionId>,
    /// Outstanding `Attach`; its answer is the baseline screen.
    attach_id: Option<u32>,
    subs: HashMap<SessionId, Sub>,
    pending: HashMap<u32, Awaiting>,
    notices: Vec<Notice>,
    grid: Dims,
    sent_dims: Option<Dims>,
    resize_due: Option<Instant>,
    /// The one paste in progress. Known v1 limit: pasting into another
    /// session cancels it (with an error notice naming the bytes not sent).
    paste: Option<(SessionId, PasteJob)>,
    policy: Policy,
    window_focused: bool,
    confirm: Option<Confirm>,
    loaded_workspaces: bool,
    loaded_sessions: bool,
    mark_read_sent: HashSet<SessionId>,
    /// `--session`: focus this id (or unique prefix) once listed.
    want: Option<String>,
    /// Create a session when the daemon has none (first start).
    pub auto_session: bool,
    auto_created: bool,
    /// Command for new sessions (`None`: the daemon's login shell).
    pub new_session_command: Option<Vec<String>>,
    counters: Counters,
    recent: HashMap<SessionId, RecentEvents>,
    resume: HashMap<SessionId, ResumeEntry>,
}

impl Controller {
    pub fn new(notify_on: Vec<String>) -> Controller {
        Controller {
            connected: false,
            workspaces: Vec::new(),
            sessions: HashMap::new(),
            previews: HashMap::new(),
            focused: None,
            view: None,
            attached_to: None,
            attach_id: None,
            subs: HashMap::new(),
            pending: HashMap::new(),
            notices: Vec::new(),
            grid: Dims::default(),
            sent_dims: None,
            resize_due: None,
            paste: None,
            policy: Policy::new(notify_on),
            window_focused: true,
            confirm: None,
            loaded_workspaces: false,
            loaded_sessions: false,
            mark_read_sent: HashSet::new(),
            want: None,
            auto_session: true,
            auto_created: false,
            new_session_command: None,
            counters: Counters::default(),
            recent: HashMap::new(),
            resume: HashMap::new(),
        }
    }

    // -- accessors -------------------------------------------------------

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Both lists arrived on the current connection.
    pub fn is_loaded(&self) -> bool {
        self.loaded_workspaces && self.loaded_sessions
    }

    pub fn focused(&self) -> Option<SessionId> {
        self.focused
    }

    pub fn focused_meta(&self) -> Option<&SessionMeta> {
        self.sessions.get(&self.focused?)
    }

    pub fn session(&self, sid: SessionId) -> Option<&SessionMeta> {
        self.sessions.get(&sid)
    }

    pub fn view(&self) -> Option<&SessionView> {
        self.view.as_ref()
    }

    pub fn view_mut(&mut self) -> Option<&mut SessionView> {
        self.view.as_mut()
    }

    pub fn preview(&self, sid: SessionId) -> Option<&Preview> {
        self.previews.get(&sid)
    }

    /// The hovered card's newest events, once asked for ([`Self::hover`]).
    pub fn recent_events(&self, sid: SessionId) -> Option<&RecentEvents> {
        self.recent.get(&sid)
    }

    /// The resume command of a dormant card, once berthd answered.
    pub fn resume_preview(&self, sid: SessionId) -> Option<&ResumePreview> {
        self.resume
            .get(&sid)
            .filter(|e| self.sessions.get(&sid).map(resume_key).as_ref() == Some(&e.key))
            .and_then(|e| e.preview.as_ref())
    }

    /// Live sessions whose agent state needs attention (Dock badge).
    pub fn attention_count(&self) -> usize {
        self.sessions
            .values()
            .filter(|m| m.is_live() && m.agent.state.needs_attention())
            .count()
    }

    pub fn notices(&self) -> &[Notice] {
        &self.notices
    }

    pub fn confirm(&self) -> Option<&Confirm> {
        self.confirm.as_ref()
    }

    #[cfg(test)]
    pub fn is_pasting(&self) -> bool {
        self.paste.is_some()
    }

    /// Workspaces in display order.
    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    pub fn workspace(&self, id: WorkspaceId) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    /// Live sessions of a workspace in display order.
    pub fn live_in(&self, ws: WorkspaceId) -> Vec<&SessionMeta> {
        let mut v: Vec<&SessionMeta> = self
            .sessions
            .values()
            .filter(|m| m.workspace == ws && m.is_live())
            .collect();
        v.sort_by_key(|m| (m.order, m.created_at_ms, m.id));
        v
    }

    /// Live sessions whose workspace is unknown.
    pub fn live_orphans(&self) -> Vec<&SessionMeta> {
        let mut v: Vec<&SessionMeta> = self
            .sessions
            .values()
            .filter(|m| m.is_live() && self.workspace(m.workspace).is_none())
            .collect();
        v.sort_by_key(|m| (m.created_at_ms, m.id));
        v
    }

    /// Dormant and restored sessions (history only).
    pub fn dormant(&self) -> Vec<&SessionMeta> {
        let ws_order = |id: WorkspaceId| self.workspace(id).map_or(u32::MAX, |w| w.order);
        let mut v: Vec<&SessionMeta> = self.sessions.values().filter(|m| !m.is_live()).collect();
        v.sort_by_key(|m| (ws_order(m.workspace), m.order, m.created_at_ms, m.id));
        v
    }

    /// Sidebar order, which ⌘1..9 follow.
    pub fn jump_order(&self) -> Vec<SessionId> {
        let mut out: Vec<SessionId> = Vec::new();
        for ws in &self.workspaces {
            out.extend(self.live_in(ws.id).iter().map(|m| m.id));
        }
        out.extend(self.live_orphans().iter().map(|m| m.id));
        out.extend(self.dormant().iter().map(|m| m.id));
        out
    }

    /// `<workspace>/<session>`.
    pub fn qualified_title(&self, sid: SessionId) -> String {
        match self.sessions.get(&sid) {
            Some(m) => {
                let ws = self.workspace(m.workspace).map_or("?", |w| w.name.as_str());
                format!("{ws}/{}", m.title())
            }
            None => sid.short(),
        }
    }

    /// The text of the current selection (⌘C).
    /// Counters since the previous call.
    pub fn take_counters(&mut self) -> Counters {
        std::mem::take(&mut self.counters)
    }

    pub fn copy_text(&self) -> Option<String> {
        let view = self.view.as_ref()?;
        let span = view.selection_span()?;
        let text = selection::selection_text(&span, |v| view.line(v));
        (!text.is_empty()).then_some(text)
    }

    /// Earliest instant `tick` has work (resize debounce, paste retry,
    /// history fetches waiting on a timer). None while disconnected: `tick`
    /// does nothing until the connection is back, so an expired timer would
    /// only spin the event loop.
    pub fn next_deadline(&self) -> Option<Instant> {
        if !self.connected {
            return None;
        }
        let paste = self.paste.as_ref().and_then(|(_, j)| j.deadline());
        let history = self
            .view
            .as_ref()
            .filter(|_| self.attach_id.is_none())
            .and_then(SessionView::next_deadline);
        [self.resize_due, paste, history]
            .into_iter()
            .flatten()
            .min()
    }

    // -- notices ---------------------------------------------------------

    pub fn error(&mut self, text: impl Into<String>) {
        let text = text.into();
        tracing::warn!("{text}");
        self.push_notice(NoticeKind::Error, text);
    }

    pub fn info(&mut self, text: impl Into<String>) {
        let text = text.into();
        tracing::info!("{text}");
        self.push_notice(NoticeKind::Info, text);
    }

    fn push_notice(&mut self, kind: NoticeKind, text: String) {
        let now = Instant::now();
        if let Some(last) = self.notices.last_mut() {
            if last.text == text && last.kind == kind {
                last.count += 1;
                last.at = now;
                return;
            }
        }
        self.notices.push(Notice {
            kind,
            text,
            count: 1,
            at: now,
        });
        if self.notices.len() > MAX_NOTICES {
            // Drop the oldest info first; errors only when nothing else is left.
            let idx = self
                .notices
                .iter()
                .position(|n| n.kind == NoticeKind::Info)
                .unwrap_or(0);
            self.notices.remove(idx);
        }
    }

    pub fn dismiss_notice(&mut self, idx: usize) {
        if idx < self.notices.len() {
            self.notices.remove(idx);
        }
    }

    // -- connection lifecycle ------------------------------------------------

    /// Set before the first connection: focus this session once listed.
    pub fn want_session(&mut self, id_or_prefix: String) {
        self.want = Some(id_or_prefix);
    }

    pub fn on_connected(&mut self, out: &mut dyn Outbound) {
        self.connected = true;
        self.pending.clear();
        self.subs.clear();
        self.attached_to = None;
        self.attach_id = None;
        self.sent_dims = None;
        self.mark_read_sent.clear();
        self.recent.clear();
        self.resume.clear();
        self.loaded_workspaces = false;
        self.loaded_sessions = false;
        self.send(out, Request::ListWorkspaces, Some(Awaiting::ListWorkspaces));
        self.send(out, Request::ListSessions, Some(Awaiting::ListSessions));
    }

    pub fn on_disconnected(&mut self, reason: &str) {
        let was = self.connected;
        self.connected = false;
        self.pending.clear();
        self.subs.clear();
        self.attached_to = None;
        self.attach_id = None;
        if let Some((_, job)) = self.paste.take() {
            if !job.is_done() {
                self.error(format!(
                    "粘贴中断：与 berthd 的连接已断开，{} 字节未发送",
                    job.queued_bytes()
                ));
            }
        }
        if was {
            self.error(format!("与 berthd 的连接已断开：{reason}"));
        }
    }

    fn send(
        &mut self,
        out: &mut dyn Outbound,
        req: Request,
        awaiting: Option<Awaiting>,
    ) -> Option<u32> {
        match out.send(req) {
            Ok(id) => {
                if let Some(a) = awaiting {
                    self.pending.insert(id, a);
                }
                Some(id)
            }
            Err(e) => {
                self.error(format!("请求未能发送：{e:#}"));
                None
            }
        }
    }

    fn grid_dims(&self) -> Dims {
        if self.grid.cols < 2 || self.grid.rows < 1 {
            FALLBACK_DIMS
        } else {
            self.grid
        }
    }

    // -- inbound -----------------------------------------------------------

    /// Apply one daemon message.
    pub fn handle(&mut self, out: &mut dyn Outbound, msg: DaemonMsg, now: Instant) -> Vec<Effect> {
        let mut effects = Vec::new();
        let reply_to = msg.reply_to;
        if let Event::Error { message } = msg.event {
            self.on_error(out, reply_to, message, now);
            return effects;
        }
        let awaiting = reply_to.and_then(|id| self.pending.remove(&id));
        match msg.event {
            Event::Error { .. } => unreachable!("handled above"),
            Event::Workspaces(mut list) => {
                list.sort_by_key(|w| (w.order, w.created_at_ms));
                self.workspaces = list;
                self.loaded_workspaces = true;
                self.after_listing(out, now);
            }
            Event::WorkspaceUpdated(ws) => {
                let id = ws.id;
                match self.workspaces.iter_mut().find(|w| w.id == id) {
                    Some(w) => *w = ws,
                    None => self.workspaces.push(ws),
                }
                self.workspaces.sort_by_key(|w| (w.order, w.created_at_ms));
                if let Some(Awaiting::CreateWorkspace { then_session: true }) = awaiting {
                    self.create_session(out, id);
                }
            }
            Event::WorkspaceRemoved(id) => self.workspaces.retain(|w| w.id != id),
            Event::Sessions(list) => {
                self.sessions = list.into_iter().map(|m| (m.id, m)).collect();
                for m in self.sessions.values() {
                    self.policy.seed(m.id, &m.agent.state);
                }
                let sessions = &self.sessions;
                self.previews.retain(|sid, _| sessions.contains_key(sid));
                self.loaded_sessions = true;
                self.after_listing(out, now);
            }
            Event::SessionUpdated(meta) => self.on_session_updated(out, meta, awaiting, now),
            Event::SessionRemoved(sid) => self.on_session_removed(out, sid, now),
            Event::Screen(u) => self.on_screen(reply_to, &u),
            Event::Lines {
                session,
                start,
                lines,
                styles,
            } => {
                if let (Some(view), Some(id)) = (self.view.as_mut(), reply_to) {
                    if view.id == session {
                        view.apply_lines(id, start, lines, &styles);
                    }
                }
            }
            Event::Preview {
                session,
                lines,
                styles,
            } => {
                self.counters.previews += 1;
                self.counters.preview_sessions.insert(session);
                let p = self.previews.entry(session).or_default();
                p.lines = lines;
                p.styles = StyleTable::new();
                p.styles.apply(&styles);
            }
            Event::Title { session, title } => {
                if let Some(m) = self.sessions.get_mut(&session) {
                    m.title_auto = title;
                }
            }
            Event::Cwd { session, path } => {
                if let Some(m) = self.sessions.get_mut(&session) {
                    m.cwd = path;
                }
            }
            Event::Bell { .. } => {}
            Event::Notify {
                session,
                title,
                body,
            } => {
                if self
                    .policy
                    .program_notify(session, self.attended(session), now)
                {
                    let body = match title {
                        Some(t) if !t.is_empty() => format!("{t}: {body}"),
                        _ => body,
                    };
                    effects.push(Effect::Notify {
                        title: self.qualified_title(session),
                        body,
                    });
                }
            }
            Event::AgentChanged { session, agent } => {
                let attended = self.attended(session);
                if self
                    .policy
                    .agent_changed(session, &agent.state, attended, now)
                {
                    effects.push(Effect::Notify {
                        title: self.qualified_title(session),
                        body: notify::body(&agent.kind, &agent.state),
                    });
                }
                if let Some(m) = self.sessions.get_mut(&session) {
                    m.agent = agent;
                }
                if let Some(r) = self.recent.get_mut(&session) {
                    r.stale = true;
                }
            }
            Event::Exited { session, .. } => {
                if self.paste.as_ref().is_some_and(|(s, _)| *s == session) {
                    self.paste = None;
                    self.error("粘贴中断：session 已退出");
                }
            }
            Event::Status(_) => {
                if let (Some(id), Some((_, job))) = (reply_to, self.paste.as_mut()) {
                    if job.on_reply(id, now) == JobReply::Progress {
                        self.pump_paste(out, now);
                    }
                }
            }
            Event::Ok => {}
            Event::Events { session, events } => {
                if let Some(r) = self.recent.get_mut(&session) {
                    if reply_to.is_some() && r.in_flight == reply_to {
                        r.in_flight = None;
                        r.events = Some(events);
                        r.error = None;
                    }
                }
            }
            Event::ResumeCommand {
                session,
                cwd,
                command,
            } => {
                if let Some(e) = self.resume.get_mut(&session) {
                    if reply_to.is_some() && e.in_flight == reply_to {
                        e.in_flight = None;
                        e.preview = Some(ResumePreview { cwd, command });
                    }
                }
            }
            Event::Hello { .. } | Event::Incompatible { .. } => {
                tracing::debug!("unexpected handshake message after Hello");
            }
        }
        effects
    }

    fn attended(&self, sid: SessionId) -> bool {
        self.focused == Some(sid) && self.window_focused
    }

    fn on_error(
        &mut self,
        out: &mut dyn Outbound,
        reply_to: Option<u32>,
        message: String,
        now: Instant,
    ) {
        if let (Some(id), Some((_, job))) = (reply_to, self.paste.as_mut()) {
            match job.on_error(id, &message, now) {
                JobReply::Unrelated => {}
                JobReply::Progress => return,
                JobReply::Failed(text) => {
                    self.paste = None;
                    self.error(format!("粘贴失败：{text}"));
                    return;
                }
            }
        }
        let awaiting = reply_to.and_then(|id| self.pending.remove(&id));
        let text = match awaiting {
            Some(Awaiting::Events(sid)) => {
                if let Some(r) = self.recent.get_mut(&sid) {
                    if r.in_flight == reply_to {
                        r.in_flight = None;
                        r.error = Some(message);
                    }
                }
                return;
            }
            Some(Awaiting::Resume(sid)) => {
                if let Some(e) = self.resume.get_mut(&sid) {
                    if e.in_flight == reply_to {
                        e.in_flight = None;
                        e.preview = Some(ResumePreview {
                            cwd: PathBuf::new(),
                            command: Err(message),
                        });
                    }
                }
                return;
            }
            Some(Awaiting::Attach) => {
                self.attach_id = None;
                self.attached_to = None;
                format!("无法打开 session：{message}")
            }
            Some(Awaiting::Subscribe) => format!("无法订阅侧栏预览：{message}"),
            Some(Awaiting::Fetch(sid)) => {
                if let (Some(view), Some(id)) = (self.view.as_mut(), reply_to) {
                    if view.id == sid {
                        view.fetch_failed(id, now);
                    }
                }
                format!("读取历史失败：{message}")
            }
            Some(Awaiting::ListWorkspaces) | Some(Awaiting::ListSessions) => {
                format!("读取 session 列表失败：{message}")
            }
            Some(Awaiting::CreateWorkspace { .. }) => format!("新建 workspace 失败：{message}"),
            Some(Awaiting::CreateSession) => format!("新建 session 失败：{message}"),
            Some(Awaiting::Command(what)) => format!("{what}失败：{message}"),
            None if message.starts_with("backpressure:") => {
                format!("输入被拒绝（程序没有读取输入）：{message}")
            }
            None => format!("berthd：{message}"),
        };
        self.error(text);
        // A failed attach leaves the focused session without a screen; keep
        // the placeholder rather than retrying in a loop.
        let _ = out;
    }

    fn after_listing(&mut self, out: &mut dyn Outbound, now: Instant) {
        if !self.is_loaded() {
            return;
        }
        if let Some(want) = self.want.take() {
            match self.find_session(&want) {
                Ok(sid) => self.focused = Some(sid),
                Err(e) => self.error(e),
            }
        }
        match self.focused {
            Some(sid) if self.sessions.contains_key(&sid) => self.focus(out, sid, now),
            _ => {
                self.focused = None;
                self.view = None;
                if let Some(first) = self.jump_order().first().copied() {
                    self.focus(out, first, now);
                }
            }
        }
        if self.sessions.is_empty() && self.auto_session && !self.auto_created {
            self.auto_created = true;
            self.new_session(out);
        }
    }

    /// A session id or unique prefix (with or without dashes).
    pub fn find_session(&self, want: &str) -> std::result::Result<SessionId, String> {
        let want = want.to_ascii_lowercase();
        let hits: Vec<SessionId> = self
            .sessions
            .keys()
            .filter(|sid| {
                let full = sid.to_string();
                full.starts_with(&want) || full.replace('-', "").starts_with(&want)
            })
            .copied()
            .collect();
        match hits.as_slice() {
            [one] => Ok(*one),
            [] => Err(format!("没有 id 以 {want} 开头的 session")),
            _ => Err(format!("{want} 匹配多个 session，请写更长的前缀")),
        }
    }

    fn on_session_updated(
        &mut self,
        out: &mut dyn Outbound,
        meta: SessionMeta,
        awaiting: Option<Awaiting>,
        now: Instant,
    ) {
        let sid = meta.id;
        let was_live = self.sessions.get(&sid).map(SessionMeta::is_live);
        if was_live.is_none() {
            self.policy.seed(sid, &meta.agent.state);
        }
        if !meta.unread {
            self.mark_read_sent.remove(&sid);
        }
        let live = meta.is_live();
        self.sessions.insert(sid, meta);
        if matches!(awaiting, Some(Awaiting::CreateSession)) {
            self.focus(out, sid, now);
            return;
        }
        if self.focused == Some(sid) {
            if was_live == Some(false) && live && self.connected {
                // Revived: the actor may be a new one (after a crash) that
                // has no subscription of ours — attach again.
                self.attached_to = None;
                self.focus(out, sid, now);
            } else {
                self.mark_read(out, sid);
            }
        }
    }

    fn on_session_removed(&mut self, out: &mut dyn Outbound, sid: SessionId, now: Instant) {
        self.sessions.remove(&sid);
        self.previews.remove(&sid);
        self.subs.remove(&sid);
        self.policy.forget(sid);
        self.mark_read_sent.remove(&sid);
        self.recent.remove(&sid);
        self.resume.remove(&sid);
        if self.paste.as_ref().is_some_and(|(s, _)| *s == sid) {
            self.paste = None;
        }
        if self.confirm.as_ref().is_some_and(|c| match c {
            Confirm::Kill { session, .. } | Confirm::Delete { session, .. } => *session == sid,
        }) {
            self.confirm = None;
        }
        if self.focused == Some(sid) {
            self.focused = None;
            self.view = None;
            self.attached_to = None;
            self.attach_id = None;
            if let Some(first) = self.jump_order().first().copied() {
                self.focus(out, first, now);
            }
        }
    }

    fn on_screen(&mut self, reply_to: Option<u32>, u: &ScreenUpdate) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        if view.id != u.session {
            return;
        }
        let baseline = reply_to.is_some() && reply_to == self.attach_id;
        if baseline {
            self.attach_id = None;
        } else if self.attach_id.is_some() {
            return; // waiting for the Attach answer
        }
        if view.apply_screen(u, baseline) {
            self.counters.screens += 1;
        }
    }

    // -- focus, visibility, geometry ------------------------------------

    /// Show `sid` in the main view.
    pub fn focus(&mut self, out: &mut dyn Outbound, sid: SessionId, now: Instant) {
        if !self.sessions.contains_key(&sid) {
            return;
        }
        if self.focused == Some(sid) && (self.attached_to == Some(sid) || !self.connected) {
            self.mark_read(out, sid);
            return;
        }
        if let Some(prev) = self.focused.filter(|p| *p != sid) {
            if self.attached_to == Some(prev) && self.connected {
                self.send(
                    out,
                    Request::Detach { session: prev },
                    Some(Awaiting::Command("离开 session ")),
                );
            }
            self.attached_to = None;
        }
        if self.focused != Some(sid) || self.view.as_ref().is_none_or(|v| v.id != sid) {
            self.view = Some(SessionView::new(sid));
        }
        self.focused = Some(sid);
        self.attach_id = None;
        if self.connected {
            if self.subs.remove(&sid).is_some() {
                self.send(
                    out,
                    Request::Unsubscribe { session: sid },
                    Some(Awaiting::Command("取消预览")),
                );
            }
            let dims = self.grid_dims();
            self.attach_id = self.send(
                out,
                Request::Attach { session: sid, dims },
                Some(Awaiting::Attach),
            );
            if self.attach_id.is_some() {
                self.attached_to = Some(sid);
                self.sent_dims = Some(dims);
            }
            self.mark_read(out, sid);
        }
        let _ = now;
    }

    fn mark_read(&mut self, out: &mut dyn Outbound, sid: SessionId) {
        let unread = self.sessions.get(&sid).is_some_and(|m| m.unread);
        if unread && self.window_focused && self.connected && self.mark_read_sent.insert(sid) {
            self.send(
                out,
                Request::MarkRead { session: sid },
                Some(Awaiting::Command("标记已读")),
            );
        }
    }

    pub fn set_window_focused(&mut self, out: &mut dyn Outbound, focused: bool) {
        self.window_focused = focused;
        if let Some(sid) = self.focused.filter(|_| focused) {
            self.mark_read(out, sid);
        }
    }

    /// Cards currently visible in the sidebar (every frame).
    pub fn set_visible(&mut self, out: &mut dyn Outbound, visible: &[SessionId], now: Instant) {
        if !self.connected || !self.is_loaded() {
            return;
        }
        for &sid in visible {
            if self.sessions.get(&sid).is_some_and(resumable) {
                self.want_resume(out, sid);
            }
            if Some(sid) == self.focused || !self.sessions.contains_key(&sid) {
                continue;
            }
            match self.subs.get_mut(&sid) {
                Some(s) => s.last_seen = now,
                None => {
                    let req = Request::Subscribe {
                        session: sid,
                        mode: PREVIEW,
                    };
                    if self.send(out, req, Some(Awaiting::Subscribe)).is_some() {
                        self.subs.insert(sid, Sub { last_seen: now });
                    }
                }
            }
        }
        let stale: Vec<SessionId> = self
            .subs
            .iter()
            .filter(|(sid, s)| {
                !visible.contains(sid)
                    && now.saturating_duration_since(s.last_seen) >= UNSUBSCRIBE_AFTER
            })
            .map(|(sid, _)| *sid)
            .collect();
        for sid in stale {
            self.subs.remove(&sid);
            if Some(sid) != self.focused {
                self.send(
                    out,
                    Request::Unsubscribe { session: sid },
                    Some(Awaiting::Command("取消预览")),
                );
            }
        }
    }

    /// A card is hovered: fetch its newest events unless they are current
    /// or on their way.
    pub fn hover(&mut self, out: &mut dyn Outbound, sid: SessionId) {
        if !self.connected || !self.sessions.contains_key(&sid) {
            return;
        }
        let r = self.recent.entry(sid).or_default();
        let current = (r.events.is_some() || r.error.is_some()) && !r.stale;
        if current || r.in_flight.is_some() {
            return;
        }
        r.stale = false;
        let req = Request::ListEvents {
            session: sid,
            limit: HOVER_EVENTS,
        };
        let id = self.send(out, req, Some(Awaiting::Events(sid)));
        if let Some(r) = self.recent.get_mut(&sid) {
            r.in_flight = id;
        }
    }

    /// Ask berthd for a dormant card's resume command unless it is known
    /// for the session as it is now, or on its way.
    fn want_resume(&mut self, out: &mut dyn Outbound, sid: SessionId) {
        let Some(key) = self.sessions.get(&sid).map(resume_key) else {
            return;
        };
        if let Some(e) = self.resume.get(&sid) {
            if e.key == key && (e.preview.is_some() || e.in_flight.is_some()) {
                return;
            }
        }
        let id = self.send(
            out,
            Request::ResumeCommand { session: sid },
            Some(Awaiting::Resume(sid)),
        );
        self.resume.insert(
            sid,
            ResumeEntry {
                key,
                preview: None,
                in_flight: id,
            },
        );
    }

    /// The grid size the window fits; sent as `Resize` after
    /// [`RESIZE_DEBOUNCE`] without further change.
    pub fn set_grid(&mut self, dims: Dims, now: Instant) {
        if dims != self.grid {
            self.grid = dims;
            self.resize_due = Some(now + RESIZE_DEBOUNCE);
        }
    }

    /// Timers: resize debounce, paste retries, history prefetch.
    pub fn tick(&mut self, out: &mut dyn Outbound, now: Instant) {
        self.notices.retain(|n| {
            n.kind == NoticeKind::Error || now.saturating_duration_since(n.at) < INFO_TTL
        });
        if !self.connected {
            return;
        }
        if self.resize_due.is_some_and(|t| now >= t) {
            self.resize_due = None;
            let dims = self.grid_dims();
            if let Some(sid) = self.attached_to {
                if self.sent_dims != Some(dims) {
                    self.send(
                        out,
                        Request::Resize { session: sid, dims },
                        Some(Awaiting::Command("调整尺寸")),
                    );
                    self.sent_dims = Some(dims);
                }
            }
        }
        self.pump_paste(out, now);
        self.fetch_history(out, now);
    }

    /// Scroll the main view (positive: up into the history).
    pub fn scroll(&mut self, out: &mut dyn Outbound, lines: i64, now: Instant) {
        if let Some(view) = self.view.as_mut() {
            view.scroll_by(lines);
        }
        self.fetch_history(out, now);
    }

    fn fetch_history(&mut self, out: &mut dyn Outbound, now: Instant) {
        if !self.connected || self.attach_id.is_some() {
            return;
        }
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let sid = view.id;
        let mut failed = None;
        for f in view.wanted_fetches(now) {
            let req = Request::FetchLines {
                session: sid,
                start: f.start,
                count: f.count,
            };
            match out.send(req) {
                Ok(id) => {
                    view.note_fetch(id, f);
                    self.pending.insert(id, Awaiting::Fetch(sid));
                }
                Err(e) => {
                    view.fetch_failed(0, now);
                    failed = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = failed {
            self.error(format!("读取历史失败：{e:#}"));
        }
    }

    // -- user actions ----------------------------------------------------

    fn focused_live(&mut self) -> Option<SessionId> {
        let sid = self.focused?;
        if !self.connected {
            self.error("未连接 berthd，输入没有发送");
            return None;
        }
        if !self.sessions.get(&sid).is_some_and(SessionMeta::is_live) {
            self.info("这个 session 已结束；点侧栏的 Revive 重新启动");
            return None;
        }
        Some(sid)
    }

    /// Keyboard / IME bytes for the focused session.
    pub fn input(&mut self, out: &mut dyn Outbound, bytes: Vec<u8>, now: Instant) {
        if bytes.is_empty() {
            return;
        }
        let Some(sid) = self.focused_live() else {
            return;
        };
        if let Some(view) = self.view.as_mut() {
            view.scroll_to_bottom();
            view.clear_selection();
        }
        match self.paste.as_mut() {
            // Keys typed during a paste go after it.
            Some((psid, job)) if *psid == sid => {
                job.push(&bytes);
                self.pump_paste(out, now);
            }
            _ => {
                self.send(
                    out,
                    Request::Input {
                        session: sid,
                        data: bytes,
                    },
                    None,
                );
            }
        }
    }

    /// Bytes the terminal protocol generates (mouse reports, focus in/out):
    /// sent like keys but without scrolling or touching the selection, and
    /// silently dropped when the session cannot take input.
    pub fn report(&mut self, out: &mut dyn Outbound, bytes: Vec<u8>, now: Instant) {
        let Some(sid) = self.focused else {
            return;
        };
        if bytes.is_empty()
            || !self.connected
            || !self.sessions.get(&sid).is_some_and(SessionMeta::is_live)
        {
            return;
        }
        match self.paste.as_mut() {
            Some((psid, job)) if *psid == sid => {
                job.push(&bytes);
                self.pump_paste(out, now);
            }
            _ => {
                self.send(
                    out,
                    Request::Input {
                        session: sid,
                        data: bytes,
                    },
                    None,
                );
            }
        }
    }

    /// ⌘V.
    pub fn paste(&mut self, out: &mut dyn Outbound, text: &str, now: Instant) {
        let Some(sid) = self.focused_live() else {
            return;
        };
        let modes = self
            .view
            .as_ref()
            .map(SessionView::modes)
            .unwrap_or_default();
        let bytes = paste::encode(text, modes);
        if bytes.is_empty() {
            return;
        }
        if let Some(view) = self.view.as_mut() {
            view.scroll_to_bottom();
            view.clear_selection();
        }
        match self.paste.as_mut() {
            Some((psid, job)) if *psid == sid => job.push(&bytes),
            _ => {
                if let Some((_, old)) = self.paste.take() {
                    if !old.is_done() {
                        self.error(format!(
                            "上一个 session 的粘贴已取消，{} 字节未发送",
                            old.queued_bytes()
                        ));
                    }
                }
                self.paste = Some((sid, PasteJob::new(&bytes)));
            }
        }
        self.pump_paste(out, now);
    }

    fn pump_paste(&mut self, out: &mut dyn Outbound, now: Instant) {
        let Some((sid, job)) = self.paste.as_mut() else {
            return;
        };
        let sid = *sid;
        let mut failed = None;
        if let Some(chunk) = job.next_chunk(now) {
            let data = chunk.to_vec();
            match out
                .send(Request::Input { session: sid, data })
                .and_then(|input| Ok((input, out.send(Request::DaemonStatus)?)))
            {
                Ok((input, barrier)) => job.sent(input, barrier),
                Err(e) => failed = Some(e),
            }
        }
        let done = job.is_done();
        if let Some(e) = failed {
            let left = job.queued_bytes();
            self.paste = None;
            self.error(format!("粘贴失败：{e:#}（{left} 字节未发送）"));
        } else if done {
            self.paste = None;
        }
    }

    /// ⌘N: a session in the focused session's workspace (else the first;
    /// with no workspace at all, one rooted at $HOME is created first).
    pub fn new_session(&mut self, out: &mut dyn Outbound) {
        let ws = self
            .focused_meta()
            .map(|m| m.workspace)
            .filter(|w| self.workspace(*w).is_some())
            .or_else(|| self.workspaces.first().map(|w| w.id));
        match ws {
            Some(ws) => self.create_session(out, ws),
            None => {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/"));
                self.create_workspace(out, home);
            }
        }
    }

    /// The "+" of a workspace header.
    pub fn new_session_in(&mut self, out: &mut dyn Outbound, ws: WorkspaceId) {
        self.create_session(out, ws);
    }

    /// ⌘⇧N after the folder was chosen: reuse a workspace with that root,
    /// else create one; then a session in it.
    pub fn new_workspace(&mut self, out: &mut dyn Outbound, root: PathBuf) {
        match self.workspaces.iter().find(|w| w.root == root) {
            Some(ws) => {
                let id = ws.id;
                self.create_session(out, id);
            }
            None => self.create_workspace(out, root),
        }
    }

    fn create_workspace(&mut self, out: &mut dyn Outbound, root: PathBuf) {
        let name = workspace_name(&root);
        self.send(
            out,
            Request::CreateWorkspace { name, root },
            Some(Awaiting::CreateWorkspace { then_session: true }),
        );
    }

    fn create_session(&mut self, out: &mut dyn Outbound, workspace: WorkspaceId) {
        let req = Request::CreateSession {
            workspace,
            cwd: None,
            command: self.new_session_command.clone(),
            title: None,
            dims: self.grid_dims(),
        };
        self.send(out, req, Some(Awaiting::CreateSession));
    }

    /// ⌘1..9 (1-based).
    pub fn jump(&mut self, out: &mut dyn Outbound, n: usize, now: Instant) {
        if let Some(sid) = n
            .checked_sub(1)
            .and_then(|i| self.jump_order().get(i).copied())
        {
            self.focus(out, sid, now);
        }
    }

    /// ⌘W: kill a live session (asking first when an agent or a command
    /// runs); a dormant one is deleted after confirmation.
    pub fn request_close(&mut self, out: &mut dyn Outbound) {
        let Some(meta) = self.focused_meta() else {
            return;
        };
        let (sid, title) = (meta.id, meta.title().to_string());
        if meta.is_live() {
            let agent = &meta.agent;
            if agent.kind.is_agent() || agent.state.is_busy() {
                let what = if agent.kind.is_agent() {
                    format!("{} 正在这个 session 里运行", notify_name(&agent.kind))
                } else {
                    "这个 session 里有命令正在运行".to_string()
                };
                self.confirm = Some(Confirm::Kill {
                    session: sid,
                    title,
                    what,
                });
            } else {
                self.kill(out, sid);
            }
        } else {
            self.confirm = Some(Confirm::Delete {
                session: sid,
                title,
            });
        }
    }

    pub fn answer_confirm(&mut self, out: &mut dyn Outbound, yes: bool) {
        let Some(c) = self.confirm.take() else {
            return;
        };
        if !yes {
            return;
        }
        match c {
            Confirm::Kill { session, .. } => self.kill(out, session),
            Confirm::Delete { session, .. } => {
                self.send(
                    out,
                    Request::Delete { session },
                    Some(Awaiting::Command("删除 session ")),
                );
            }
        }
    }

    fn kill(&mut self, out: &mut dyn Outbound, session: SessionId) {
        self.send(
            out,
            Request::Kill { session },
            Some(Awaiting::Command("关闭 session ")),
        );
    }

    /// Revive button: only the protocol message; the daemon builds the argv
    /// (and validates the agent's external id for `ResumeAgent`).
    pub fn revive(
        &mut self,
        out: &mut dyn Outbound,
        sid: SessionId,
        mode: ReviveMode,
        now: Instant,
    ) {
        if self.focused != Some(sid) {
            self.focus(out, sid, now);
        }
        self.send(
            out,
            Request::Revive { session: sid, mode },
            Some(Awaiting::Command("Revive ")),
        );
    }
}

fn notify_name(kind: &berth_core::AgentKind) -> String {
    match kind {
        berth_core::AgentKind::Shell => "shell".into(),
        berth_core::AgentKind::Claude => "claude".into(),
        berth_core::AgentKind::Codex => "codex".into(),
        berth_core::AgentKind::Other(n) => n.clone(),
    }
}

/// Workspace name for a root directory: its last component ("~" for $HOME).
pub fn workspace_name(root: &Path) -> String {
    if std::env::var_os("HOME").is_some_and(|h| Path::new(&h) == root) {
        return "~".into();
    }
    root.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| root.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_core::{
        AgentInfo, AgentKind, AgentState, CursorState, SessionStatus, StyleId, TermModes,
    };

    #[derive(Default)]
    struct Fake {
        sent: Vec<(u32, Request)>,
        next: u32,
    }

    impl Outbound for Fake {
        fn send(&mut self, req: Request) -> Result<u32> {
            self.next += 1;
            self.sent.push((self.next, req));
            Ok(self.next)
        }
    }

    impl Fake {
        fn take(&mut self) -> Vec<(u32, Request)> {
            std::mem::take(&mut self.sent)
        }
    }

    fn ws(order: u32) -> Workspace {
        Workspace {
            id: WorkspaceId::new(),
            name: format!("ws{order}"),
            root: PathBuf::from(format!("/tmp/ws{order}")),
            color: None,
            order,
            created_at_ms: 0,
        }
    }

    fn session(ws: &Workspace, order: u32, live: bool) -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            workspace: ws.id,
            title_auto: format!("s{order}"),
            status: if live {
                SessionStatus::Live
            } else {
                SessionStatus::Restored
            },
            order,
            ..Default::default()
        }
    }

    fn reply(id: u32, event: Event) -> DaemonMsg {
        DaemonMsg {
            reply_to: Some(id),
            event,
        }
    }

    fn push(event: Event) -> DaemonMsg {
        DaemonMsg {
            reply_to: None,
            event,
        }
    }

    fn screen(sid: SessionId, seq: u64, text: &str) -> ScreenUpdate {
        let mut line = LineSnapshot::blank();
        line.push_str(text, StyleId::DEFAULT);
        ScreenUpdate {
            session: sid,
            seq,
            dims: Dims { cols: 20, rows: 2 },
            full: true,
            lines: vec![(0, line)],
            cursor: CursorState::default(),
            modes: TermModes::SHOW_CURSOR,
            display_offset: 0,
            history_len: 0,
            styles: vec![],
        }
    }

    /// Connected controller with `sessions` listed; returns the requests
    /// sent after the listing.
    fn listed(
        c: &mut Controller,
        out: &mut Fake,
        wss: Vec<Workspace>,
        sessions: Vec<SessionMeta>,
    ) -> Vec<(u32, Request)> {
        c.auto_session = false;
        c.set_grid(
            Dims {
                cols: 100,
                rows: 30,
            },
            Instant::now(),
        );
        c.on_connected(out);
        let sent = out.take();
        assert!(matches!(sent[0].1, Request::ListWorkspaces));
        assert!(matches!(sent[1].1, Request::ListSessions));
        let now = Instant::now();
        c.handle(out, reply(sent[0].0, Event::Workspaces(wss)), now);
        c.handle(out, reply(sent[1].0, Event::Sessions(sessions)), now);
        out.take()
    }

    fn attach_id(sent: &[(u32, Request)], sid: SessionId) -> u32 {
        sent.iter()
            .find(|(_, r)| matches!(r, Request::Attach { session, .. } if *session == sid))
            .map(|(id, _)| *id)
            .expect("Attach sent")
    }

    fn claude_dormant(ws: &Workspace, order: u32, id: &str) -> SessionMeta {
        let mut m = session(ws, order, false);
        m.agent.kind = AgentKind::Claude;
        m.agent.external_id = Some(id.into());
        m.cwd = PathBuf::from("/tmp/proj");
        m
    }

    fn event_entry(at_ms: i64, kind: &str) -> EventEntry {
        EventEntry {
            at_ms,
            kind: kind.into(),
            state: "thinking".into(),
            detail: None,
        }
    }

    fn requests<'a>(
        sent: &'a [(u32, Request)],
        pick: impl Fn(&Request) -> bool + 'a,
    ) -> Vec<(u32, &'a Request)> {
        sent.iter()
            .filter(|(_, r)| pick(r))
            .map(|(id, r)| (*id, r))
            .collect()
    }

    #[test]
    fn hover_fetches_events_once_until_the_agent_changes() {
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        let w = ws(0);
        let s = session(&w, 0, true);
        let sid = s.id;
        listed(&mut c, &mut out, vec![w], vec![s.clone()]);
        let now = Instant::now();
        c.hover(&mut out, sid);
        let sent = out.take();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(
            sent[0].1,
            Request::ListEvents {
                session: sid,
                limit: HOVER_EVENTS
            }
        );
        let first = sent[0].0;
        c.hover(&mut out, sid);
        assert!(out.take().is_empty(), "in flight: no second request");
        let events = |ids: &[&str]| Event::Events {
            session: sid,
            events: ids.iter().map(|k| event_entry(1, k)).collect(),
        };
        // An answer to some other request is not taken.
        c.handle(&mut out, reply(first + 100, events(&["hook:Other"])), now);
        assert!(c.recent_events(sid).unwrap().events.is_none());
        c.handle(&mut out, reply(first, events(&["hook:Stop"])), now);
        let got = c.recent_events(sid).unwrap();
        assert_eq!(got.events.as_ref().unwrap()[0].kind, "hook:Stop");
        c.hover(&mut out, sid);
        assert!(out.take().is_empty(), "current: no request");
        // The agent changed: the next hover asks again.
        let mut agent = s.agent.clone();
        agent.state = AgentState::WaitingInput;
        c.handle(
            &mut out,
            push(Event::AgentChanged {
                session: sid,
                agent,
            }),
            now,
        );
        c.hover(&mut out, sid);
        let sent = out.take();
        assert_eq!(
            requests(&sent, |r| matches!(r, Request::ListEvents { .. })).len(),
            1
        );
        // A failure is kept for the details, not raised as a notice.
        c.handle(
            &mut out,
            DaemonMsg {
                reply_to: Some(sent[0].0),
                event: Event::Error {
                    message: "cannot read events: disk".into(),
                },
            },
            now,
        );
        assert_eq!(
            c.recent_events(sid).unwrap().error.as_deref(),
            Some("cannot read events: disk")
        );
        assert!(c.notices().is_empty(), "{:?}", c.notices());
        // Unknown sessions are not asked about.
        c.hover(&mut out, SessionId::new());
        assert!(out.take().is_empty());
    }

    #[test]
    fn dormant_cards_ask_for_their_resume_command() {
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        let w = ws(0);
        let d = claude_dormant(&w, 0, "abc");
        let shell = session(&w, 1, false);
        let (did, shell_id) = (d.id, shell.id);
        listed(&mut c, &mut out, vec![w], vec![d.clone(), shell]);
        let now = Instant::now();
        c.set_visible(&mut out, &[did, shell_id], now);
        let sent = out.take();
        let asked = requests(&sent, |r| matches!(r, Request::ResumeCommand { .. }));
        assert_eq!(asked.len(), 1, "{sent:?}");
        assert_eq!(*asked[0].1, Request::ResumeCommand { session: did });
        let id = asked[0].0;
        c.set_visible(&mut out, &[did, shell_id], now);
        assert!(requests(&out.take(), |r| matches!(r, Request::ResumeCommand { .. })).is_empty());
        let argv = vec!["claude".to_string(), "--resume".into(), "abc".into()];
        c.handle(
            &mut out,
            reply(
                id,
                Event::ResumeCommand {
                    session: did,
                    cwd: PathBuf::from("/tmp/proj"),
                    command: Ok(argv.clone()),
                },
            ),
            now,
        );
        assert_eq!(
            c.resume_preview(did),
            Some(&ResumePreview {
                cwd: PathBuf::from("/tmp/proj"),
                command: Ok(argv),
            })
        );
        assert!(c.resume_preview(shell_id).is_none());
        // Another agent id: the old command no longer applies; asked again.
        let mut changed = d.clone();
        changed.agent.external_id = Some("def".into());
        c.handle(&mut out, push(Event::SessionUpdated(changed)), now);
        assert!(c.resume_preview(did).is_none());
        c.set_visible(&mut out, &[did], now);
        let sent = out.take();
        let asked = requests(&sent, |r| matches!(r, Request::ResumeCommand { .. }));
        assert_eq!(asked.len(), 1, "{sent:?}");
        // berthd refuses: the reason is shown instead of a command.
        c.handle(
            &mut out,
            reply(
                asked[0].0,
                Event::ResumeCommand {
                    session: did,
                    cwd: PathBuf::from("/tmp/proj"),
                    command: Err("no resume_command for claude".into()),
                },
            ),
            now,
        );
        assert!(c.resume_preview(did).unwrap().command.is_err());
    }

    #[test]
    fn attention_count_is_live_sessions_needing_attention() {
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        let w = ws(0);
        let mut waiting = session(&w, 0, true);
        waiting.agent.state = AgentState::WaitingPermission { tool: None };
        let mut done = session(&w, 1, true);
        done.agent.state = AgentState::Done;
        let mut busy = session(&w, 2, true);
        busy.agent.state = AgentState::Thinking;
        let mut dormant = session(&w, 3, false);
        dormant.agent.state = AgentState::Done;
        listed(
            &mut c,
            &mut out,
            vec![w],
            vec![waiting, done, busy, dormant],
        );
        assert_eq!(c.attention_count(), 2);
    }

    #[test]
    fn counters_report_applied_screens_and_preview_traffic() {
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        let w = ws(0);
        let a = session(&w, 0, true);
        let b = session(&w, 1, true);
        let sent = listed(&mut c, &mut out, vec![w], vec![a.clone(), b.clone()]);
        let now = Instant::now();
        c.handle(
            &mut out,
            reply(attach_id(&sent, a.id), Event::Screen(screen(a.id, 5, "x"))),
            now,
        );
        c.handle(&mut out, push(Event::Screen(screen(a.id, 6, "y"))), now);
        // Out of order: dropped, not counted.
        c.handle(&mut out, push(Event::Screen(screen(a.id, 4, "z"))), now);
        for _ in 0..3 {
            c.handle(
                &mut out,
                push(Event::Preview {
                    session: b.id,
                    lines: vec![],
                    styles: vec![],
                }),
                now,
            );
        }
        let got = c.take_counters();
        assert_eq!((got.screens, got.previews), (2, 3));
        assert_eq!(got.preview_sessions, HashSet::from([b.id]));
        assert_eq!(c.take_counters(), Counters::default(), "reset after taking");
    }

    #[test]
    fn listing_focuses_the_first_session_in_sidebar_order() {
        let (w0, w1) = (ws(0), ws(1));
        let a = session(&w1, 0, true);
        let b = session(&w0, 1, true);
        let d = session(&w0, 0, false);
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        let sent = listed(
            &mut c,
            &mut out,
            vec![w1.clone(), w0.clone()],
            vec![a.clone(), b.clone(), d.clone()],
        );
        assert_eq!(c.jump_order(), vec![b.id, a.id, d.id]);
        assert_eq!(c.focused(), Some(b.id));
        assert!(matches!(
            sent[0].1,
            Request::Attach { session, dims: Dims { cols: 100, rows: 30 } } if session == b.id
        ));
    }

    #[test]
    fn the_attach_answer_is_the_baseline_and_older_screens_are_ignored() {
        let w = ws(0);
        let s = session(&w, 0, true);
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        let sent = listed(&mut c, &mut out, vec![w], vec![s.clone()]);
        let id = attach_id(&sent, s.id);
        let now = Instant::now();
        // A screen from before the attach answer is ignored.
        c.handle(
            &mut out,
            push(Event::Screen(screen(s.id, 50, "stale"))),
            now,
        );
        assert!(!c.view().unwrap().has_screen());
        c.handle(
            &mut out,
            reply(id, Event::Screen(screen(s.id, 7, "fresh"))),
            now,
        );
        c.handle(&mut out, push(Event::Screen(screen(s.id, 6, "older"))), now);
        let view = c.view_mut().unwrap();
        assert_eq!(view.screen().lines[0].text(), "fresh");
        c.handle(&mut out, push(Event::Screen(screen(s.id, 8, "next"))), now);
        assert_eq!(c.view_mut().unwrap().screen().lines[0].text(), "next");
    }

    #[test]
    fn previews_follow_visibility_and_the_focused_session_is_never_unsubscribed() {
        let w = ws(0);
        let (a, b, x) = (
            session(&w, 0, true),
            session(&w, 1, true),
            session(&w, 2, false),
        );
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        listed(
            &mut c,
            &mut out,
            vec![w],
            vec![a.clone(), b.clone(), x.clone()],
        );
        assert_eq!(c.focused(), Some(a.id));
        let t0 = Instant::now();
        c.set_visible(&mut out, &[a.id, b.id, x.id], t0);
        let sent = out.take();
        let subscribed: Vec<SessionId> = sent
            .iter()
            .filter_map(|(_, r)| match r {
                Request::Subscribe { session, mode } => {
                    assert_eq!(*mode, PREVIEW);
                    Some(*session)
                }
                _ => None,
            })
            .collect();
        assert_eq!(subscribed, vec![b.id, x.id], "not the focused one");
        // b scrolls out of view: dropped only after the grace period.
        c.set_visible(&mut out, &[a.id, x.id], t0 + Duration::from_millis(500));
        assert!(out.take().is_empty());
        c.set_visible(
            &mut out,
            &[a.id, x.id],
            t0 + UNSUBSCRIBE_AFTER + Duration::from_millis(1),
        );
        assert!(
            matches!(out.take().as_slice(), [(_, Request::Unsubscribe { session })] if *session == b.id)
        );
        // Focusing x (which has a preview): Detach a, Unsubscribe x, then Attach x.
        c.focus(&mut out, x.id, t0);
        let kinds: Vec<String> = out
            .take()
            .iter()
            .map(|(_, r)| {
                format!("{r:?}")
                    .split([' ', '{'])
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(kinds, ["Detach", "Unsubscribe", "Attach"]);
        // a is now a plain card and gets its preview back.
        c.set_visible(&mut out, &[a.id, x.id], t0 + Duration::from_secs(5));
        assert!(
            matches!(out.take().as_slice(), [(_, Request::Subscribe { session, .. })] if *session == a.id)
        );
    }

    #[test]
    fn resize_is_sent_once_after_the_debounce() {
        let w = ws(0);
        let s = session(&w, 0, true);
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        listed(&mut c, &mut out, vec![w], vec![s.clone()]);
        let t0 = Instant::now();
        c.set_grid(Dims { cols: 90, rows: 30 }, t0);
        c.set_grid(Dims { cols: 80, rows: 25 }, t0 + Duration::from_millis(20));
        c.tick(&mut out, t0 + Duration::from_millis(40));
        assert!(out.take().is_empty(), "still debouncing");
        c.tick(&mut out, t0 + Duration::from_millis(71));
        assert!(matches!(
            out.take().as_slice(),
            [(_, Request::Resize { session, dims: Dims { cols: 80, rows: 25 } })] if *session == s.id
        ));
        c.tick(&mut out, t0 + Duration::from_millis(200));
        assert!(out.take().is_empty());
    }

    #[test]
    fn history_fetch_timers_wake_the_loop_only_while_connected() {
        let w = ws(0);
        let s = session(&w, 0, true);
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        let sent = listed(&mut c, &mut out, vec![w], vec![s.clone()]);
        let id = attach_id(&sent, s.id);
        let t0 = Instant::now() + Duration::from_secs(1);
        let mut u = screen(s.id, 1, "$ ");
        u.history_len = 5_000;
        c.handle(&mut out, reply(id, Event::Screen(u.clone())), t0);
        c.tick(&mut out, t0); // past the resize debounce of `listed`
        out.take();
        c.scroll(&mut out, 10, t0);
        assert!(matches!(
            out.take().as_slice(),
            [(
                _,
                Request::FetchLines {
                    start: 2_992,
                    count: 2_000,
                    ..
                }
            )]
        ));
        assert_eq!(c.next_deadline(), None, "nothing waits on a timer");
        // A full redraw at the scrollback limit: the visible rows are asked
        // for again at once, the rest once the history has stayed put.
        u.seq = 2;
        c.handle(&mut out, push(Event::Screen(u.clone())), t0);
        c.tick(&mut out, t0);
        assert!(matches!(
            out.take().as_slice(),
            [(
                _,
                Request::FetchLines {
                    start: 4_990,
                    count: 2,
                    ..
                }
            )]
        ));
        let settled = t0 + crate::session_view::REFRESH_INTERVAL;
        assert_eq!(c.next_deadline(), Some(settled));
        c.tick(&mut out, settled);
        assert!(matches!(
            out.take().as_slice(),
            [(
                _,
                Request::FetchLines {
                    start: 2_990,
                    count: 2_000,
                    ..
                }
            )]
        ));
        assert_eq!(c.next_deadline(), None);
        // Disconnected: no timer at all, even with a resize waiting.
        c.set_grid(Dims { cols: 50, rows: 20 }, t0);
        assert!(c.next_deadline().is_some());
        c.on_disconnected("test");
        assert_eq!(c.next_deadline(), None);
    }

    #[test]
    fn paste_waits_for_each_barrier_and_keys_typed_meanwhile_follow_it() {
        let w = ws(0);
        let s = session(&w, 0, true);
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        listed(&mut c, &mut out, vec![w], vec![s.clone()]);
        let now = Instant::now();
        let text = "x".repeat(paste::PASTE_CHUNK + 5);
        c.paste(&mut out, &text, now);
        let sent = out.take();
        assert!(
            matches!(&sent[0].1, Request::Input { data, .. } if data.len() == paste::PASTE_CHUNK)
        );
        assert!(matches!(sent[1].1, Request::DaemonStatus));
        c.input(&mut out, b"k".to_vec(), now);
        assert!(out.take().is_empty(), "queued behind the paste");
        // Refused: retried after the barrier and a backoff.
        c.handle(
            &mut out,
            reply(
                sent[0].0,
                Event::Error {
                    message: "backpressure: full".into(),
                },
            ),
            now,
        );
        let status = berth_core::DaemonStatus {
            version: String::new(),
            pid: 0,
            uptime_ms: 0,
            sessions_live: 0,
            sessions_total: 0,
        };
        c.handle(
            &mut out,
            reply(sent[1].0, Event::Status(status.clone())),
            now,
        );
        assert!(out.take().is_empty(), "backing off");
        assert!(
            c.notices().is_empty(),
            "backpressure during a paste is not an error"
        );
        assert!(c.next_deadline().is_some(), "retry timer");
        let later = now + Duration::from_millis(100);
        c.tick(&mut out, later);
        let sent = out.take();
        assert!(
            matches!(&sent[0].1, Request::Input { data, .. } if data.len() == paste::PASTE_CHUNK)
        );
        c.handle(
            &mut out,
            reply(sent[1].0, Event::Status(status.clone())),
            later,
        );
        let sent = out.take();
        assert!(matches!(&sent[0].1, Request::Input { data, .. } if data.as_slice() == b"xxxxx"));
        c.handle(
            &mut out,
            reply(sent[1].0, Event::Status(status.clone())),
            later,
        );
        let sent = out.take();
        assert!(matches!(&sent[0].1, Request::Input { data, .. } if data.as_slice() == b"k"));
        c.handle(&mut out, reply(sent[1].0, Event::Status(status)), later);
        assert!(!c.is_pasting());
        // Without a paste, keys go straight out.
        c.input(&mut out, b"z".to_vec(), later);
        assert!(
            matches!(out.take().as_slice(), [(_, Request::Input { data, .. })] if data.as_slice() == b"z")
        );
    }

    #[test]
    fn bracketed_paste_follows_the_session_modes() {
        let w = ws(0);
        let s = session(&w, 0, true);
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        let sent = listed(&mut c, &mut out, vec![w], vec![s.clone()]);
        let mut u = screen(s.id, 1, "$");
        u.modes |= TermModes::BRACKETED_PASTE;
        c.handle(
            &mut out,
            reply(attach_id(&sent, s.id), Event::Screen(u)),
            Instant::now(),
        );
        c.paste(&mut out, "a\nb", Instant::now());
        assert!(matches!(
            &out.take()[0].1,
            Request::Input { data, .. } if data.as_slice() == b"\x1b[200~a\nb\x1b[201~"
        ));
    }

    #[test]
    fn errors_are_shown_until_dismissed_and_repeats_are_counted() {
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        listed(&mut c, &mut out, vec![], vec![]);
        let now = Instant::now();
        for _ in 0..3 {
            c.handle(
                &mut out,
                push(Event::Error {
                    message: "boom".into(),
                }),
                now,
            );
        }
        assert_eq!(c.notices().len(), 1);
        assert_eq!(c.notices()[0].count, 3);
        assert!(c.notices()[0].text.contains("boom"));
        c.info("fyi");
        c.tick(&mut out, now + Duration::from_secs(60));
        assert_eq!(c.notices().len(), 1, "info expired, the error stays");
        c.dismiss_notice(0);
        assert!(c.notices().is_empty());
    }

    #[test]
    fn close_confirms_for_agents_and_dormant_sessions() {
        let w = ws(0);
        let mut agent = session(&w, 0, true);
        agent.agent = AgentInfo {
            kind: AgentKind::Claude,
            state: AgentState::Idle,
            ..Default::default()
        };
        let shell = session(&w, 1, true);
        let dormant = session(&w, 2, false);
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        listed(
            &mut c,
            &mut out,
            vec![w],
            vec![agent.clone(), shell.clone(), dormant.clone()],
        );
        let now = Instant::now();
        c.request_close(&mut out);
        assert!(matches!(c.confirm(), Some(Confirm::Kill { session, .. }) if *session == agent.id));
        c.answer_confirm(&mut out, false);
        assert!(out.take().is_empty() && c.confirm().is_none());
        c.request_close(&mut out);
        c.answer_confirm(&mut out, true);
        assert!(
            matches!(out.take().as_slice(), [(_, Request::Kill { session })] if *session == agent.id)
        );
        c.focus(&mut out, shell.id, now);
        out.take();
        c.request_close(&mut out);
        assert!(
            matches!(out.take().as_slice(), [(_, Request::Kill { session })] if *session == shell.id)
        );
        c.focus(&mut out, dormant.id, now);
        out.take();
        c.request_close(&mut out);
        assert!(matches!(c.confirm(), Some(Confirm::Delete { .. })));
        c.answer_confirm(&mut out, true);
        assert!(
            matches!(out.take().as_slice(), [(_, Request::Delete { session })] if *session == dormant.id)
        );
    }

    #[test]
    fn attention_notifies_unless_the_session_is_being_looked_at() {
        let w = ws(0);
        let (a, b) = (session(&w, 0, true), session(&w, 1, true));
        let mut c = Controller::new(notify::DEFAULT_ON.iter().map(|s| s.to_string()).collect());
        let mut out = Fake::default();
        listed(&mut c, &mut out, vec![w], vec![a.clone(), b.clone()]);
        let now = Instant::now();
        let changed = |sid, state| {
            push(Event::AgentChanged {
                session: sid,
                agent: AgentInfo {
                    kind: AgentKind::Claude,
                    state,
                    ..Default::default()
                },
            })
        };
        // a is focused and the window has focus: no notification.
        assert!(c
            .handle(&mut out, changed(a.id, AgentState::Done), now)
            .is_empty());
        // b is not focused.
        let fx = c.handle(&mut out, changed(b.id, AgentState::WaitingInput), now);
        assert_eq!(
            fx,
            vec![Effect::Notify {
                title: "ws0/s1".into(),
                body: "claude 等待输入".into()
            }]
        );
        assert_eq!(c.session(b.id).unwrap().agent.kind, AgentKind::Claude);
        // The window loses focus: the focused session notifies too.
        c.set_window_focused(&mut out, false);
        c.handle(&mut out, changed(a.id, AgentState::Thinking), now);
        assert_eq!(
            c.handle(&mut out, changed(a.id, AgentState::WaitingInput), now)
                .len(),
            1
        );
        // kind back to Shell = the agent left.
        c.handle(
            &mut out,
            push(Event::AgentChanged {
                session: b.id,
                agent: AgentInfo::default(),
            }),
            now,
        );
        assert_eq!(c.session(b.id).unwrap().agent.kind, AgentKind::Shell);
    }

    #[test]
    fn revive_sends_only_the_protocol_message_and_reattaches_when_live() {
        let w = ws(0);
        let d = session(&w, 0, false);
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        let sent = listed(&mut c, &mut out, vec![w], vec![d.clone()]);
        let now = Instant::now();
        c.handle(
            &mut out,
            reply(
                attach_id(&sent, d.id),
                Event::Screen(screen(d.id, 3, "old")),
            ),
            now,
        );
        c.input(&mut out, b"x".to_vec(), now);
        assert!(out.take().is_empty(), "no input to a dormant session");
        c.revive(&mut out, d.id, ReviveMode::Shell, now);
        let sent = out.take();
        assert!(matches!(
            sent.as_slice(),
            [(_, Request::Revive { session, mode: ReviveMode::Shell })] if *session == d.id
        ));
        let mut live = d.clone();
        live.status = SessionStatus::Live;
        c.handle(&mut out, reply(sent[0].0, Event::SessionUpdated(live)), now);
        let sent = out.take();
        let id = attach_id(&sent, d.id);
        c.handle(
            &mut out,
            reply(id, Event::Screen(screen(d.id, 1, "new shell"))),
            now,
        );
        assert_eq!(c.view_mut().unwrap().screen().lines[0].text(), "new shell");
    }

    #[test]
    fn removal_of_the_focused_session_moves_focus_and_reconnect_reattaches() {
        let w = ws(0);
        let (a, b) = (session(&w, 0, true), session(&w, 1, true));
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        listed(
            &mut c,
            &mut out,
            vec![w.clone()],
            vec![a.clone(), b.clone()],
        );
        let now = Instant::now();
        c.handle(&mut out, push(Event::SessionRemoved(a.id)), now);
        assert_eq!(c.focused(), Some(b.id));
        assert!(
            matches!(out.take().as_slice(), [(_, Request::Attach { session, .. })] if *session == b.id)
        );
        c.on_disconnected("gone");
        assert!(c.notices().iter().any(|n| n.text.contains("gone")));
        let sent = listed(&mut c, &mut out, vec![w], vec![b.clone()]);
        attach_id(&sent, b.id);
    }

    #[test]
    fn new_session_creates_a_home_workspace_first_when_there_is_none() {
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        listed(&mut c, &mut out, vec![], vec![]);
        c.new_session(&mut out);
        let sent = out.take();
        let [(id, Request::CreateWorkspace { .. })] = sent.as_slice() else {
            panic!("{sent:?}")
        };
        let w = ws(0);
        c.handle(
            &mut out,
            reply(*id, Event::WorkspaceUpdated(w.clone())),
            Instant::now(),
        );
        assert!(matches!(
            out.take().as_slice(),
            [(_, Request::CreateSession { workspace, command: None, .. })] if *workspace == w.id
        ));
    }

    /// Against the real daemon: create, type, page through history, paste
    /// through the chunked pipeline, kill and revive with history kept.
    #[test]
    fn real_daemon_session_lifecycle() {
        use crate::client::ClientEvent;
        use crate::testutil::TestDaemon;
        use std::os::unix::net::UnixStream;

        let daemon = TestDaemon::start();
        let root = tempfile::tempdir().unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let stream = UnixStream::connect(&daemon.paths.socket).unwrap();
        let mut client = Client::start(stream, berth_core::ClientRole::Gui, move |ev| {
            let _ = tx.send(ev);
        })
        .unwrap();
        let mut c = Controller::new(vec![]);
        c.auto_session = false;
        c.new_session_command = Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'ready\\n'; exec cat".into(),
        ]);
        c.set_grid(Dims { cols: 60, rows: 10 }, Instant::now());
        c.on_connected(&mut client);
        let pump = |c: &mut Controller,
                    client: &mut Client,
                    until: &dyn Fn(&mut Controller) -> bool,
                    what: &str| {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if until(c) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {what}; notices: {:?}",
                    c.notices()
                );
                match rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(ClientEvent::Msg(m)) => {
                        c.handle(client, *m, Instant::now());
                    }
                    Ok(ClientEvent::Closed(r)) => panic!("connection closed: {r}"),
                    Err(_) => {}
                }
                c.tick(client, Instant::now());
            }
        };
        let screen_has = |c: &mut Controller, text: &str| {
            c.view_mut().is_some_and(|v| {
                v.has_screen() && v.screen().lines.iter().any(|l| l.text().contains(text))
            })
        };
        pump(&mut c, &mut client, &|c| c.is_loaded(), "listing");
        c.new_workspace(&mut client, root.path().to_path_buf());
        pump(
            &mut c,
            &mut client,
            &|c| screen_has(c, "ready"),
            "the first screen",
        );
        let sid = c.focused().unwrap();

        // 40 numbered lines through the PTY: 30 of them scroll into history.
        let lines: String = (0..40).map(|i| format!("line{i:02}\n")).collect();
        c.input(&mut client, lines.into_bytes(), Instant::now());
        pump(
            &mut c,
            &mut client,
            &|c| screen_has(c, "line39"),
            "echoed input",
        );
        let top = c.view().unwrap().history_len();
        assert!(top > 20, "history_len {top}");
        c.scroll(&mut client, 1000, Instant::now());
        pump(
            &mut c,
            &mut client,
            &|c| screen_has(c, "ready"),
            "history fetched from the top",
        );

        // A paste bigger than one chunk goes through the barrier pipeline.
        let big: String = (0..20_000).map(|i| format!("p{i:05}\n")).collect();
        assert!(big.len() > 2 * paste::PASTE_CHUNK);
        c.paste(&mut client, &big, Instant::now());
        pump(
            &mut c,
            &mut client,
            &|c| !c.is_pasting(),
            "the paste to drain",
        );
        c.view_mut().unwrap().scroll_to_bottom();
        pump(
            &mut c,
            &mut client,
            &|c| screen_has(c, "p19999"),
            "the end of the paste",
        );
        assert!(c.notices().is_empty(), "{:?}", c.notices());

        // Kill (a plain command: no confirmation), then revive the same argv.
        c.request_close(&mut client);
        if c.confirm().is_some() {
            c.answer_confirm(&mut client, true);
        }
        pump(
            &mut c,
            &mut client,
            &|c| c.session(sid).is_some_and(|m| !m.is_live()),
            "the session to go dormant",
        );
        c.revive(&mut client, sid, ReviveMode::Shell, Instant::now());
        pump(
            &mut c,
            &mut client,
            &|c| c.session(sid).is_some_and(SessionMeta::is_live) && screen_has(c, "ready"),
            "the revived session",
        );
        // The old output is still above the new shell.
        let hist = c.view().unwrap().history_len();
        assert!(hist > top, "history {hist} kept across revive");
        assert!(c.notices().is_empty(), "{:?}", c.notices());
    }

    /// Lines cached while scrolled up, then shifted by output at the
    /// scrollback limit, are fetched again: what the view shows matches the
    /// daemon's own answer for the same indices.
    #[test]
    fn real_daemon_history_shifted_at_the_scrollback_limit_is_fetched_again() {
        use crate::client::ClientEvent;
        use crate::testutil::TestDaemon;
        use crossbeam_channel::Receiver;
        use std::os::unix::net::UnixStream;

        fn pump(
            c: &mut Controller,
            client: &mut Client,
            rx: &Receiver<ClientEvent>,
            until: &dyn Fn(&Controller) -> bool,
            what: &str,
        ) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !until(c) {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {what}; notices: {:?}",
                    c.notices()
                );
                match rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(ClientEvent::Msg(m)) => {
                        c.handle(client, *m, Instant::now());
                    }
                    Ok(ClientEvent::Closed(r)) => panic!("connection closed: {r}"),
                    Err(_) => {}
                }
                c.tick(client, Instant::now());
            }
        }

        /// The daemon's answer for `[start, start + count)`, asked directly.
        fn daemon_lines(
            c: &mut Controller,
            client: &mut Client,
            rx: &Receiver<ClientEvent>,
            session: SessionId,
            start: u64,
            count: u32,
        ) -> Vec<String> {
            let id = client
                .send(Request::FetchLines {
                    session,
                    start,
                    count,
                })
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                assert!(Instant::now() < deadline, "no answer to FetchLines");
                match rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(ClientEvent::Msg(m)) if m.reply_to == Some(id) => {
                        let Event::Lines { lines, .. } = m.event else {
                            panic!("FetchLines answered with another event");
                        };
                        return lines.iter().map(LineSnapshot::text).collect();
                    }
                    Ok(ClientEvent::Msg(m)) => {
                        c.handle(client, *m, Instant::now());
                    }
                    Ok(ClientEvent::Closed(r)) => panic!("connection closed: {r}"),
                    Err(_) => {}
                }
            }
        }

        fn on_screen(c: &Controller, text: &str) -> bool {
            c.view().is_some_and(|v| {
                v.has_screen()
                    && (0..u64::from(v.dims().rows)).any(|r| {
                        v.line(v.history_len() + r)
                            .is_some_and(|l| l.text().contains(text))
                    })
            })
        }

        fn shown(c: &Controller, start: u64, count: u64) -> Vec<String> {
            let v = c.view().unwrap();
            (start..start + count)
                .map(|n| v.line(n).map(LineSnapshot::text).unwrap_or_default())
                .collect()
        }

        let mut config = berth_daemon::Config::default();
        config.terminal.scrollback = 200;
        let daemon = TestDaemon::start_with(config);
        let root = tempfile::tempdir().unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let stream = UnixStream::connect(&daemon.paths.socket).unwrap();
        let mut client = Client::start(stream, berth_core::ClientRole::Gui, move |ev| {
            let _ = tx.send(ev);
        })
        .unwrap();
        let mut c = Controller::new(vec![]);
        c.auto_session = false;
        // For each "A B" it reads: the numbers A..=B, "done B", then "end B"
        // without a newline, so once "end B" shows no more output (and no
        // further scroll) is coming.
        c.new_session_command = Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "stty -echo; printf ready; \
             while read a b; do echo; seq $a $b; echo \"done $b\"; printf 'end %s' $b; done"
                .into(),
        ]);
        c.set_grid(Dims { cols: 60, rows: 10 }, Instant::now());
        c.on_connected(&mut client);
        pump(&mut c, &mut client, &rx, &|c| c.is_loaded(), "listing");
        c.new_workspace(&mut client, root.path().to_path_buf());
        pump(
            &mut c,
            &mut client,
            &rx,
            &|c| on_screen(c, "ready"),
            "the first screen",
        );
        let sid = c.focused().unwrap();

        // Far more than the scrollback holds: history_len sits at the limit.
        c.input(&mut client, b"1 300\n".to_vec(), Instant::now());
        pump(
            &mut c,
            &mut client,
            &rx,
            &|c| on_screen(c, "end 300"),
            "the first batch",
        );
        assert_eq!(c.view().unwrap().history_len(), 200);
        // Scrolled up: lines from the top of the history get cached (the
        // prefetch page may follow the visible rows by REFRESH_INTERVAL).
        c.scroll(&mut client, 100, Instant::now());
        let cached = |c: &Controller| shown(c, 20, 10).iter().all(|l| !l.is_empty());
        pump(
            &mut c,
            &mut client,
            &rx,
            &|c| c.view().unwrap().visible_complete() && cached(c),
            "the scrolled-up view",
        );
        let before = shown(&c, 20, 10);
        assert_eq!(before, daemon_lines(&mut c, &mut client, &rx, sid, 20, 10));

        // More output at the limit while the view stays scrolled up (sent
        // directly: typing would snap the view to the bottom): every index
        // moves while history_len stays put.
        client
            .send(Request::Input {
                session: sid,
                data: b"301 350\n".to_vec(),
            })
            .unwrap();
        pump(
            &mut c,
            &mut client,
            &rx,
            &|c| on_screen(c, "end 350"),
            "the second batch",
        );
        assert_eq!(c.view().unwrap().display_offset(), 100, "still scrolled up");
        assert_eq!(c.view().unwrap().history_len(), 200);
        pump(
            &mut c,
            &mut client,
            &rx,
            &|c| c.view().unwrap().visible_complete() && c.next_deadline().is_none() && cached(c),
            "the history to settle",
        );
        // Back to the lines that were cached before the shift.
        c.scroll(&mut client, 80, Instant::now());
        pump(
            &mut c,
            &mut client,
            &rx,
            &|c| c.view().unwrap().visible_complete(),
            "the view on the old lines",
        );
        let truth = daemon_lines(&mut c, &mut client, &rx, sid, 20, 10);
        assert_ne!(truth, before, "the output did shift the history");
        assert_eq!(shown(&c, 20, 10), truth);
        assert!(c.notices().is_empty(), "{:?}", c.notices());
    }
}
