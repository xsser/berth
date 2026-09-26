//! Registry of workspaces and sessions, persistence orchestration, restore
//! on start, hook routing, fan-out of session-level events to clients.
//!
//! Locking: `reg` and `conns` are never held while waiting on an actor or
//! doing slow I/O; store writes happen after the registry lock is released.
//! Actors call back into the manager from their own threads.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use berth_core::{
    now_ms, AgentInfo, AgentSignal, AgentState, ClaudeHookEvent, ClientRole, DaemonMsg,
    DaemonStatus, Dims, Event, HookEnvelope, Paths, PersistPolicy, ReviveMode, SessionId,
    SessionMeta, SessionStatus, StateSource, SubscribeMode, Workspace, WorkspaceId,
};
use berth_store::{EventRecord, Store};
use berth_vt::PtySpawn;
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use tokio::sync::{oneshot, watch};

use crate::agent_state::{is_valid_external_id, AgentMachine, Signal, HEURISTIC_CONFIDENCE};
use crate::config::Config;
use crate::hooks;
use crate::outbox::Outbox;
use crate::session::{self, ActorConfig, ActorHandle, SessionCmd};
use crate::view::ConnId;

pub type Result<T> = std::result::Result<T, String>;

/// How long `shutdown` waits for each session to write its snapshot.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "nu", "elvish", "xonsh",
];

pub struct Manager {
    pub(crate) paths: Paths,
    pub(crate) config: Config,
    pub(crate) store: Store,
    started_ms: i64,
    reg: Mutex<Registry>,
    conns: Mutex<HashMap<ConnId, ConnEntry>>,
    next_conn: AtomicU64,
    stop_tx: watch::Sender<bool>,
}

struct ConnEntry {
    outbox: Arc<Outbox>,
    role: ClientRole,
}

#[derive(Default)]
struct Registry {
    workspaces: HashMap<WorkspaceId, Workspace>,
    sessions: HashMap<SessionId, Entry>,
}

struct Entry {
    meta: SessionMeta,
    machine: AgentMachine,
    actor: Option<ActorHandle>,
    /// Connections with a `Full` attach (for the unread rule).
    attached: HashSet<ConnId>,
}

impl Entry {
    fn new(meta: SessionMeta) -> Entry {
        let machine = AgentMachine::new(meta.agent.clone());
        Entry {
            meta,
            machine,
            actor: None,
            attached: HashSet::new(),
        }
    }
}

fn unknown(sid: SessionId) -> String {
    format!("unknown session {sid}")
}

fn stopped() -> String {
    "session actor stopped".to_string()
}

impl Manager {
    /// Load the registry. Sessions that were `Live`/`Dormant` in a previous
    /// daemon life become `Restored` (history only); their snapshots are
    /// loaded lazily when first needed.
    pub fn start(
        paths: Paths,
        config: Config,
        store: Store,
        stop_tx: watch::Sender<bool>,
    ) -> anyhow::Result<Arc<Manager>> {
        let now = now_ms();
        let mut reg = Registry::default();
        for ws in store.list_workspaces()? {
            reg.workspaces.insert(ws.id, ws);
        }
        for mut meta in store.list_sessions()? {
            if !matches!(meta.status, SessionStatus::Restored) {
                meta.status = SessionStatus::Restored;
                if !matches!(meta.agent.state, AgentState::Exited { .. }) {
                    meta.agent.state = AgentState::Exited { code: None };
                    meta.agent.since_ms = now;
                }
                if let Err(e) = store.upsert_session(&meta) {
                    tracing::warn!(session = %meta.id, error = %e, "cannot persist restored status");
                }
            }
            reg.sessions.insert(meta.id, Entry::new(meta));
        }
        tracing::info!(
            workspaces = reg.workspaces.len(),
            sessions = reg.sessions.len(),
            "registry loaded"
        );
        Ok(Arc::new(Manager {
            paths,
            config,
            store,
            started_ms: now,
            reg: Mutex::new(reg),
            conns: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
            stop_tx,
        }))
    }

    fn actor_config(&self) -> ActorConfig {
        ActorConfig {
            scrollback: self.config.scrollback(),
            snapshot_interval: self.config.snapshot_interval(),
            max_restored_lines: self.config.max_restored_lines(),
            osc52_store: self.config.terminal.osc52_store,
        }
    }

    // -- connections ----------------------------------------------------------

    pub fn register_conn(&self, role: ClientRole, outbox: Arc<Outbox>) -> ConnId {
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.conns.lock().insert(id, ConnEntry { outbox, role });
        id
    }

