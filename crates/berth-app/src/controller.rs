//! The GUI's protocol state (integrate.md §1–§4), free of winit and wgpu so
//! it can be driven by tests: workspaces and sessions (listed once, then
//! kept current from `WorkspaceUpdated` / `SessionUpdated` /
//! `SessionRemoved` / `AgentChanged` / `Exited`), the split panes and one
//! [`SessionView`] per pane, sidebar preview subscriptions, history
//! prefetch, the paste pipeline, notification decisions and the list of
//! errors shown to the user.
//!
//! Panes (DESIGN §17.3): a [`PaneTree`] says which sessions the terminal
//! area shows; every pane has its own view, `Attach` and size (`Resize` is
//! per pane, from the shared [`Geometry`]). The focused pane is the
//! sidebar's "current" session and gets the keyboard. A session is shown
//! by at most one pane: showing one that has a pane focuses that pane;
//! otherwise it replaces the focused pane's session. The layout and the
//! focus are reported through [`Controller::take_layout_changed`] for
//! `gui-state.json`, and a saved layout is applied once the sessions are
//! listed (panes of sessions that are gone are pruned).
//!
//! Subscription rules that follow from the daemon's semantics:
//! - `Unsubscribe` drops *every* subscription of this connection on that
//!   session, the `Attach` included, so it is never sent for a session a
//!   pane shows; showing a session that has a preview sends `Unsubscribe`
//!   first and `Attach` after it (requests are handled in order).
//! - A pane that stops showing a session sends `Detach`; its card
//!   re-subscribes a preview if it is visible.
//! - The answer to `Attach` is the first full screen and the new `seq`
//!   baseline; other screens for the session are ignored until it arrives.
//!   One connection attaches to every shown session; the daemon keeps the
//!   attachments per session.
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
use crate::gui_state::GuiState;
use crate::notify::{self, Policy};
use crate::panes::{Direction, Geometry, PaneError, PaneTree, Rect, Removal, SplitDir, SplitPath};
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
type ResumeKey = (Option<AgentKind>, Option<String>, Option<PathBuf>, PathBuf);

fn resume_key(m: &SessionMeta) -> ResumeKey {
    (
        m.agent.resume_kind().cloned(),
        m.agent.external_id.clone(),
        m.agent.transcript_path.clone(),
        m.cwd.clone(),
    )
}

/// A dormant card offers "Resume" when its agent left an id to resume: the
/// agent still running when the session ended, or the one that left before
/// (`/exit`: the kind is `Shell` again).
pub fn resumable(m: &SessionMeta) -> bool {
    !m.is_live() && m.agent.resume_kind().is_some() && m.agent.external_id.is_some()
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
    /// ⌘W / 「归档」 on a live session running an agent or a command:
    /// archiving ends it.
    Archive {
        session: SessionId,
        title: String,
        what: String,
    },
    /// 「彻底删除…」 of an archived session: its history is deleted.
    Delete { session: SessionId, title: String },
    /// 「删除 workspace…」 on a workspace without sessions.
    DeleteWorkspace { id: WorkspaceId, name: String },
}

impl Confirm {
    fn session(&self) -> Option<SessionId> {
        match self {
            Confirm::Archive { session, .. } | Confirm::Delete { session, .. } => Some(*session),
            Confirm::DeleteWorkspace { .. } => None,
        }
    }
}

/// Traffic seen since the last [`Controller::take_counters`] (`--stats`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// `Screen` updates applied to the panes' views.
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
    /// `split`: open the new session in a pane beside this session's
    /// (⌘D / ⌘⇧D) instead of in the focused pane.
    CreateSession {
        split: Option<(SessionId, SplitDir)>,
    },
    Attach(SessionId),
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

/// A pane's view of its session and the state of its `Attach`.
struct Pane {
    view: SessionView,
    /// `Attach` was sent on this connection.
    attached: bool,
    /// The outstanding `Attach`: its answer is the baseline screen.
    attach_id: Option<u32>,
    /// The cells the pane fits.
    dims: Dims,
    /// The size last sent (`Attach` / `Resize`).
    sent_dims: Option<Dims>,
    resize_due: Option<Instant>,
}

impl Pane {
    fn new(view: SessionView, dims: Dims) -> Pane {
        Pane {
            view,
            attached: false,
            attach_id: None,
            dims,
            sent_dims: None,
            resize_due: None,
        }
    }

    /// The connection is new or gone: nothing is attached.
    fn detached(&mut self) {
        self.attached = false;
        self.attach_id = None;
        self.sent_dims = None;
        self.resize_due = None;
    }
}

/// The cells of a pane at `rect` (the fallback size when it is too small
/// for a terminal).
fn fit(g: &Geometry, rect: Rect) -> Dims {
    let d = g.cells(rect);
    if d.cols < 2 || d.rows < 1 {
        FALLBACK_DIMS
    } else {
        d
    }
}

pub struct Controller {
    connected: bool,
    workspaces: Vec<Workspace>,
    sessions: HashMap<SessionId, SessionMeta>,
    previews: HashMap<SessionId, Preview>,
    /// Which sessions the terminal area shows (`None`: none).
    layout: Option<PaneTree>,
    /// One per leaf of `layout`.
    panes: HashMap<SessionId, Pane>,
    /// The focused pane's session: the sidebar's "current" one, and where
    /// keys go.
    focused: Option<SessionId>,
    /// The terminal area in pixels (`None` until the window reports it).
    geometry: Option<Geometry>,
    /// The layout and focus last reported by [`Self::take_layout_changed`]
    /// (or restored).
    reported: (Option<PaneTree>, Option<SessionId>),
    /// The first listing placed the panes: the layout is worth saving.
    layout_ready: bool,
    /// `gui-state.json` as read at start; applied once sessions are listed.
    restore: Option<GuiState>,
    subs: HashMap<SessionId, Sub>,
    pending: HashMap<u32, Awaiting>,
    notices: Vec<Notice>,
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
    /// `--split-right` / `--split-down` after `--session`: a layout built
    /// once listed (`SID` or `SID@TARGET`).
    want_splits: Vec<(SplitDir, String)>,
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
            layout: None,
            panes: HashMap::new(),
            focused: None,
            geometry: None,
            reported: (None, None),
            layout_ready: false,
            restore: None,
            subs: HashMap::new(),
            pending: HashMap::new(),
            notices: Vec::new(),
            paste: None,
            policy: Policy::new(notify_on),
            window_focused: true,
            confirm: None,
            loaded_workspaces: false,
            loaded_sessions: false,
            mark_read_sent: HashSet::new(),
            want: None,
            want_splits: Vec::new(),
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

    /// The focused pane's view.
    pub fn view(&self) -> Option<&SessionView> {
        self.pane_view(self.focused?)
    }

    pub fn view_mut(&mut self) -> Option<&mut SessionView> {
        self.pane_view_mut(self.focused?)
    }

    /// The view of `sid`'s pane.
    pub fn pane_view(&self, sid: SessionId) -> Option<&SessionView> {
        self.panes.get(&sid).map(|p| &p.view)
    }

    pub fn pane_view_mut(&mut self, sid: SessionId) -> Option<&mut SessionView> {
        self.panes.get_mut(&sid).map(|p| &mut p.view)
    }

    /// Every pane's view, in no particular order.
    pub fn views_mut(&mut self) -> impl Iterator<Item = &mut SessionView> {
        self.panes.values_mut().map(|p| &mut p.view)
    }

    /// Which sessions the terminal area shows (`None`: none).
    pub fn layout(&self) -> Option<&PaneTree> {
        self.layout.as_ref()
    }

    /// Whether `sid` has a pane (it is never shown twice).
    pub fn is_shown(&self, sid: SessionId) -> bool {
        self.panes.contains_key(&sid)
    }

    pub fn pane_count(&self) -> usize {
        self.layout.as_ref().map_or(0, PaneTree::pane_count)
    }

    /// The cells `sid`'s pane fits.
    pub fn pane_dims(&self, sid: SessionId) -> Option<Dims> {
        self.panes.get(&sid).map(|p| p.dims)
    }

    /// The layout and the focus, for `gui-state.json`.
    pub fn gui_state(&self) -> GuiState {
        GuiState::new(self.layout.clone(), self.focused)
    }

    /// The first listing placed the panes: before that the layout is not
    /// known yet and must not overwrite a saved one.
    pub fn layout_ready(&self) -> bool {
        self.layout_ready
    }