    pub fn conn_closed(&self, conn: ConnId) {
        self.conns.lock().remove(&conn);
        let mut reg = self.reg.lock();
        for entry in reg.sessions.values_mut() {
            entry.attached.remove(&conn);
            if let Some(actor) = &entry.actor {
                let _ = actor.tx.send(SessionCmd::ConnClosed { conn });
            }
        }
    }

    /// Let every writer flush what is queued, then end.
    pub fn close_all_conns(&self) {
        for c in self.conns.lock().values() {
            c.outbox.close();
        }
    }

    /// Unsolicited event to every GUI / CLI connection.
    pub fn broadcast(&self, event: Event) {
        let conns = self.conns.lock();
        for c in conns.values().filter(|c| c.role != ClientRole::Hook) {
            c.outbox.push(DaemonMsg {
                reply_to: None,
                event: event.clone(),
            });
        }
    }

    // -- persistence helpers ------------------------------------------------

    fn persist(&self, meta: &SessionMeta) {
        if let Err(e) = self.store.upsert_session(meta) {
            tracing::warn!(session = %meta.id, error = %e, "cannot persist session");
        }
    }

    /// Persist + broadcast `SessionUpdated`.
    fn commit(&self, meta: SessionMeta) -> SessionMeta {
        self.persist(&meta);
        self.broadcast(Event::SessionUpdated(meta.clone()));
        meta
    }

    fn update(
        &self,
        sid: SessionId,
        f: impl FnOnce(&mut Entry) -> Result<()>,
    ) -> Result<SessionMeta> {
        let meta = {
            let mut reg = self.reg.lock();
            let entry = reg.sessions.get_mut(&sid).ok_or_else(|| unknown(sid))?;
            f(entry)?;
            entry.meta.clone()
        };
        Ok(self.commit(meta))
    }

    fn record(
        &self,
        sid: SessionId,
        at_ms: i64,
        kind: String,
        state: &AgentState,
        detail: Option<String>,
    ) {
        let e = EventRecord {
            session: sid,
            at_ms,
            kind,
            state: state.name().to_string(),
            detail,
        };
        if let Err(err) = self.store.record_event(&e) {
            tracing::warn!(session = %sid, error = %err, "cannot record event");
        }
    }

    // -- callbacks from session actors ---------------------------------------

    pub(crate) fn meta(&self, sid: SessionId) -> Option<SessionMeta> {
        self.reg.lock().sessions.get(&sid).map(|e| e.meta.clone())
    }

    pub(crate) fn apply_signal(&self, sid: SessionId, signal: Signal) {
        self.transition(sid, signal, |_| {});
    }

    /// Run the state machine; on a transition update meta (+ unread rule),
    /// record the event, persist and broadcast. Returns false when the
    /// machine reported nothing (then nothing was persisted).
    fn transition(
        &self,
        sid: SessionId,
        signal: Signal,
        before: impl FnOnce(&mut SessionMeta),
    ) -> bool {
        let now = now_ms();
        let (applied, meta, unread_changed) = {
            let mut reg = self.reg.lock();
            let Some(entry) = reg.sessions.get_mut(&sid) else {
                return false;
            };
            before(&mut entry.meta);
            let Some(applied) = entry.machine.apply(&signal, now) else {
                return false;
            };
            entry.meta.agent = entry.machine.info().clone();
            let attention = applied.state_changed
                && entry.meta.agent.state.needs_attention()
                && entry.attached.is_empty();
            let unread_changed = attention && !entry.meta.unread;
            if attention {
                entry.meta.unread = true;
            }
            (applied, entry.meta.clone(), unread_changed)
        };
        self.record(sid, now, applied.kind, &meta.agent.state, applied.detail);
        self.persist(&meta);
        self.broadcast(Event::AgentChanged {
            session: sid,
            agent: meta.agent.clone(),
        });
        if unread_changed || !meta.is_live() {
            self.broadcast(Event::SessionUpdated(meta));
        }
        true
    }

    pub(crate) fn session_exited(&self, sid: SessionId, code: Option<i32>) {
        let at_ms = now_ms();
        let committed = self.transition(sid, Signal::Exited(code), |m| {
            m.status = SessionStatus::Dormant {
                exit_code: code,
                at_ms,
            };
        });
        // Agent already showed this exit: the status change still counts.
        if !committed {
            if let Some(meta) = self.meta(sid) {
                self.commit(meta);
            }
        }
    }

    /// A session actor panicked (`session::Actor::crashed`). Forget its
    /// handle — `actor_tx` then starts a history-only actor from the
    /// snapshot on the next request — and, if a PTY was running, record the
    /// session as Dormant like an exit without code.
    pub(crate) fn actor_crashed(&self, sid: SessionId, was_live: bool, what: &str) {
        if let Some(e) = self.reg.lock().sessions.get_mut(&sid) {
            e.actor = None;
        }
        if was_live {
            self.session_exited(sid, None);
            self.broadcast(Event::Exited {
                session: sid,
                code: None,
            });
        }
        self.broadcast(Event::Error {
            message: format!("session {sid} stopped after an internal error: {what}"),
        });
    }

    pub(crate) fn touch(&self, sid: SessionId) {
        if let Some(e) = self.reg.lock().sessions.get_mut(&sid) {
            e.meta.last_active_ms = now_ms();
        }
    }

    pub(crate) fn set_cwd(&self, sid: SessionId, path: PathBuf) {
        let changed = self.update(sid, |e| {
            if e.meta.cwd == path {
                return Err(String::new());
            }
            e.meta.cwd = path.clone();
            Ok(())
        });
        if changed.is_ok() {
            self.broadcast(Event::Cwd { session: sid, path });
        }
    }

    pub(crate) fn set_title(&self, sid: SessionId, title: String) {
        let changed = self.update(sid, |e| {
            if e.meta.title_auto == title {
                return Err(String::new());
            }
            e.meta.title_auto = title.clone();
            Ok(())
        });
        if changed.is_ok() {
            self.broadcast(Event::Title {
                session: sid,
                title,
            });
        }
    }

    pub(crate) fn set_dims(&self, sid: SessionId, dims: Dims) {
        let meta = {
            let mut reg = self.reg.lock();
            let Some(e) = reg.sessions.get_mut(&sid) else {
                return;
            };
            e.meta.cols = dims.cols;
            e.meta.rows = dims.rows;
            e.meta.clone()
        };
        self.persist(&meta);
    }
}

impl Manager {
    // -- actors ---------------------------------------------------------------

    /// The session's actor, starting a history-only one if needed.
    fn actor_tx(self: &Arc<Self>, sid: SessionId) -> Result<Sender<SessionCmd>> {
        let mut reg = self.reg.lock();
        let entry = reg.sessions.get_mut(&sid).ok_or_else(|| unknown(sid))?;
        if let Some(actor) = &entry.actor {
            return Ok(actor.tx.clone());
        }
        let handle = session::spawn_restored(self.clone(), &entry.meta, self.actor_config())
            .map_err(|e| format!("cannot start session actor: {e}"))?;
        let tx = handle.tx.clone();
        entry.actor = Some(handle);
        Ok(tx)
    }

    /// The session's actor if one runs (no spawn).
    fn existing_tx(&self, sid: SessionId) -> Result<Option<Sender<SessionCmd>>> {
        let reg = self.reg.lock();
        let entry = reg.sessions.get(&sid).ok_or_else(|| unknown(sid))?;
        Ok(entry.actor.as_ref().map(|a| a.tx.clone()))
    }

    fn pty_spawn(&self, meta: &SessionMeta, command: Vec<String>, cwd: PathBuf) -> PtySpawn {
        let mut env = meta.env.clone();
        env.push(("BERTH_SESSION_ID".into(), meta.id.to_string()));
        env.push((
            "BERTH_SOCKET".into(),
            self.paths.socket.to_string_lossy().into_owned(),
        ));
        PtySpawn {
            command,
            cwd,
            env,
            cols: meta.cols.max(2),
            rows: meta.rows.max(1),
        }
    }

    // -- workspaces -----------------------------------------------------------

    pub fn list_workspaces(&self) -> Vec<Workspace> {
        let mut list: Vec<Workspace> = self.reg.lock().workspaces.values().cloned().collect();
        list.sort_by_key(|w| (w.order, w.created_at_ms));
        list
    }

    pub fn create_workspace(&self, name: String, root: PathBuf) -> Result<Workspace> {
        if name.trim().is_empty() {
            return Err("workspace name is empty".into());
        }
        let ws = {
            let mut reg = self.reg.lock();
            let order = reg
                .workspaces
                .values()
                .map(|w| w.order.saturating_add(1))
                .max()
                .unwrap_or(0);
            let ws = Workspace {
                id: WorkspaceId::new(),
                name,
                root,
                color: None,
                order,
                created_at_ms: now_ms(),
            };
            reg.workspaces.insert(ws.id, ws.clone());
            ws
        };
        if let Err(e) = self.store.upsert_workspace(&ws) {
            self.reg.lock().workspaces.remove(&ws.id);
            return Err(format!("cannot persist workspace: {e}"));
        }
        self.broadcast(Event::WorkspaceUpdated(ws.clone()));
        Ok(ws)
    }