    /// The layout or the focus changed since the previous call (or the
    /// restore): the state to write.
    pub fn take_layout_changed(&mut self) -> Option<GuiState> {
        if !self.layout_ready {
            return None;
        }
        let now = (self.layout.clone(), self.focused);
        if now == self.reported {
            return None;
        }
        self.reported = now;
        Some(self.gui_state())
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

    /// Live sessions whose agent state needs attention (Dock badge); never
    /// archived ones (DESIGN §17.1).
    pub fn attention_count(&self) -> usize {
        self.unarchived()
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

    /// The sessions of the workspace lists: all but the archived ones.
    fn unarchived(&self) -> impl Iterator<Item = &SessionMeta> {
        self.sessions.values().filter(|m| !m.is_archived())
    }

    /// Known and not archived: it may have a pane and a preview (berthd
    /// refuses to attach or subscribe to an archived session).
    fn showable(&self, sid: SessionId) -> bool {
        self.sessions.get(&sid).is_some_and(|m| !m.is_archived())
    }

    /// Live sessions of a workspace in display order.
    pub fn live_in(&self, ws: WorkspaceId) -> Vec<&SessionMeta> {
        let mut v: Vec<&SessionMeta> = self
            .unarchived()
            .filter(|m| m.workspace == ws && m.is_live())
            .collect();
        v.sort_by_key(|m| (m.order, m.created_at_ms, m.id));
        v
    }

    /// Live sessions whose workspace is unknown.
    pub fn live_orphans(&self) -> Vec<&SessionMeta> {
        let mut v: Vec<&SessionMeta> = self
            .unarchived()
            .filter(|m| m.is_live() && self.workspace(m.workspace).is_none())
            .collect();
        v.sort_by_key(|m| (m.created_at_ms, m.id));
        v
    }

    /// Dormant and restored sessions (history only), not archived.
    pub fn dormant(&self) -> Vec<&SessionMeta> {
        let ws_order = |id: WorkspaceId| self.workspace(id).map_or(u32::MAX, |w| w.order);
        let mut v: Vec<&SessionMeta> = self.unarchived().filter(|m| !m.is_live()).collect();
        v.sort_by_key(|m| (ws_order(m.workspace), m.order, m.created_at_ms, m.id));
        v
    }

    /// Archived sessions, the most recently archived first (the sidebar's
    /// 「归档」 section).
    pub fn archived(&self) -> Vec<&SessionMeta> {
        let mut v: Vec<&SessionMeta> = self.sessions.values().filter(|m| m.is_archived()).collect();
        v.sort_by_key(|m| (std::cmp::Reverse(m.archived_at_ms), m.created_at_ms, m.id));
        v
    }

    /// Sidebar order, which ⌘1..9 follow (archived sessions are not in it).
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
        let view = self.view()?;
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
        let panes = self.panes.values().flat_map(|p| {
            let history = p.view.next_deadline().filter(|_| p.attach_id.is_none());
            [p.resize_due, history]
        });
        panes.chain([paste]).flatten().min()
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

    /// Set before the first connection, after [`Self::want_session`]: once
    /// listed, split a pane and show this session in the new one. `spec` is
    /// `SID` (split the pane added last) or `SID@TARGET` (split TARGET's);
    /// ids or unique prefixes.
    pub fn want_split(&mut self, dir: SplitDir, spec: String) {
        self.want_splits.push((dir, spec));
    }

    /// Set before the first connection: the saved layout, applied (without
    /// the sessions that are gone or archived) once listed.
    pub fn restore(&mut self, state: GuiState) {
        self.reported = (state.layout.clone(), state.focused);
        self.restore = Some(state);
    }

    pub fn on_connected(&mut self, out: &mut dyn Outbound) {
        self.connected = true;
        self.pending.clear();
        self.subs.clear();
        for p in self.panes.values_mut() {
            p.detached();
        }
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
        for p in self.panes.values_mut() {
            p.detached();
        }
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
            Event::WorkspaceRemoved(id) => {
                self.workspaces.retain(|w| w.id != id);
                if matches!(self.confirm, Some(Confirm::DeleteWorkspace { id: c, .. }) if c == id) {
                    self.confirm = None;
                }
            }
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
                if let (Some(p), Some(id)) = (self.panes.get_mut(&session), reply_to) {
                    p.view.apply_lines(id, start, lines, &styles);
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
                    .program_notify(session, self.muted(session), now)
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
                let muted = self.muted(session);
                if self.policy.agent_changed(session, &agent.state, muted, now) {
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

    /// No desktop notification for `sid`: it is being looked at, or it is
    /// archived (DESIGN §17.1). The policy still follows its state.
    fn muted(&self, sid: SessionId) -> bool {
        self.attended(sid)
            || self
                .sessions
                .get(&sid)
                .is_some_and(SessionMeta::is_archived)
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
            Some(Awaiting::Attach(sid)) => {
                if let Some(p) = self.panes.get_mut(&sid) {
                    if p.attach_id == reply_to {
                        p.detached();
                    }
                }
                format!("无法打开 session：{message}")
            }
            Some(Awaiting::Subscribe) => format!("无法订阅侧栏预览：{message}"),
            Some(Awaiting::Fetch(sid)) => {
                if let (Some(p), Some(id)) = (self.panes.get_mut(&sid), reply_to) {
                    p.view.fetch_failed(id, now);
                }
                format!("读取历史失败：{message}")
            }
            Some(Awaiting::ListWorkspaces) | Some(Awaiting::ListSessions) => {
                format!("读取 session 列表失败：{message}")
            }
            Some(Awaiting::CreateWorkspace { .. }) => format!("新建 workspace 失败：{message}"),
            Some(Awaiting::CreateSession { .. }) => format!("新建 session 失败：{message}"),
            Some(Awaiting::Command(what)) => format!("{what}失败：{message}"),
            None if message.starts_with("backpressure:") => {
                format!("输入被拒绝（程序没有读取输入）：{message}")
            }
            None => format!("berthd：{message}"),
        };
        self.error(text);
        // A failed attach leaves its pane without a screen; keep the
        // placeholder rather than retrying in a loop (the next layout change
        // or click on the session attaches again).
        let _ = out;
    }

    fn after_listing(&mut self, out: &mut dyn Outbound, now: Instant) {
        if !self.is_loaded() {
            return;
        }
        // The saved layout (first listing only: a reconnect keeps the panes).
        if let Some(state) = self.restore.take() {
            if self.layout.is_none() {
                let sessions = &self.sessions;
                self.layout = state.layout.and_then(|t| {
                    t.prune(|sid| sessions.get(&sid).is_some_and(|m| !m.is_archived()))
                });
                self.focused = state.focused;
            }
        }
        // `--session`, with `--split-*` a whole layout.
        if let Some(want) = self.want.take() {
            match self.find_showable(&want) {
                Ok(sid) => {
                    let splits = std::mem::take(&mut self.want_splits);
                    if splits.is_empty() {
                        self.show(sid);
                    } else {
                        self.build_layout(sid, splits);
                    }
                }
                Err(e) => self.error(e),
            }
        }
        self.want_splits.clear();
        // Sessions that went away or were archived while disconnected.
        let sessions = &self.sessions;
        self.layout = self
            .layout
            .take()
            .and_then(|t| t.prune(|sid| sessions.get(&sid).is_some_and(|m| !m.is_archived())));
        // The first listing fills an empty terminal area; a later one (a
        // reconnect) leaves it as it was (the last pane may have been closed).
        if self.layout.is_none() && !self.layout_ready {
            if let Some(first) = self.jump_order().first().copied() {
                self.show(first);
            }
        }
        self.layout_ready = true;
        self.sync_panes(out, now);
        // First start: berthd has no session (archived ones aside).
        let none = self.sessions.values().all(SessionMeta::is_archived);
        if none && self.auto_session && !self.auto_created {
            self.auto_created = true;
            self.new_session(out);
        }
    }

    /// `--session FIRST --split-right B --split-down C@FIRST …`: FIRST is
    /// focused; a split without `@TARGET` splits the pane added last.
    fn build_layout(&mut self, first: SessionId, splits: Vec<(SplitDir, String)>) {
        let mut tree = PaneTree::Leaf(first);
        let mut last = first;
        for (dir, spec) in splits {
            let (want, target) = match spec.split_once('@') {
                Some((s, t)) => (s, Some(t)),
                None => (spec.as_str(), None),
            };
            let placed = self.find_showable(want).and_then(|sid| {
                let target = match target {
                    Some(t) => self.find_session(t)?,
                    None => last,
                };
                tree.split(target, sid, dir).map_err(|e| match e {
                    PaneError::AlreadyShown => format!("{want} 已在分屏中"),
                    PaneError::NotShown => format!("{} 不在分屏中", target.short()),
                })?;
                Ok(sid)
            });
            match placed {
                Ok(sid) => last = sid,
                Err(e) => self.error(format!("分屏参数 {spec}：{e}")),
            }
        }
        self.layout = Some(tree);
        self.focused = Some(first);
    }

    /// [`Self::find_session`] for a pane: not an archived session.
    fn find_showable(&self, want: &str) -> std::result::Result<SessionId, String> {
        let sid = self.find_session(want)?;
        if self.showable(sid) {
            Ok(sid)
        } else {
            Err(format!("{} 已归档，先在侧栏「归档」里恢复", sid.short()))
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
        let archived = meta.is_archived();
        self.sessions.insert(sid, meta);
        if let Some(Awaiting::CreateSession { split }) = awaiting {
            // Beside the pane it was split from; if that pane went away
            // meanwhile, beside the focused one.
            let beside = split.and_then(|(target, dir)| {
                let target = if self.is_shown(target) {
                    target
                } else {
                    self.focused?
                };
                Some((target, dir))
            });
            let placed = match (beside, self.layout.as_mut()) {
                (Some((target, dir)), Some(tree)) => tree.split(target, sid, dir).is_ok(),
                _ => false,
            };
            if placed {
                self.focused = Some(sid);
                self.sync_panes(out, now);
            } else {
                self.focus(out, sid, now);
            }
            return;
        }
        if archived {
            // By us or by berthd's scan: it leaves its pane (berthd refuses
            // to attach it), and a question about archiving it is moot.
            if matches!(&self.confirm, Some(Confirm::Archive { session, .. }) if *session == sid) {
                self.confirm = None;
            }
            self.close_pane(out, sid, now);
            return;
        }
        let Some(p) = self.panes.get_mut(&sid) else {
            return;
        };
        if was_live == Some(false) && live && self.connected {
            // Revived: the actor may be a new one (after a crash) that has
            // no subscription of ours — attach again (the view is kept).
            p.detached();
            self.sync_panes(out, now);
        } else if self.focused == Some(sid) {
            self.mark_read(out, sid);
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
        if self.confirm.as_ref().and_then(Confirm::session) == Some(sid) {
            self.confirm = None;
        }
        // Its pane closes (no `Detach`: the session is gone); the last one
        // shows the first session left.
        self.panes.remove(&sid);
        if let Some(tree) = self.layout.as_mut() {
            match tree.remove(sid) {
                Removal::Removed { focus } => {
                    if self.focused == Some(sid) {
                        self.focused = Some(focus);
                    }
                }
                Removal::LastPane => {
                    self.layout = None;
                    self.focused = None;
                    if let Some(first) = self.jump_order().first().copied() {
                        self.show(first);
                    }
                }
                Removal::NotShown => return,
            }
        }
        self.sync_panes(out, now);
    }

    fn on_screen(&mut self, reply_to: Option<u32>, u: &ScreenUpdate) {
        let Some(p) = self.panes.get_mut(&u.session) else {
            return;
        };
        let baseline = reply_to.is_some() && reply_to == p.attach_id;
        if baseline {
            p.attach_id = None;
        } else if p.attach_id.is_some() {
            return; // waiting for the Attach answer
        }
        if p.view.apply_screen(u, baseline) {
            self.counters.screens += 1;
        }
    }

    // -- panes, focus, visibility, geometry -----------------------------

    /// Show `sid`: focus its pane if it has one, else show it in the
    /// focused pane (sidebar click, ⌘1..9, a new session).
    pub fn focus(&mut self, out: &mut dyn Outbound, sid: SessionId, now: Instant) {
        if !self.showable(sid) {
            return;
        }
        self.show(sid);
        self.sync_panes(out, now);
    }

    /// The layout part of [`Self::focus`].
    fn show(&mut self, sid: SessionId) {
        match self.layout.as_mut() {
            Some(tree) if tree.contains(sid) => {}
            Some(tree) => {
                let target = self
                    .focused
                    .filter(|f| tree.contains(*f))
                    .unwrap_or_else(|| tree.first_leaf());
                tree.replace(target, sid);
            }
            None => self.layout = Some(PaneTree::Leaf(sid)),
        }
        self.focused = Some(sid);
    }

    /// Sidebar menu 「在右侧/下方分屏打开」: `sid` in a new pane beside the
    /// focused one (a session already shown just gets the focus).
    pub fn open_in_split(
        &mut self,
        out: &mut dyn Outbound,
        sid: SessionId,
        dir: SplitDir,
        now: Instant,
    ) {
        if !self.showable(sid) {
            return;
        }
        let target = self.focused.filter(|f| self.panes_in_layout(*f));
        let placed = match (self.layout.as_mut(), target) {
            (Some(tree), Some(target)) => tree.split(target, sid, dir).is_ok(),
            _ => false,
        };
        if !placed {
            self.show(sid);
        }
        self.focused = Some(sid);
        self.sync_panes(out, now);
    }

    /// ⌘D / ⌘⇧D: a new session beside the focused pane, in the focused
    /// session's workspace and cwd, shown once berthd created it. Without a
    /// pane this is ⌘N.
    pub fn split(&mut self, out: &mut dyn Outbound, dir: SplitDir) {
        let Some(meta) = self.focused_meta() else {
            self.new_session(out);
            return;
        };
        let target = meta.id;
        let cwd = (!meta.cwd.as_os_str().is_empty()).then(|| meta.cwd.clone());
        let ws = Some(meta.workspace)
            .filter(|w| self.workspace(*w).is_some())
            .or_else(|| self.workspaces.first().map(|w| w.id));
        let Some(workspace) = ws else {
            self.new_session(out);
            return;
        };
        let req = Request::CreateSession {
            workspace,
            cwd,
            command: self.new_session_command.clone(),
            title: None,
            dims: self.split_dims(target, dir),
        };
        self.send(
            out,
            req,
            Some(Awaiting::CreateSession {
                split: Some((target, dir)),
            }),
        );
    }

    /// 「从分屏移除」: close `sid`'s pane and keep the session running; the
    /// neighbour takes the space (and the focus, if it was focused). The
    /// last pane stays. Returns whether a pane was closed.
    pub fn remove_from_split(
        &mut self,
        out: &mut dyn Outbound,
        sid: SessionId,
        now: Instant,
    ) -> bool {
        let Some(tree) = self.layout.as_mut() else {
            return false;
        };
        match tree.remove(sid) {
            Removal::Removed { focus } => {
                if self.focused == Some(sid) {
                    self.focused = Some(focus);
                }
                self.sync_panes(out, now);
                true
            }
            Removal::NotShown | Removal::LastPane => false,
        }
    }

    /// ⌥⌘ arrows: focus the pane next to the focused one.
    pub fn focus_dir(&mut self, out: &mut dyn Outbound, dir: Direction, now: Instant) {
        let (area, divider) = self.area();
        let next = match (self.layout.as_ref(), self.focused) {
            (Some(tree), Some(from)) => tree.neighbor(area, divider, from, dir),
            _ => None,
        };
        if let Some(sid) = next {
            self.focus(out, sid, now);
        }
    }

    /// A divider is dragged: the split's ratio (clamped); the panes are
    /// resized after the debounce.
    pub fn set_ratio(&mut self, path: &SplitPath, ratio: f32, now: Instant) {
        if self
            .layout
            .as_mut()
            .is_some_and(|t| t.set_ratio(path, ratio))
        {
            self.resize_panes(now);
        }
    }

    /// The terminal area's geometry; every pane whose size in cells
    /// changed sends `Resize` after [`RESIZE_DEBOUNCE`] without further
    /// change.
    pub fn set_geometry(&mut self, g: Geometry, now: Instant) {
        if self.geometry != Some(g) {
            self.geometry = Some(g);
            self.resize_panes(now);
        }
    }

    /// Tests: a terminal area of `dims` cells of 10×20 px, no padding.
    #[cfg(test)]
    pub fn set_grid(&mut self, dims: Dims, now: Instant) {
        let (w, h) = (f32::from(dims.cols) * 10.0, f32::from(dims.rows) * 20.0);
        self.set_geometry(
            Geometry {
                area: Rect::new(0.0, 0.0, w, h),
                cell_w: 10.0,
                cell_h: 20.0,
                pad: 0.0,
                divider: 6.0,
            },
            now,
        );
    }

    /// The terminal area and the divider width (a nominal area before the
    /// window reported one: only the panes' relative places matter then).
    fn area(&self) -> (Rect, f32) {
        match self.geometry {
            Some(g) => (g.area, g.divider),
            None => (Rect::new(0.0, 0.0, 1000.0, 1000.0), 6.0),
        }
    }

    /// Every leaf's cells for the current geometry, in tree order.
    fn leaf_dims(&self) -> Vec<(SessionId, Dims)> {
        let Some(tree) = self.layout.as_ref() else {
            return Vec::new();
        };
        match self.geometry {
            Some(g) => tree
                .layout(g.area, g.divider)
                .panes
                .iter()
                .map(|p| (p.session, fit(&g, p.rect)))
                .collect(),
            None => tree
                .leaves()
                .into_iter()
                .map(|s| (s, FALLBACK_DIMS))
                .collect(),
        }
    }

    /// The cells a new pane would get by splitting `target`'s.
    fn split_dims(&self, target: SessionId, dir: SplitDir) -> Dims {
        let (Some(g), Some(tree)) = (self.geometry, self.layout.as_ref()) else {
            return FALLBACK_DIMS;
        };
        let probe = SessionId::new();
        let mut tree = tree.clone();
        if tree.split(target, probe, dir).is_err() {
            return FALLBACK_DIMS;
        }
        tree.layout(g.area, g.divider)
            .rect_of(probe)
            .map_or(FALLBACK_DIMS, |r| fit(&g, r))
    }

    /// The cells of the pane a new session will be shown in (the focused
    /// one; the whole area when nothing is shown).
    fn replace_dims(&self) -> Dims {
        if let Some(d) = self.focused.and_then(|s| self.pane_dims(s)) {
            return d;
        }
        self.geometry.map_or(FALLBACK_DIMS, |g| fit(&g, g.area))
    }

    fn resize_panes(&mut self, now: Instant) {
        for (sid, dims) in self.leaf_dims() {
            if let Some(p) = self.panes.get_mut(&sid) {
                if p.dims != dims {
                    p.dims = dims;
                    if p.attached {
                        p.resize_due = Some(now + RESIZE_DEBOUNCE);
                    }
                }
            }
        }
    }

    /// Make the panes match the layout: `Detach` the sessions no longer
    /// shown, a view per new pane, sizes from the geometry, `Attach` every
    /// pane not attached on this connection (dropping its preview first),
    /// a focus that is one of the panes, marked read.
    fn sync_panes(&mut self, out: &mut dyn Outbound, now: Instant) {
        let dims = self.leaf_dims();
        let gone: Vec<SessionId> = self
            .panes
            .keys()
            .filter(|sid| !dims.iter().any(|(s, _)| s == *sid))
            .copied()
            .collect();
        for sid in gone {
            let attached = self.panes.remove(&sid).is_some_and(|p| p.attached);
            if attached && self.connected && self.sessions.contains_key(&sid) {
                self.send(
                    out,
                    Request::Detach { session: sid },
                    Some(Awaiting::Command("离开 session ")),
                );
            }
        }
        if !self.focused.is_some_and(|f| self.panes_in_layout(f)) {
            self.focused = self.layout.as_ref().map(PaneTree::first_leaf);
        }
        for &(sid, d) in &dims {
            self.panes
                .entry(sid)
                .or_insert_with(|| Pane::new(SessionView::new(sid), d));
        }
        self.resize_panes(now);
        if self.connected {
            for &(sid, _) in &dims {
                if self.panes.get(&sid).is_none_or(|p| p.attached) {
                    continue;
                }
                if self.subs.remove(&sid).is_some() {
                    self.send(
                        out,
                        Request::Unsubscribe { session: sid },
                        Some(Awaiting::Command("取消预览")),
                    );
                }
                let dims = self.panes[&sid].dims;
                let id = self.send(
                    out,
                    Request::Attach { session: sid, dims },
                    Some(Awaiting::Attach(sid)),
                );
                if let (Some(p), Some(id)) = (self.panes.get_mut(&sid), id) {
                    p.attached = true;
                    p.attach_id = Some(id);
                    p.sent_dims = Some(dims);
                    p.resize_due = None;
                }
            }
        }
        if let Some(sid) = self.focused {
            self.mark_read(out, sid);
        }
    }

    fn panes_in_layout(&self, sid: SessionId) -> bool {
        self.layout.as_ref().is_some_and(|t| t.contains(sid))
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
            // Archived (just now, the list was drawn before): no preview.
            if !self.showable(sid) {
                continue;
            }
            if self.sessions.get(&sid).is_some_and(resumable) {
                self.want_resume(out, sid);
            }
            if self.panes.contains_key(&sid) {
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
            if !self.panes.contains_key(&sid) {
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

    /// Timers: resize debounce, paste retries, history prefetch.
    pub fn tick(&mut self, out: &mut dyn Outbound, now: Instant) {
        self.notices.retain(|n| {
            n.kind == NoticeKind::Error || now.saturating_duration_since(n.at) < INFO_TTL
        });
        if !self.connected {
            return;
        }
        let leaves = self
            .layout
            .as_ref()
            .map(PaneTree::leaves)
            .unwrap_or_default();
        for sid in leaves {
            let Some(p) = self.panes.get_mut(&sid) else {
                continue;
            };
            if !p.resize_due.is_some_and(|t| now >= t) {
                continue;
            }
            p.resize_due = None;
            if p.attached && p.sent_dims != Some(p.dims) {
                let dims = p.dims;
                p.sent_dims = Some(dims);
                self.send(
                    out,
                    Request::Resize { session: sid, dims },
                    Some(Awaiting::Command("调整尺寸")),
                );
            }
        }
        self.pump_paste(out, now);
        self.fetch_history(out, now);
    }

    /// Scroll the focused pane (positive: up into the history).
    pub fn scroll(&mut self, out: &mut dyn Outbound, lines: i64, now: Instant) {
        if let Some(sid) = self.focused {
            self.scroll_in(out, sid, lines, now);
        }
    }

    /// Scroll `sid`'s pane (the wheel over a pane that is not focused).
    pub fn scroll_in(&mut self, out: &mut dyn Outbound, sid: SessionId, lines: i64, now: Instant) {
        if let Some(p) = self.panes.get_mut(&sid) {
            p.view.scroll_by(lines);
        }
        self.fetch_history(out, now);
    }

    fn fetch_history(&mut self, out: &mut dyn Outbound, now: Instant) {
        if !self.connected {
            return;
        }
        let leaves = self
            .layout
            .as_ref()
            .map(PaneTree::leaves)
            .unwrap_or_default();
        let mut failed = None;
        'panes: for sid in leaves {
            let Some(p) = self.panes.get_mut(&sid) else {
                continue;
            };
            if p.attach_id.is_some() {
                continue;
            }
            for f in p.view.wanted_fetches(now) {
                let req = Request::FetchLines {
                    session: sid,
                    start: f.start,
                    count: f.count,
                };
                match out.send(req) {
                    Ok(id) => {
                        p.view.note_fetch(id, f);
                        self.pending.insert(id, Awaiting::Fetch(sid));
                    }
                    Err(e) => {
                        p.view.fetch_failed(0, now);
                        failed = Some(e);
                        break 'panes;
                    }
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
        if let Some(view) = self.view_mut() {
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

    /// Bytes the terminal protocol generates (mouse reports, focus in/out)
    /// for the focused pane's session.
    pub fn report(&mut self, out: &mut dyn Outbound, bytes: Vec<u8>, now: Instant) {
        if let Some(sid) = self.focused {
            self.report_to(out, sid, bytes, now);
        }
    }

    /// Protocol bytes for `sid` (a pane under the pointer): sent like keys
    /// but without scrolling or touching the selection, and silently
    /// dropped when the session cannot take input.
    pub fn report_to(
        &mut self,
        out: &mut dyn Outbound,
        sid: SessionId,
        bytes: Vec<u8>,
        now: Instant,
    ) {
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
        let modes = self.view().map(SessionView::modes).unwrap_or_default();
        let bytes = paste::encode(text, modes);
        if bytes.is_empty() {
            return;
        }
        if let Some(view) = self.view_mut() {
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

    /// ⌘N / ⌘T: a session in the focused session's workspace (else the first;
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
            dims: self.replace_dims(),
        };
        self.send(out, req, Some(Awaiting::CreateSession { split: None }));
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

    /// ⌘W / 「关闭 pane」 (DESIGN §17.3): close the focused pane and archive
    /// its session ([`Self::request_archive`]). A session never has a
    /// second pane, so no pane is left showing it. Deleting a session is
    /// 「彻底删除…」 in the 「归档」 section.
    pub fn request_close(&mut self, out: &mut dyn Outbound, now: Instant) {
        if let Some(sid) = self.focused {
            self.request_archive(out, sid, now);
        }
    }

    /// 「归档」 (DESIGN §17.1): a live session running an agent or a
    /// command asks first ([`Confirm::Archive`]), any other at once. Only
    /// `Archive` is sent (berthd ends a live session itself); its pane
    /// closes as it goes out.
    pub fn request_archive(&mut self, out: &mut dyn Outbound, sid: SessionId, now: Instant) {
        let Some(meta) = self.sessions.get(&sid).filter(|m| !m.is_archived()) else {
            return;
        };
        let agent = &meta.agent;
        if meta.is_live() && (agent.kind.is_agent() || agent.state.is_busy()) {
            let what = if agent.kind.is_agent() {
                format!("{} 正在这个 session 里运行", notify_name(&agent.kind))
            } else {
                "这个 session 里有命令正在运行".to_string()
            };
            self.confirm = Some(Confirm::Archive {
                session: sid,
                title: meta.title().to_string(),
                what,
            });
            return;
        }
        self.archive(out, sid, now);
    }

    /// Send `Archive`; once sent, `sid`'s pane closes.
    fn archive(&mut self, out: &mut dyn Outbound, sid: SessionId, now: Instant) {
        let req = Request::Archive { session: sid };
        if self
            .send(out, req, Some(Awaiting::Command("归档")))
            .is_some()
        {
            self.close_pane(out, sid, now);
        }
    }

    /// Take `sid`'s pane out of the layout: the neighbour takes the space
    /// (and the focus, if it had it); after the last pane the terminal area
    /// is empty.
    fn close_pane(&mut self, out: &mut dyn Outbound, sid: SessionId, now: Instant) {
        let Some(tree) = self.layout.as_mut() else {
            return;
        };
        match tree.remove(sid) {
            Removal::Removed { focus } => {
                if self.focused == Some(sid) {
                    self.focused = Some(focus);
                }
            }
            Removal::LastPane => {
                self.layout = None;
                self.focused = None;
            }
            Removal::NotShown => return,
        }
        self.sync_panes(out, now);
    }

    /// 「恢复」 of an archived session: back in its workspace, dormant
    /// (Revive as before).
    pub fn unarchive(&mut self, out: &mut dyn Outbound, sid: SessionId) {
        if self
            .sessions
            .get(&sid)
            .is_some_and(SessionMeta::is_archived)
        {
            self.send(
                out,
                Request::Unarchive { session: sid },
                Some(Awaiting::Command("恢复")),
            );
        }
    }

    /// 「彻底删除…」 of an archived session: asks first.
    pub fn request_delete(&mut self, sid: SessionId) {
        if let Some(m) = self.sessions.get(&sid).filter(|m| m.is_archived()) {
            self.confirm = Some(Confirm::Delete {
                session: sid,
                title: m.title().to_string(),
            });
        }
    }

    pub fn answer_confirm(&mut self, out: &mut dyn Outbound, yes: bool, now: Instant) {
        let Some(c) = self.confirm.take() else {
            return;
        };
        if !yes {
            return;
        }
        match c {
            Confirm::Archive { session, .. } => self.archive(out, session, now),
            Confirm::Delete { session, .. } => {
                self.send(
                    out,
                    Request::Delete { session },
                    Some(Awaiting::Command("删除 session ")),
                );
            }
            Confirm::DeleteWorkspace { id, .. } => {
                self.send(
                    out,
                    Request::DeleteWorkspace { id },
                    Some(Awaiting::Command("删除 workspace ")),
                );
            }
        }
    }

    /// 「重命名…」 of a session; an empty title restores the automatic one.
    pub fn rename(&mut self, out: &mut dyn Outbound, sid: SessionId, title: &str) {
        if !self.sessions.contains_key(&sid) {
            return;
        }
        let title = title.trim();
        let title = (!title.is_empty()).then(|| title.to_string());
        self.send(
            out,
            Request::Rename {
                session: sid,
                title,
            },
            Some(Awaiting::Command("重命名 session ")),
        );
    }

    /// 「重命名…」 of a workspace (a workspace keeps a name: empty is
    /// ignored).
    pub fn rename_workspace(&mut self, out: &mut dyn Outbound, id: WorkspaceId, name: &str) {
        let name = name.trim();
        if name.is_empty() || self.workspace(id).is_none() {
            return;
        }
        self.send(
            out,
            Request::RenameWorkspace {
                id,
                name: name.to_string(),
            },
            Some(Awaiting::Command("重命名 workspace ")),
        );
    }

    /// Whether a workspace has sessions (dormant and archived ones count:
    /// deleting the workspace would orphan them).
    pub fn workspace_has_sessions(&self, id: WorkspaceId) -> bool {
        self.sessions.values().any(|m| m.workspace == id)
    }

    /// 「删除 workspace…」: asks first; refused while it has sessions.
    pub fn request_delete_workspace(&mut self, id: WorkspaceId) {
        let Some(name) = self.workspace(id).map(|w| w.name.clone()) else {
            return;
        };
        if self.workspace_has_sessions(id) {
            self.info("先移走或彻底删除其中的 session");
            return;
        }
        self.confirm = Some(Confirm::DeleteWorkspace { id, name });
    }

    /// 「标记已读」.
    pub fn mark_read_now(&mut self, out: &mut dyn Outbound, sid: SessionId) {
        if !self.connected || !self.sessions.contains_key(&sid) {
            return;
        }
        self.mark_read_sent.insert(sid);
        self.send(
            out,
            Request::MarkRead { session: sid },
            Some(Awaiting::Command("标记已读")),
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
    fn a_dormant_card_whose_agent_left_asks_for_its_resume_command() {
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        let w = ws(0);
        let mut left = claude_dormant(&w, 0, "abc");
        left.agent.kind = AgentKind::Shell;
        left.agent.last_agent = Some(AgentKind::Claude);
        let mut shell = session(&w, 1, false);
        shell.agent.external_id = Some("abc".into()); // no agent to resume it
        let (lid, sid) = (left.id, shell.id);
        assert!(resumable(&left) && !resumable(&shell));
        listed(&mut c, &mut out, vec![w], vec![left.clone(), shell]);
        let now = Instant::now();
        c.set_visible(&mut out, &[lid, sid], now);
        let sent = out.take();
        let asked = requests(&sent, |r| matches!(r, Request::ResumeCommand { .. }));
        assert_eq!(asked.len(), 1, "{sent:?}");
        assert_eq!(*asked[0].1, Request::ResumeCommand { session: lid });
        let argv = vec!["claude".to_string(), "--resume".into(), "abc".into()];
        let answer = Event::ResumeCommand {
            session: lid,
            cwd: PathBuf::from("/tmp/proj"),
            command: Ok(argv),
        };
        c.handle(&mut out, reply(asked[0].0, answer), now);
        assert!(c.resume_preview(lid).is_some());
        // Another agent left since: the preview was for Claude.
        let mut codex = left;
        codex.agent.last_agent = Some(AgentKind::Codex);
        c.handle(&mut out, push(Event::SessionUpdated(codex)), now);
        assert!(c.resume_preview(lid).is_none());
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

    /// A session archived by berthd (its answer to `Archive`, or its scan).
    fn archived(m: &SessionMeta, at_ms: i64) -> SessionMeta {
        let mut m = m.clone();
        m.status = SessionStatus::Dormant {
            exit_code: None,
            at_ms,
        };
        m.archived_at_ms = Some(at_ms);
        m
    }

    #[test]
    fn close_archives_the_focused_session_and_asks_first_while_it_is_busy() {
        let w = ws(0);
        let mut agent = session(&w, 0, true);
        agent.agent = AgentInfo {
            kind: AgentKind::Claude,
            state: AgentState::Idle,
            ..Default::default()
        };
        let mut busy = session(&w, 1, true);
        busy.agent.state = AgentState::Thinking;
        let shell = session(&w, 2, true);
        let dormant = session(&w, 3, false);
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        listed(
            &mut c,
            &mut out,
            vec![w],
            vec![agent.clone(), busy.clone(), shell.clone(), dormant.clone()],
        );
        let now = Instant::now();
        // Live with an agent: asks; 「取消」 changes nothing.
        c.request_close(&mut out, now);
        assert!(matches!(
            c.confirm(),
            Some(Confirm::Archive { session, what, .. })
                if *session == agent.id && what.contains("claude")
        ));
        c.answer_confirm(&mut out, false, now);
        assert!(out.take().is_empty() && c.confirm().is_none());
        assert_eq!(c.layout(), Some(&PaneTree::Leaf(agent.id)));
        // Confirmed: only `Archive` (berthd ends the session itself); the
        // pane closes, and without another one the terminal area is empty.
        c.request_close(&mut out, now);
        c.answer_confirm(&mut out, true, now);
        assert_eq!(
            names(&out.take()),
            [n("Archive", agent.id), n("Detach", agent.id)]
        );
        assert_eq!(c.layout(), None);
        assert_eq!(c.focused(), None);
        c.request_close(&mut out, now);
        assert!(out.take().is_empty() && c.confirm().is_none(), "no pane");
        // A command running: asks too.
        c.focus(&mut out, busy.id, now);
        out.take();
        c.request_close(&mut out, now);
        assert!(matches!(
            c.confirm(),
            Some(Confirm::Archive { session, what, .. })
                if *session == busy.id && what.contains("命令")
        ));
        c.answer_confirm(&mut out, false, now);
        // An idle shell and a dormant session: at once.
        for s in [&shell, &dormant] {
            c.focus(&mut out, s.id, now);
            out.take();
            c.request_close(&mut out, now);
            assert!(c.confirm().is_none());
            assert_eq!(names(&out.take()), [n("Archive", s.id), n("Detach", s.id)]);
            assert_eq!(c.layout(), None);
        }
    }

    #[test]
    fn archived_sessions_are_left_out_of_the_lists_the_badge_and_notifications() {
        let w = ws(0);
        let mut a = session(&w, 0, true);
        a.agent.state = AgentState::WaitingInput;
        let b = session(&w, 1, false);
        let mut old = archived(&session(&w, 2, false), 10);
        old.agent.state = AgentState::Done;
        // Never sent by berthd (an archived session is not live); the
        // filters do not rely on that.
        let mut odd = session(&w, 3, true);
        odd.archived_at_ms = Some(20);
        odd.agent.state = AgentState::WaitingPermission { tool: None };
        let mut c = Controller::new(notify::DEFAULT_ON.iter().map(|s| s.to_string()).collect());
        let mut out = Fake::default();
        listed(
            &mut c,
            &mut out,
            vec![w.clone()],
            vec![a.clone(), b.clone(), old.clone(), odd.clone()],
        );
        assert_eq!(c.jump_order(), [a.id, b.id], "⌘1..9");
        assert_eq!(c.live_in(w.id).len(), 1);
        assert_eq!(c.dormant().len(), 1);
        let ids: Vec<SessionId> = c.archived().iter().map(|m| m.id).collect();
        assert_eq!(ids, [odd.id, old.id], "most recently archived first");
        assert_eq!(c.attention_count(), 1, "the Dock badge");
        let now = Instant::now();
        // No preview, no pane.
        c.set_visible(&mut out, &[a.id, b.id, old.id, odd.id], now);
        assert_eq!(names(&out.take()), [n("Subscribe", b.id)]);
        c.focus(&mut out, old.id, now);
        c.open_in_split(&mut out, odd.id, SplitDir::Right, now);
        assert_eq!(c.layout(), Some(&PaneTree::Leaf(a.id)));
        assert!(out.take().is_empty());
        // Nobody looks at the window: only sessions in the lists notify.
        c.set_window_focused(&mut out, false);
        let changed = |sid| {
            push(Event::AgentChanged {
                session: sid,
                agent: AgentInfo {
                    kind: AgentKind::Claude,
                    state: AgentState::WaitingInput,
                    ..Default::default()
                },
            })
        };
        let note = |sid| {
            push(Event::Notify {
                session: sid,
                title: None,
                body: "hi".into(),
            })
        };
        for sid in [old.id, odd.id] {
            assert!(c.handle(&mut out, changed(sid), now).is_empty());
            assert!(c.handle(&mut out, note(sid), now).is_empty());
        }
        assert_eq!(c.attention_count(), 1);
        assert_eq!(c.handle(&mut out, changed(b.id), now).len(), 1);
        assert_eq!(c.handle(&mut out, note(a.id), now).len(), 1);
    }

    #[test]
    fn an_archived_session_leaves_its_pane_and_the_last_one_leaves_the_area_empty() {
        let w = ws(0);
        let a = session(&w, 0, true);
        let mut b = session(&w, 1, true);
        b.agent.kind = AgentKind::Claude;
        let x = session(&w, 2, true);
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        listed(
            &mut c,
            &mut out,
            vec![w.clone()],
            vec![a.clone(), b.clone(), x.clone()],
        );
        let now = Instant::now();
        c.open_in_split(&mut out, b.id, SplitDir::Right, now);
        c.request_archive(&mut out, b.id, now);
        assert!(matches!(c.confirm(), Some(Confirm::Archive { session, .. }) if *session == b.id));
        out.take();
        // berthd's scan archives it meanwhile: its pane closes (the
        // neighbour takes the focus) and the question is moot.
        c.handle(&mut out, push(Event::SessionUpdated(archived(&b, 5))), now);
        assert!(c.confirm().is_none());
        assert_eq!(c.layout(), Some(&PaneTree::Leaf(a.id)));
        assert_eq!(c.focused(), Some(a.id));
        assert_eq!(names(&out.take()), [n("Detach", b.id)]);
        // The last pane: the terminal area is empty; nothing else opens.
        c.handle(&mut out, push(Event::SessionUpdated(archived(&a, 6))), now);
        assert_eq!(c.layout(), None);
        assert_eq!(c.focused(), None);
        assert_eq!(names(&out.take()), [n("Detach", a.id)]);
        assert_eq!(c.jump_order(), [x.id]);
        // A click opens one.
        c.focus(&mut out, x.id, now);
        assert_eq!(names(&out.take()), [n("Attach", x.id)]);

        // Archived while disconnected: pruned at the reconnect …
        c.open_in_split(&mut out, a.id, SplitDir::Down, now);
        assert_eq!(c.layout(), Some(&PaneTree::Leaf(x.id)), "not archived ones");
        c.handle(&mut out, push(Event::SessionUpdated(a.clone())), now); // restored, revived
        c.open_in_split(&mut out, a.id, SplitDir::Down, now);
        assert_eq!(c.layout().unwrap().leaves(), [x.id, a.id]);
        out.take();
        c.on_disconnected("gone");
        let sent = listed(
            &mut c,
            &mut out,
            vec![w.clone()],
            vec![archived(&a, 7), archived(&b, 5), x.clone()],
        );
        assert_eq!(c.layout(), Some(&PaneTree::Leaf(x.id)));
        assert_eq!(names(&sent), [n("Attach", x.id)]);
        // … and a reconnect leaves an empty area empty.
        c.on_disconnected("gone");
        let sent = listed(
            &mut c,
            &mut out,
            vec![w],
            vec![archived(&a, 7), archived(&b, 5), archived(&x, 8)],
        );
        assert_eq!(c.layout(), None);
        assert!(sent.is_empty(), "{sent:?}");
    }

    #[test]
    fn an_archived_session_is_not_opened_at_start() {
        let w = ws(0);
        let a = session(&w, 0, true);
        let old = archived(&session(&w, 1, false), 1);
        // `--session` / `--split-right` naming it: an error, the rest opens.
        for split in [false, true] {
            let mut c = Controller::new(vec![]);
            if split {
                c.want_session(a.id.short());
                c.want_split(SplitDir::Right, old.id.short());
            } else {
                c.want_session(old.id.short());
            }
            let mut out = Fake::default();
            let sent = listed(
                &mut c,
                &mut out,
                vec![w.clone()],
                vec![a.clone(), old.clone()],
            );
            assert_eq!(c.layout(), Some(&PaneTree::Leaf(a.id)));
            assert_eq!(names(&sent), [n("Attach", a.id)]);
            assert!(
                c.notices().iter().any(|n| n.text.contains("已归档")),
                "{:?}",
                c.notices()
            );
        }
        // Nothing but archived sessions: a first start (one is created).
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        c.on_connected(&mut out);
        let sent = out.take();
        let now = Instant::now();
        c.handle(
            &mut out,
            reply(sent[0].0, Event::Workspaces(vec![w.clone()])),
            now,
        );
        c.handle(&mut out, reply(sent[1].0, Event::Sessions(vec![old])), now);
        assert_eq!(c.layout(), None);
        assert!(matches!(
            out.take().as_slice(),
            [(_, Request::CreateSession { workspace, .. })] if *workspace == w.id
        ));
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

    /// `(request, session)` of each request, for compact assertions.
    fn names(sent: &[(u32, Request)]) -> Vec<(String, Option<SessionId>)> {
        sent.iter()
            .map(|(_, r)| {
                let debug = format!("{r:?}");
                let name = debug.split([' ', '{', '(']).next().unwrap_or_default();
                let session = match r {
                    Request::Attach { session, .. }
                    | Request::Detach { session }
                    | Request::Subscribe { session, .. }
                    | Request::Unsubscribe { session }
                    | Request::Resize { session, .. }
                    | Request::Kill { session }
                    | Request::MarkRead { session }
                    | Request::Revive { session, .. }
                    | Request::Archive { session }
                    | Request::Unarchive { session }
                    | Request::Delete { session } => Some(*session),
                    _ => None,
                };
                (name.to_string(), session)
            })
            .collect()
    }

    fn n(name: &str, sid: SessionId) -> (String, Option<SessionId>) {
        (name.to_string(), Some(sid))
    }

    const HALF_WIDTH: Dims = Dims { cols: 49, rows: 30 };
    const QUARTER: Dims = Dims { cols: 49, rows: 14 };

    #[test]
    fn split_creates_a_session_beside_the_focused_pane_in_its_directory() {
        let w = ws(0);
        let mut a = session(&w, 0, true);
        a.cwd = PathBuf::from("/tmp/proj");
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        let sent = listed(&mut c, &mut out, vec![w.clone()], vec![a.clone()]);
        assert_eq!(names(&sent), [n("Attach", a.id)]);
        let t0 = Instant::now();
        c.split(&mut out, SplitDir::Right);
        let sent = out.take();
        let [(
            id,
            Request::CreateSession {
                workspace,
                cwd,
                dims,
                ..
            },
        )] = sent.as_slice()
        else {
            panic!("{sent:?}")
        };
        assert_eq!(*workspace, w.id);
        assert_eq!(cwd.as_deref(), Some(Path::new("/tmp/proj")));
        // 100 columns less the 6 px divider, halved.
        assert_eq!(*dims, HALF_WIDTH);
        assert_eq!(c.pane_count(), 1, "shown once berthd created it");
        let b = session(&w, 1, true);
        c.handle(&mut out, reply(*id, Event::SessionUpdated(b.clone())), t0);
        assert_eq!(c.layout().unwrap().leaves(), [a.id, b.id]);
        assert_eq!(c.focused(), Some(b.id));
        let sent = out.take();
        assert!(
            matches!(sent.as_slice(), [(_, Request::Attach { session, dims })] if *session == b.id && *dims == HALF_WIDTH),
            "{sent:?}"
        );
        // The pane that was split shrinks after the debounce.
        c.tick(&mut out, t0 + RESIZE_DEBOUNCE);
        assert!(matches!(
            out.take().as_slice(),
            [(_, Request::Resize { session, dims })] if *session == a.id && *dims == HALF_WIDTH
        ));
        // ⌘⇧D splits the focused (new) pane downwards.
        c.split(&mut out, SplitDir::Down);
        let sent = out.take();
        let [(id, Request::CreateSession { dims, .. })] = sent.as_slice() else {
            panic!("{sent:?}")
        };
        assert_eq!(*dims, QUARTER);
        let d = session(&w, 2, true);
        c.handle(&mut out, reply(*id, Event::SessionUpdated(d.clone())), t0);
        assert_eq!(c.layout().unwrap().leaves(), [a.id, b.id, d.id]);
        assert_eq!(c.focused(), Some(d.id));
        assert_eq!(names(&out.take()), [n("Attach", d.id)]);
        // A session created otherwise (⌘N) replaces the focused pane's.
        c.new_session(&mut out);
        let sent = out.take();
        let [(id, Request::CreateSession { dims, .. })] = sent.as_slice() else {
            panic!("{sent:?}")
        };
        assert_eq!(*dims, QUARTER, "the focused pane's size");
        let e = session(&w, 3, true);
        c.handle(&mut out, reply(*id, Event::SessionUpdated(e.clone())), t0);
        assert_eq!(c.layout().unwrap().leaves(), [a.id, b.id, e.id]);
        assert_eq!(
            names(&out.take()),
            [n("Detach", d.id), n("Attach", e.id)],
            "the replaced session keeps running"
        );
    }

    #[test]
    fn a_click_focuses_a_shown_pane_or_replaces_the_focused_one() {
        let w = ws(0);
        let (a, b, x) = (
            session(&w, 0, true),
            session(&w, 1, true),
            session(&w, 2, true),
        );
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        listed(
            &mut c,
            &mut out,
            vec![w],
            vec![a.clone(), b.clone(), x.clone()],
        );
        let now = Instant::now();
        // Previews for the cards not shown in a pane.
        c.set_visible(&mut out, &[a.id, b.id, x.id], now);
        assert_eq!(
            names(&out.take()),
            [n("Subscribe", b.id), n("Subscribe", x.id)]
        );
        c.open_in_split(&mut out, b.id, SplitDir::Right, now);
        assert_eq!(c.layout().unwrap().leaves(), [a.id, b.id]);
        assert_eq!(c.focused(), Some(b.id));
        assert_eq!(
            names(&out.take()),
            [n("Unsubscribe", b.id), n("Attach", b.id)]
        );
        // A shown session only gets the focus.
        c.focus(&mut out, a.id, now);
        assert_eq!(c.focused(), Some(a.id));
        assert!(out.take().is_empty());
        c.open_in_split(&mut out, b.id, SplitDir::Down, now);
        assert_eq!(c.layout().unwrap().leaves(), [a.id, b.id], "never twice");
        assert_eq!(c.focused(), Some(b.id));
        assert!(out.take().is_empty());
        // Another one replaces the focused pane's session.
        c.focus(&mut out, a.id, now);
        c.focus(&mut out, x.id, now);
        assert_eq!(c.layout().unwrap().leaves(), [x.id, b.id]);
        assert_eq!(
            names(&out.take()),
            [n("Detach", a.id), n("Unsubscribe", x.id), n("Attach", x.id)]
        );
        c.set_visible(&mut out, &[a.id, b.id, x.id], now);
        assert_eq!(names(&out.take()), [n("Subscribe", a.id)]);
        // ⌘W closes only the focused pane and archives its session (an
        // idle shell: at once); the neighbour takes the space and the focus.
        c.request_close(&mut out, now);
        assert_eq!(names(&out.take()), [n("Archive", x.id), n("Detach", x.id)]);
        assert_eq!(c.layout(), Some(&PaneTree::Leaf(b.id)));
        assert_eq!(c.focused(), Some(b.id));
        c.handle(&mut out, push(Event::SessionUpdated(archived(&x, 1))), now);
        assert_eq!(c.layout(), Some(&PaneTree::Leaf(b.id)));
        assert!(out.take().is_empty());
        assert_eq!(
            c.pane_dims(b.id),
            Some(Dims {
                cols: 100,
                rows: 30
            })
        );
    }

    #[test]
    fn removing_a_pane_from_the_split_keeps_its_session() {
        let w = ws(0);
        let (a, b) = (session(&w, 0, true), session(&w, 1, true));
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        listed(&mut c, &mut out, vec![w], vec![a.clone(), b.clone()]);
        let now = Instant::now();
        c.open_in_split(&mut out, b.id, SplitDir::Down, now);
        assert_eq!(
            c.pane_dims(b.id),
            Some(Dims {
                cols: 100,
                rows: 14
            })
        );
        out.take();
        assert!(c.remove_from_split(&mut out, b.id, now));
        assert_eq!(names(&out.take()), [n("Detach", b.id)]);
        assert!(c.session(b.id).is_some());
        assert_eq!(c.layout(), Some(&PaneTree::Leaf(a.id)));
        assert_eq!(c.focused(), Some(a.id));
        // The last pane stays.
        assert!(!c.remove_from_split(&mut out, a.id, now));
        assert!(!c.remove_from_split(&mut out, b.id, now));
        assert!(out.take().is_empty());
        assert_eq!(c.pane_count(), 1);
    }

    #[test]
    fn arrows_move_the_focus_and_a_divider_drag_resizes_both_panes() {
        let w = ws(0);
        let (a, b) = (session(&w, 0, true), session(&w, 1, true));
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        listed(&mut c, &mut out, vec![w], vec![a.clone(), b.clone()]);
        let now = Instant::now();
        c.open_in_split(&mut out, b.id, SplitDir::Right, now);
        c.focus_dir(&mut out, Direction::Left, now);
        assert_eq!(c.focused(), Some(a.id));
        c.focus_dir(&mut out, Direction::Left, now);
        c.focus_dir(&mut out, Direction::Up, now);
        assert_eq!(c.focused(), Some(a.id), "nothing there");
        c.focus_dir(&mut out, Direction::Right, now);
        assert_eq!(c.focused(), Some(b.id));
        out.take();
        let t0 = Instant::now();
        c.set_ratio(&SplitPath::default(), 0.25, t0);
        assert_eq!(c.pane_dims(a.id), Some(Dims { cols: 24, rows: 30 }));
        c.tick(&mut out, t0 + RESIZE_DEBOUNCE / 2);
        assert!(out.take().is_empty(), "still debouncing");
        c.tick(&mut out, t0 + RESIZE_DEBOUNCE);
        let sent = out.take();
        assert!(
            matches!(
                sent.as_slice(),
                [
                    (_, Request::Resize { session: s1, dims: Dims { cols: 24, rows: 30 } }),
                    (_, Request::Resize { session: s2, dims: Dims { cols: 74, rows: 30 } }),
                ] if *s1 == a.id && *s2 == b.id
            ),
            "{sent:?}"
        );
        // Clamped: neither pane disappears.
        c.set_ratio(&SplitPath::default(), 0.0, t0);
        assert_eq!(c.pane_dims(a.id), Some(Dims { cols: 19, rows: 30 }));
    }

    #[test]
    fn a_reconnect_attaches_every_pane_again() {
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
        c.open_in_split(&mut out, b.id, SplitDir::Right, now);
        c.focus(&mut out, a.id, now);
        out.take();
        c.on_disconnected("gone");
        let sent = listed(&mut c, &mut out, vec![w], vec![a.clone(), b.clone()]);
        // Nothing is unsubscribed (that would drop the attachments too).
        assert_eq!(names(&sent), [n("Attach", a.id), n("Attach", b.id)]);
        assert_eq!(c.layout().unwrap().leaves(), [a.id, b.id]);
        assert_eq!(c.focused(), Some(a.id));
    }

    #[test]
    fn the_saved_layout_is_restored_without_sessions_that_are_gone_or_archived() {
        let w = ws(0);
        let (a, b, x) = (
            session(&w, 0, true),
            session(&w, 1, true),
            session(&w, 2, true),
        );
        let mut archived = session(&w, 3, false);
        archived.archived_at_ms = Some(1);
        let gone = SessionId::new();
        let mut tree = PaneTree::Leaf(a.id);
        tree.split(a.id, b.id, SplitDir::Right).unwrap();
        tree.split(b.id, gone, SplitDir::Down).unwrap();
        tree.split(a.id, archived.id, SplitDir::Down).unwrap();
        let mut c = Controller::new(vec![]);
        c.restore(GuiState::new(Some(tree), Some(gone)));
        assert_eq!(c.take_layout_changed(), None, "not before the listing");
        let mut out = Fake::default();
        let sent = listed(
            &mut c,
            &mut out,
            vec![w.clone()],
            vec![a.clone(), b.clone(), x.clone(), archived.clone()],
        );
        assert_eq!(c.layout().unwrap().leaves(), [a.id, b.id]);
        assert_eq!(c.focused(), Some(a.id), "the focused pane is gone");
        assert_eq!(names(&sent), [n("Attach", a.id), n("Attach", b.id)]);
        // The pruned layout is written back, once.
        let saved = c.take_layout_changed().unwrap();
        assert_eq!(saved, GuiState::new(c.layout().cloned(), Some(a.id)));
        assert_eq!(c.take_layout_changed(), None);
        c.focus(&mut out, b.id, Instant::now());
        assert_eq!(c.take_layout_changed().unwrap().focused, Some(b.id));

        // Nothing left of it: the first session, as without a saved layout.
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        c.restore(GuiState::new(Some(PaneTree::Leaf(gone)), Some(gone)));
        listed(&mut c, &mut out, vec![w], vec![a.clone(), b.clone()]);
        assert_eq!(c.layout(), Some(&PaneTree::Leaf(a.id)));
        assert_eq!(c.focused(), Some(a.id));
    }

    #[test]
    fn command_line_splits_build_the_layout_in_order() {
        let w = ws(0);
        let (a, b, x, d) = (
            session(&w, 0, true),
            session(&w, 1, true),
            session(&w, 2, true),
            session(&w, 3, true),
        );
        let mut c = Controller::new(vec![]);
        let prefix = |s: &SessionMeta| s.id.short();
        c.want_session(prefix(&a));
        c.want_split(SplitDir::Right, prefix(&b));
        c.want_split(SplitDir::Down, format!("{}@{}", prefix(&x), prefix(&a)));
        c.want_split(SplitDir::Down, format!("{}@{}", prefix(&d), prefix(&b)));
        c.want_split(SplitDir::Right, prefix(&a));
        let mut out = Fake::default();
        let sent = listed(
            &mut c,
            &mut out,
            vec![w],
            vec![a.clone(), b.clone(), x.clone(), d.clone()],
        );
        assert_eq!(c.layout().unwrap().leaves(), [a.id, x.id, b.id, d.id]);
        assert_eq!(c.focused(), Some(a.id));
        let attached = requests(&sent, |r| matches!(r, Request::Attach { .. }));
        assert_eq!(attached.len(), 4, "{sent:?}");
        for (_, r) in attached {
            assert!(
                matches!(r, Request::Attach { dims, .. } if *dims == QUARTER),
                "{r:?}"
            );
        }
        assert!(
            c.notices()
                .iter()
                .any(|n| n.text.contains("分屏参数") && n.text.contains("已在分屏中")),
            "{:?}",
            c.notices()
        );
    }

    #[test]
    fn menu_actions_map_to_controller_calls_or_app_effects() {
        use crate::menus::{apply, AppEffect, MenuAction, RenameTarget};
        let w = ws(0);
        let (a, b) = (session(&w, 0, true), session(&w, 1, false));
        let mut c = Controller::new(vec![]);
        let mut out = Fake::default();
        listed(
            &mut c,
            &mut out,
            vec![w.clone()],
            vec![a.clone(), b.clone()],
        );
        let now = Instant::now();
        let mut act = |c: &mut Controller, action| apply(c, &mut out, action, now);
        assert_eq!(act(&mut c, MenuAction::Copy), Some(AppEffect::Copy));
        assert_eq!(act(&mut c, MenuAction::Paste), Some(AppEffect::Paste));
        assert_eq!(
            act(&mut c, MenuAction::RenameSession(a.id)),
            Some(AppEffect::Rename(RenameTarget::Session(a.id)))
        );
        assert_eq!(
            act(&mut c, MenuAction::RenameWorkspace(w.id)),
            Some(AppEffect::Rename(RenameTarget::Workspace(w.id)))
        );
        assert_eq!(
            act(&mut c, MenuAction::OpenInSplit(b.id, SplitDir::Down)),
            None
        );
        assert_eq!(c.layout().unwrap().leaves(), [a.id, b.id]);
        assert_eq!(act(&mut c, MenuAction::RemoveFromSplit(b.id)), None);
        assert_eq!(c.layout(), Some(&PaneTree::Leaf(a.id)));
        act(&mut c, MenuAction::MarkRead(a.id));
        act(&mut c, MenuAction::Revive(b.id));
        act(&mut c, MenuAction::NewSessionIn(w.id));
        act(&mut c, MenuAction::Split(SplitDir::Right));
        // 「关闭 pane」 is ⌘W on that pane: it closes (the last one) and its
        // session is archived.
        act(&mut c, MenuAction::ClosePane(a.id));
        assert_eq!(c.layout(), None);
        // 「归档」 of a dormant session: at once.
        act(&mut c, MenuAction::Archive(b.id));
        // Refused while the workspace has sessions.
        act(&mut c, MenuAction::DeleteWorkspace(w.id));
        assert!(c.confirm().is_none());
        assert!(c
            .notices()
            .iter()
            .any(|n| n.text.contains("先移走或彻底删除")));
        let sent = out.take();
        let got: Vec<String> = names(&sent).into_iter().map(|(name, _)| name).collect();
        assert_eq!(
            got,
            [
                "Attach",   // OpenInSplit
                "Detach",   // RemoveFromSplit
                "MarkRead", // MarkRead
                "Detach",   // Revive focuses b in the (only) pane …
                "Attach",
                "Revive",        // … then revives it
                "CreateSession", // NewSessionIn
                "CreateSession", // Split
                "Detach",        // ClosePane focuses a again …
                "Attach",
                "Archive", // … archives it (an idle shell: at once) …
                "Detach",  // … and its pane closes
                "Archive", // Archive (b)
            ],
            "{sent:?}"
        );
        assert!(matches!(
            &sent[5].1,
            Request::Revive { session, mode: ReviveMode::Shell } if *session == b.id
        ));
        assert!(matches!(&sent[10].1, Request::Archive { session } if *session == a.id));
        assert!(matches!(&sent[12].1, Request::Archive { session } if *session == b.id));
        // The 「归档」 section's entries, once berthd archived b.
        c.handle(&mut out, push(Event::SessionUpdated(archived(&b, 1))), now);
        apply(&mut c, &mut out, MenuAction::Unarchive(b.id), now);
        assert_eq!(names(&out.take()), [n("Unarchive", b.id)]);
        apply(&mut c, &mut out, MenuAction::DeleteForever(b.id), now);
        assert!(matches!(c.confirm(), Some(Confirm::Delete { session, .. }) if *session == b.id));
        assert!(out.take().is_empty(), "asks first");
        c.answer_confirm(&mut out, true, now);
        assert_eq!(names(&out.take()), [n("Delete", b.id)]);
    }

    /// Against the real daemon: create, type, page through history, paste
    /// through the chunked pipeline, close (archive), restore and revive
    /// with history kept.
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

        // ⌘W (a plain command: no confirmation): the pane closes and berthd
        // ends the session, then archives it.
        c.request_close(&mut client, Instant::now());
        if c.confirm().is_some() {
            c.answer_confirm(&mut client, true, Instant::now());
        }
        assert_eq!(c.layout(), None);
        pump(
            &mut c,
            &mut client,
            &|c| {
                c.session(sid)
                    .is_some_and(|m| m.is_archived() && !m.is_live())
            },
            "the session to be archived",
        );
        assert!(c.jump_order().is_empty());
        assert_eq!(c.archived().len(), 1);
        // 「恢复」, then revive the same argv.
        c.unarchive(&mut client, sid);
        pump(
            &mut c,
            &mut client,
            &|c| c.session(sid).is_some_and(|m| !m.is_archived()),
            "the session to be restored",
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