    pub fn rename_workspace(&self, id: WorkspaceId, name: String) -> Result<Workspace> {
        let ws = {
            let mut reg = self.reg.lock();
            let ws = reg.workspaces.get_mut(&id).ok_or("unknown workspace")?;
            ws.name = name;
            ws.clone()
        };
        self.store
            .upsert_workspace(&ws)
            .map_err(|e| e.to_string())?;
        self.broadcast(Event::WorkspaceUpdated(ws.clone()));
        Ok(ws)
    }

    /// Refuses while sessions still belong to the workspace (deleting them
    /// would purge history; the client must do that explicitly).
    pub fn delete_workspace(&self, id: WorkspaceId) -> Result<()> {
        {
            let mut reg = self.reg.lock();
            if !reg.workspaces.contains_key(&id) {
                return Err("unknown workspace".into());
            }
            if reg.sessions.values().any(|e| e.meta.workspace == id) {
                return Err("workspace still has sessions".into());
            }
            reg.workspaces.remove(&id);
        }
        self.store.delete_workspace(id).map_err(|e| e.to_string())?;
        self.broadcast(Event::WorkspaceRemoved(id));
        Ok(())
    }

    // -- sessions -------------------------------------------------------------

    pub fn list_sessions(&self) -> Vec<SessionMeta> {
        let reg = self.reg.lock();
        let ws_order = |id: &WorkspaceId| reg.workspaces.get(id).map_or(u32::MAX, |w| w.order);
        let mut list: Vec<SessionMeta> = reg.sessions.values().map(|e| e.meta.clone()).collect();
        list.sort_by_key(|m| (ws_order(&m.workspace), m.order, m.created_at_ms));
        list
    }

    pub async fn create_session(
        self: &Arc<Self>,
        workspace: WorkspaceId,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
        title: Option<String>,
        dims: Dims,
    ) -> Result<SessionMeta> {
        let now = now_ms();
        let meta = {
            let reg = self.reg.lock();
            let ws = reg.workspaces.get(&workspace).ok_or("unknown workspace")?;
            let order = reg
                .sessions
                .values()
                .filter(|e| e.meta.workspace == workspace)
                .map(|e| e.meta.order.saturating_add(1))
                .max()
                .unwrap_or(0);
            let dims = if dims.cols < 2 || dims.rows < 1 {
                Dims { cols: 80, rows: 24 }
            } else {
                dims
            };
            SessionMeta {
                id: SessionId::new(),
                workspace,
                title_auto: String::new(),
                title_user: title.filter(|t| !t.is_empty()),
                cwd: cwd.unwrap_or_else(|| ws.root.clone()),
                command: command.unwrap_or_default(),
                env: Vec::new(),
                status: SessionStatus::Live,
                agent: AgentInfo {
                    since_ms: now,
                    confidence: HEURISTIC_CONFIDENCE,
                    ..Default::default()
                },
                created_at_ms: now,
                last_active_ms: now,
                unread: false,
                persist: PersistPolicy {
                    snapshot: true,
                    journal: self.config.persist.journal,
                },
                order,
                cols: dims.cols,
                rows: dims.rows,
            }
        };
        let spawn = self.pty_spawn(&meta, meta.command.clone(), meta.cwd.clone());
        self.reg
            .lock()
            .sessions
            .insert(meta.id, Entry::new(meta.clone()));
        let started = match session::spawn_live(self.clone(), &meta, spawn, self.actor_config()) {
            Ok((handle, init)) => {
                if let Some(e) = self.reg.lock().sessions.get_mut(&meta.id) {
                    e.actor = Some(handle);
                }
                init.await.unwrap_or_else(|_| Err(stopped()))
            }
            Err(e) => Err(format!("cannot start session thread: {e}")),
        };
        if let Err(e) = started {
            let entry = self.reg.lock().sessions.remove(&meta.id);
            if let Some(join) = entry.and_then(|e| e.actor).and_then(|a| a.join) {
                let _ = tokio::task::spawn_blocking(move || join.join()).await;
            }
            return Err(format!(
                "cannot start {:?} in {}: {e}",
                meta.command,
                meta.cwd.display()
            ));
        }
        tracing::info!(session = %meta.id, cwd = %meta.cwd.display(), "session created");
        Ok(self.commit(meta))
    }

    pub fn attach(
        self: &Arc<Self>,
        conn: ConnId,
        outbox: Arc<Outbox>,
        sid: SessionId,
        dims: Dims,
        reply_to: u32,
    ) -> Result<()> {
        let tx = self.actor_tx(sid)?;
        if let Some(e) = self.reg.lock().sessions.get_mut(&sid) {
            e.attached.insert(conn);
        }
        tx.send(SessionCmd::Attach {
            conn,
            outbox,
            dims,
            reply_to: Some(reply_to),
        })
        .map_err(|_| stopped())
    }

    pub fn detach(&self, conn: ConnId, sid: SessionId) -> Result<()> {
        if let Some(e) = self.reg.lock().sessions.get_mut(&sid) {
            e.attached.remove(&conn);
        }
        if let Some(tx) = self.existing_tx(sid)? {
            tx.send(SessionCmd::Detach { conn })
                .map_err(|_| stopped())?;
        }
        Ok(())
    }

    pub fn resize(&self, conn: ConnId, sid: SessionId, dims: Dims) -> Result<()> {
        if let Some(tx) = self.existing_tx(sid)? {
            tx.send(SessionCmd::Resize { conn, dims })
                .map_err(|_| stopped())?;
        }
        Ok(())
    }

    pub fn input(&self, sid: SessionId, data: Vec<u8>) -> Result<()> {
        let tx = {
            let reg = self.reg.lock();
            let e = reg.sessions.get(&sid).ok_or_else(|| unknown(sid))?;
            if !e.meta.is_live() {
                return Err("session is not live".into());
            }
            e.actor.as_ref().map(|a| a.tx.clone())
        };
        tx.ok_or_else(stopped)?
            .send(SessionCmd::Input(data))
            .map_err(|_| stopped())
    }

    pub async fn fetch_lines(
        self: &Arc<Self>,
        sid: SessionId,
        start: u64,
        count: u32,
    ) -> Result<Event> {
        let tx = self.actor_tx(sid)?;
        let (reply, rx) = oneshot::channel();
        tx.send(SessionCmd::FetchLines {
            start,
            count,
            reply,
        })
        .map_err(|_| stopped())?;
        rx.await.map_err(|_| stopped())
    }

    pub fn subscribe(
        self: &Arc<Self>,
        conn: ConnId,
        outbox: Arc<Outbox>,
        sid: SessionId,
        mode: SubscribeMode,
        reply_to: u32,
    ) -> Result<()> {
        let tx = self.actor_tx(sid)?;
        tx.send(SessionCmd::Subscribe {
            conn,
            outbox,
            mode,
            reply_to: Some(reply_to),
        })
        .map_err(|_| stopped())
    }

    pub fn unsubscribe(&self, conn: ConnId, sid: SessionId) -> Result<()> {
        if let Some(e) = self.reg.lock().sessions.get_mut(&sid) {
            e.attached.remove(&conn);
        }
        if let Some(tx) = self.existing_tx(sid)? {
            tx.send(SessionCmd::Unsubscribe { conn })
                .map_err(|_| stopped())?;
        }
        Ok(())
    }

    pub fn kill(&self, sid: SessionId) -> Result<()> {
        let tx = {
            let reg = self.reg.lock();
            let e = reg.sessions.get(&sid).ok_or_else(|| unknown(sid))?;
            if !e.meta.is_live() {
                return Err("session is not live".into());
            }
            e.actor.as_ref().map(|a| a.tx.clone())
        };
        tx.ok_or_else(stopped)?
            .send(SessionCmd::Kill)
            .map_err(|_| stopped())
    }
}

impl Manager {
    /// New PTY under the session's history, in the original cwd: `Shell`
    /// starts a shell; `ResumeAgent` executes the agent's resume command
    /// directly (see `revive_argv`). When the agent exits the session goes
    /// Dormant; `Revive { Shell }` then continues in a shell.
    pub async fn revive(self: &Arc<Self>, sid: SessionId, mode: ReviveMode) -> Result<SessionMeta> {
        let meta = self.meta(sid).ok_or_else(|| unknown(sid))?;
        if meta.is_live() {
            return Err("session is live".into());
        }
        let command = revive_argv(&self.config, &meta, mode)?;
        let root = self
            .reg
            .lock()
            .workspaces
            .get(&meta.workspace)
            .map(|w| w.root.clone());
        let cwd = usable_cwd(&meta.cwd, root.as_deref());
        let spawn = self.pty_spawn(&meta, command, cwd.clone());
        let tx = self.actor_tx(sid)?;
        // Live *before* the PTY starts (registry only, committed below): the
        // actor reports the new child's exit only after it started, so an
        // agent that exits within milliseconds (a stale resume id) is never
        // overwritten with Live — and a concurrent revive sees "live".
        let now = now_ms();
        let revived = {
            let mut reg = self.reg.lock();
            let e = reg.sessions.get_mut(&sid).ok_or_else(|| unknown(sid))?;
            if e.meta.is_live() {
                return Err("session is live".into());
            }
            e.meta.status = SessionStatus::Live;
            e.meta.cwd = cwd;
            e.meta.last_active_ms = now;
            let mut agent = e.meta.agent.clone();
            agent.state = AgentState::Idle;
            agent.since_ms = now;
            agent.source = StateSource::Heuristic;
            agent.confidence = HEURISTIC_CONFIDENCE;
            e.machine.reset(agent.clone());
            e.meta.agent = agent.clone();
            agent
        };
        let (reply, rx) = oneshot::channel();
        let started = match tx.send(SessionCmd::Revive { spawn, reply }) {
            Ok(()) => rx.await.unwrap_or_else(|_| Err(stopped())),
            Err(_) => Err(stopped()),
        };
        if let Err(err) = started {
            // Nothing started: put the previous state back.
            let _ = self.update(sid, |e| {
                e.meta.status = meta.status.clone();
                e.meta.cwd = meta.cwd.clone();
                e.meta.last_active_ms = meta.last_active_ms;
                e.machine.reset(meta.agent.clone());
                e.meta.agent = meta.agent.clone();
                Ok(())
            });
            return Err(err);
        }
        let kind = match mode {
            ReviveMode::Shell => "revive:shell",
            ReviveMode::ResumeAgent => "revive:resume",
        };
        self.record(sid, now, kind.into(), &revived.state, None);
        // Current state, which already includes an exit that raced us.
        let meta = self.commit(self.meta(sid).ok_or_else(|| unknown(sid))?);
        self.broadcast(Event::AgentChanged {
            session: sid,
            agent: meta.agent.clone(),
        });
        Ok(meta)
    }

    /// Stop the actor (no snapshot), purge metadata/events/snapshot/journal.
    pub async fn delete_session(&self, sid: SessionId) -> Result<()> {
        let actor = {
            let mut reg = self.reg.lock();
            reg.sessions
                .get_mut(&sid)
                .ok_or_else(|| unknown(sid))?
                .actor
                .take()
        };
        if let Some(ActorHandle { tx, join }) = actor {
            let (reply, rx) = oneshot::channel();
            if tx.send(SessionCmd::Remove { reply }).is_ok() {
                let _ = rx.await;
            }
            if let Some(join) = join {
                let _ = tokio::task::spawn_blocking(move || join.join()).await;
            }
        }
        self.reg.lock().sessions.remove(&sid);
        self.store.purge_session(sid).map_err(|e| e.to_string())?;
        self.broadcast(Event::SessionRemoved(sid));
        Ok(())
    }

    pub fn mark_read(&self, sid: SessionId) -> Result<SessionMeta> {
        self.update(sid, |e| {
            e.meta.unread = false;
            Ok(())
        })
    }

    pub fn rename_session(&self, sid: SessionId, title: Option<String>) -> Result<SessionMeta> {
        self.update(sid, |e| {
            e.meta.title_user = title.filter(|t| !t.is_empty());
            Ok(())
        })
    }

    pub fn move_session(
        &self,
        sid: SessionId,
        workspace: WorkspaceId,
        order: u32,
    ) -> Result<SessionMeta> {
        if !self.reg.lock().workspaces.contains_key(&workspace) {
            return Err("unknown workspace".into());
        }
        self.update(sid, |e| {
            e.meta.workspace = workspace;
            e.meta.order = order;
            Ok(())
        })
    }

    pub fn set_persist(&self, sid: SessionId, policy: PersistPolicy) -> Result<SessionMeta> {
        let meta = self.update(sid, |e| {
            e.meta.persist = policy;
            Ok(())
        })?;
        match self.existing_tx(sid)? {
            Some(tx) => tx
                .send(SessionCmd::SetPersist(policy))
                .map_err(|_| stopped())?,
            None if !policy.snapshot => {
                self.store.delete_snapshot(sid).map_err(|e| e.to_string())?
            }
            None => {}
        }
        Ok(meta)
    }

    pub fn handle_hook(&self, envelope: HookEnvelope) {
        let target = {
            let reg = self.reg.lock();
            hooks::resolve_target(&envelope, reg.sessions.values().map(|e| &e.meta))
        };
        let Some(sid) = target else {
            return;
        };
        match envelope.signal {
            AgentSignal::Claude(hook) => {
                // Resume needs Claude's own cwd (it keys transcripts by cwd).
                let cwd = match (&hook.event, &hook.cwd) {
                    (ClaudeHookEvent::SessionStart { .. }, Some(cwd)) => Some(cwd.clone()),
                    _ => None,
                };
                self.transition(sid, Signal::Hook(hook), |m| {
                    if let Some(cwd) = cwd {
                        m.cwd = cwd;
                    }
                });
            }
            AgentSignal::Codex(notify) => {
                self.transition(sid, Signal::Codex(notify), |_| {});
            }
            AgentSignal::Statusline(update) => {
                let meta = {
                    let mut reg = self.reg.lock();
                    let Some(e) = reg.sessions.get_mut(&sid) else {
                        return;
                    };
                    if !e.machine.apply_statusline(&update) {
                        return;
                    }
                    e.meta.agent = e.machine.info().clone();
                    e.meta.clone()
                };
                self.persist(&meta);
                self.broadcast(Event::AgentChanged {
                    session: sid,
                    agent: meta.agent,
                });
            }
        }
    }

    pub fn status(&self) -> DaemonStatus {
        let reg = self.reg.lock();
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            uptime_ms: now_ms() - self.started_ms,
            sessions_live: reg.sessions.values().filter(|e| e.meta.is_live()).count() as u32,
            sessions_total: reg.sessions.len() as u32,
        }
    }

    pub fn request_shutdown(&self) {
        self.stop_tx.send_replace(true);
    }

    /// Every actor writes its final snapshot and stops; metadata is flushed.
    pub async fn shutdown(&self) {
        let actors: Vec<(SessionId, ActorHandle)> = {
            let mut reg = self.reg.lock();
            reg.sessions
                .iter_mut()
                .filter_map(|(id, e)| e.actor.take().map(|a| (*id, a)))
                .collect()
        };
        let mut waits = Vec::new();
        for (sid, actor) in actors {
            let (reply, rx) = oneshot::channel();
            if actor.tx.send(SessionCmd::Shutdown { reply }).is_ok() {
                waits.push((sid, rx, actor.join));
            }
        }
        for (sid, rx, join) in waits {
            match tokio::time::timeout(SHUTDOWN_WAIT, rx).await {
                Ok(_) => {
                    if let Some(join) = join {
                        let _ = tokio::task::spawn_blocking(move || join.join()).await;
                    }
                }
                Err(_) => tracing::warn!(session = %sid, "session did not stop in time"),
            }
        }
        let metas: Vec<SessionMeta> = self
            .reg
            .lock()
            .sessions
            .values()
            .map(|e| e.meta.clone())
            .collect();
        for meta in &metas {
            self.persist(meta);
        }
        tracing::info!(sessions = metas.len(), "sessions saved");
    }
}

/// Reuse the session's original command when it is a plain shell (e.g.
/// `/bin/sh`); anything else revives into the login shell.
fn revive_command(original: &[String]) -> Vec<String> {
    let is_shell = original.first().is_some_and(|prog| {
        let base = Path::new(prog)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(prog);
        SHELLS.contains(&base.trim_start_matches('-'))
    });
    if is_shell {
        original.to_vec()
    } else {
        Vec::new()
    }
}

/// argv of a revive. `Shell`: the original plain shell, else the login
/// shell. `ResumeAgent`: the agent's resume command, executed directly —
/// never typed into or parsed by a shell — with the word `{id}` replaced by
/// the external id, which must be a plain token (ids stored before that
/// check existed are refused here too).
fn revive_argv(config: &Config, meta: &SessionMeta, mode: ReviveMode) -> Result<Vec<String>> {
    match mode {
        ReviveMode::Shell => Ok(revive_command(&meta.command)),
        ReviveMode::ResumeAgent => {
            let id = meta
                .agent
                .external_id
                .as_deref()
                .ok_or("no agent session id to resume")?;
            if !is_valid_external_id(id) {
                return Err("agent session id is not a plain token; refusing to resume".into());
            }
            config.resume_argv(&meta.agent.kind, id)
        }
    }
}

fn usable_cwd(cwd: &Path, workspace_root: Option<&Path>) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let found = [Some(cwd), workspace_root, home.as_deref()]
        .into_iter()
        .flatten()
        .find(|p| p.is_dir())
        .map(Path::to_path_buf);
    found.unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_core::AgentKind;

    #[test]
    fn revive_reuses_plain_shells_only() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(revive_command(&v(&["/bin/sh"])), v(&["/bin/sh"]));
        assert_eq!(revive_command(&v(&["zsh", "-l"])), v(&["zsh", "-l"]));
        assert!(revive_command(&v(&["claude"])).is_empty());
        assert!(revive_command(&[]).is_empty());
    }

    /// Review high #1: `ResumeAgent` runs the agent's argv itself, no shell.
    #[test]
    fn resume_agent_argv_is_exec_ready_and_shell_free() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut meta = SessionMeta {
            id: SessionId::new(),
            workspace: WorkspaceId::new(),
            title_auto: String::new(),
            title_user: None,
            cwd: PathBuf::from("/"),
            command: v(&["/bin/zsh"]),
            env: Vec::new(),
            status: SessionStatus::Restored,
            agent: AgentInfo::default(),
            created_at_ms: 0,
            last_active_ms: 0,
            unread: false,
            persist: PersistPolicy {
                snapshot: true,
                journal: false,
            },
            order: 0,
            cols: 80,
            rows: 24,
        };
        let config = Config::default();
        meta.agent.kind = AgentKind::Claude;
        meta.agent.external_id = Some("0f8c2e1a-1111-2222-3333-444455556666".into());
        let argv = revive_argv(&config, &meta, ReviveMode::ResumeAgent).unwrap();
        assert_eq!(
            argv,
            v(&["claude", "--resume", "0f8c2e1a-1111-2222-3333-444455556666"])
        );
        assert!(!SHELLS.contains(&argv[0].as_str()));
        meta.agent.kind = AgentKind::Codex;
        assert_eq!(
            revive_argv(&config, &meta, ReviveMode::ResumeAgent).unwrap(),
            v(&["codex", "resume", "0f8c2e1a-1111-2222-3333-444455556666"])
        );
        // A hostile id stored by an older daemon is refused, not quoted.
        meta.agent.external_id = Some("\u{15}touch /tmp/x #".into());
        assert!(revive_argv(&config, &meta, ReviveMode::ResumeAgent).is_err());
        meta.agent.external_id = None;
        assert!(revive_argv(&config, &meta, ReviveMode::ResumeAgent).is_err());
        assert_eq!(
            revive_argv(&config, &meta, ReviveMode::Shell).unwrap(),
            v(&["/bin/zsh"])
        );
    }

    /// Review high #2: a panicking actor leaves a Dormant session that a
    /// fresh history-only actor keeps serving.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_panic_leaves_a_usable_session() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(dir.path());
        paths.ensure_dirs().unwrap();
        let store = Store::open(&paths).unwrap();
        let (stop_tx, _stop_rx) = watch::channel(false);
        let mgr = Manager::start(paths, Config::default(), store, stop_tx).unwrap();
        let ws = mgr
            .create_workspace("w".into(), dir.path().to_path_buf())
            .unwrap();
        let dims = Dims { cols: 80, rows: 24 };
        let sid = mgr
            .create_session(ws.id, None, Some(vec!["/bin/sh".into()]), None, dims)
            .await
            .unwrap()
            .id;
        let events = Outbox::new();
        let conn = mgr.register_conn(ClientRole::Gui, events.clone());
        let tx = mgr.existing_tx(sid).unwrap().unwrap();
        tx.send(SessionCmd::Crash).unwrap();

        let (mut exited, mut error) = (false, false);
        while !(exited && error) {
            let batch = tokio::time::timeout(Duration::from_secs(10), events.next_batch())
                .await
                .expect("crash announced")
                .expect("outbox open");
            for msg in batch {
                match msg.event {
                    Event::Exited { session, code } if session == sid => {
                        assert_eq!(code, None);
                        exited = true;
                    }
                    Event::Error { message } if message.contains("internal error") => error = true,
                    _ => {}
                }
            }
        }
        assert!(
            mgr.existing_tx(sid).unwrap().is_none(),
            "dead actor forgotten"
        );
        assert!(matches!(
            mgr.meta(sid).unwrap().status,
            SessionStatus::Dormant {
                exit_code: None,
                ..
            }
        ));

        // A fresh actor serves FetchLines, Attach, Revive and Delete.
        assert!(matches!(
            mgr.fetch_lines(sid, 0, 10).await.unwrap(),
            Event::Lines { .. }
        ));
        let screen = Outbox::new();
        mgr.attach(conn, screen.clone(), sid, dims, 7).unwrap();
        let batch = tokio::time::timeout(Duration::from_secs(5), screen.next_batch())
            .await
            .expect("attach answered")
            .expect("outbox open");
        assert!(batch
            .iter()
            .any(|m| m.reply_to == Some(7) && matches!(m.event, Event::Screen(_))));
        assert!(mgr.revive(sid, ReviveMode::Shell).await.unwrap().is_live());
        mgr.delete_session(sid).await.unwrap();
        assert!(mgr.meta(sid).is_none());
        mgr.shutdown().await;
    }

    #[test]
    fn usable_cwd_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(usable_cwd(dir.path(), None), dir.path());
        let gone = dir.path().join("gone");
        assert_eq!(usable_cwd(&gone, Some(dir.path())), dir.path());
    }
}
