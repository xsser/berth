//! Session actor: one std thread per session owning `PtyHandle` +
//! `Terminal`. The loop selects over PTY output, commands and deadlines
//! (4 ms render batch, preview throttle, 5 s snapshot, silence / foreground
//! heuristics, kill escalation) — no fixed-rate ticker, so idle sessions
//! don't wake up needlessly.
//!
//! PTY input (and the terminal's replies) goes through a writer thread per
//! PTY behind a byte budget (`MAX_PENDING_INPUT`): a child that does not read
//! its input blocks that thread, never the actor, which keeps rendering,
//! answering and killing.
//!
//! Virtual line space (`FetchLines`, `history_len`):
//! `[restored prefix 0..R) ++ [live scrollback) ++ [screen rows)`. The prefix
//! is what earlier lives left behind (loaded lazily from the snapshot for
//! restored sessions, or folded from the old terminal on revive); style ids
//! stay valid across lives because the new terminal inherits the interner.

use std::any::Any;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use berth_core::{
    now_ms, CursorState, DaemonMsg, Dims, Event, LineSnapshot, PersistPolicy, ScreenUpdate,
    SessionId, SessionMeta, SessionSnapshotFile, StyleInterner, StyleTable, SubscribeMode,
    TermModes, SNAPSHOT_FORMAT_VERSION,
};
use berth_store::JournalWriter;
use berth_vt::{
    Damage, OscEvent, ProcessOutcome, PtyHandle, PtyOutput, PtySpawn, TermEvent, Terminal,
    TerminalConfig, KILL_GRACE,
};
use crossbeam_channel::{select, Receiver, Sender};
use parking_lot::RwLock;
use tokio::sync::oneshot;

use crate::agent_state::{Signal, SILENCE_IDLE_SECS};
use crate::manager::{is_shell_program, Manager};
use crate::outbox::Outbox;
use crate::view::{
    cap_front, prefix_from_snapshot, referenced_styles, trim_trailing_blank, ConnId, SubKind,
    Subscriber,
};

/// Output is coalesced this long before screen deltas go out (≤ 250 Hz).
pub const BATCH: Duration = Duration::from_millis(4);
const SILENCE: Duration = Duration::from_secs(SILENCE_IDLE_SECS);
const FOREGROUND_POLL: Duration = Duration::from_secs(1);
const ACTIVITY_MIN_INTERVAL: Duration = Duration::from_millis(500);
/// Output right after user input is most likely the echo of that input.
const ECHO_WINDOW: Duration = Duration::from_millis(250);
/// `try_wait` poll interval while a `Kill` is pending (SIGKILL follows after
/// berth-vt's `KILL_GRACE`).
const KILL_POLL: Duration = Duration::from_millis(50);
/// A reaped child normally produces PTY EOF at once (macOS revokes the tty
/// when the session leader exits). Where it does not (a background job keeps
/// a Linux pty open), finish once the output has been quiet this long.
const EOF_GRACE: Duration = Duration::from_millis(500);
const EXIT_CODE_WAIT: Duration = Duration::from_millis(500);
const IDLE_WAKE: Duration = Duration::from_secs(3600);
/// Upper bound of one `FetchLines` reply.
pub const MAX_FETCH_LINES: u32 = 5_000;

pub(crate) enum SessionCmd {
    Input(Vec<u8>),
    Attach {
        conn: ConnId,
        outbox: Arc<Outbox>,
        dims: Dims,
        reply_to: Option<u32>,
    },
    Detach {
        conn: ConnId,
    },
    Resize {
        conn: ConnId,
        dims: Dims,
    },
    Subscribe {
        conn: ConnId,
        outbox: Arc<Outbox>,
        mode: SubscribeMode,
        reply_to: Option<u32>,
    },
    Unsubscribe {
        conn: ConnId,
    },
    ConnClosed {
        conn: ConnId,
    },
    FetchLines {
        start: u64,
        count: u32,
        reply: oneshot::Sender<Event>,
    },
    Kill,
    Revive {
        spawn: PtySpawn,
        reply: oneshot::Sender<Result<(), String>>,
    },
    SetPersist(PersistPolicy),
    /// Graceful stop: final snapshot, then SIGHUP the child.
    Shutdown {
        reply: oneshot::Sender<()>,
    },
    /// Stop without snapshot (session is being deleted).
    Remove {
        reply: oneshot::Sender<()>,
    },
    /// Test-only: panic on the actor thread.
    #[cfg(test)]
    Crash,
    /// Test hook: block the actor thread (a session that cannot stop).
    #[cfg(test)]
    Stall(Duration),
}

#[derive(Clone, Debug)]
pub(crate) struct ActorConfig {
    pub scrollback: usize,
    pub snapshot_interval: Duration,
    pub max_restored_lines: usize,
    /// `[terminal] osc52_store`.
    pub osc52_store: bool,
}

pub(crate) struct ActorHandle {
    pub tx: Sender<SessionCmd>,
    pub join: Option<JoinHandle<()>>,
    pub input: Arc<InputBudget>,
}

/// Cap on PTY input queued for a session: accepted by `Manager::input` but
/// not yet written by the PTY's writer thread. Beyond it input is refused
/// (backpressure) instead of piling up behind a child that does not read.
/// One message is always admitted into an empty queue, so a single paste
/// larger than the cap still goes through (memory per session: cap + one
/// frame). Terminal replies share the budget and are dropped when it is full.
pub const MAX_PENDING_INPUT: usize = 1024 * 1024;

/// Input bytes accepted for a session but not yet written to (or discarded
/// by) its PTY writer.
#[derive(Default)]
pub(crate) struct InputBudget {
    pending: AtomicUsize,
    dropping: AtomicBool,
}

impl InputBudget {
    /// Account for `n` more bytes; `false` if they exceed the cap.
    pub fn reserve(&self, n: usize) -> bool {
        let mut cur = self.pending.load(Ordering::Acquire);
        loop {
            if cur != 0 && cur.saturating_add(n) > MAX_PENDING_INPUT {
                return false;
            }
            match self.pending.compare_exchange_weak(
                cur,
                cur.saturating_add(n),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(now) => cur = now,
            }
        }
    }

    /// Whether this is the first drop since the queue last drained (warn
    /// once per episode).
    pub fn first_drop(&self) -> bool {
        !self.dropping.swap(true, Ordering::AcqRel)
    }

    /// `n` reserved bytes were written (or discarded) by the writer.
    pub fn release(&self, n: usize) {
        if self.pending.fetch_sub(n, Ordering::AcqRel) == n {
            self.dropping.store(false, Ordering::Release);
        }
    }

    #[cfg(test)]
    fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }
}

struct Live {
    /// Shared with the writer thread, which holds a read lock (and a strong
    /// reference) only while a write is in flight. The actor's `&mut`
    /// operations (`try_wait`, `kill`, `force_kill`) use `try_write` and
    /// never wait for a write; `resize` / `foreground_process` take a read
    /// lock, which a write in flight does not block.
    pty: Arc<RwLock<PtyHandle>>,
    /// The child's pid, which is also its process group (portable-pty
    /// `setsid`s the child).
    pid: u32,
    rx: Receiver<PtyOutput>,
    /// Queue of the writer thread (bounded by the session's `InputBudget`).
    writer: Sender<Vec<u8>>,
    /// Set when the PTY is let go: the writer discards what is still queued.
    closed: Arc<AtomicBool>,
}

pub(crate) struct Actor {
    id: SessionId,
    mgr: Arc<Manager>,
    cfg: ActorConfig,
    persist: PersistPolicy,
    live: Option<Live>,
    term: Option<Terminal>,
    /// Generation of the GUI's colors `term` answers color queries with
    /// (`Manager::term_colors_since`; 0: none, a new terminal's state).
    term_colors_seen: u64,
    prefix: Vec<LineSnapshot>,
    /// Style table of the restored prefix while no terminal exists.
    restored_styles: StyleInterner,
    subs: Vec<Subscriber>,
    attached_dims: HashMap<ConnId, Dims>,
    dims: Dims,
    seq: u64,
    screen_dirty: bool,
    flush_at: Option<Instant>,
    snap_dirty: bool,
    snapshot_written: bool,
    last_snapshot: Instant,
    last_output: Option<Instant>,
    last_input: Option<Instant>,
    last_input_signal: Option<Instant>,
    last_activity_signal: Option<Instant>,
    silence_reported: bool,
    fg_name: Option<String>,
    /// Whether the last foreground poll found the session's own shell in
    /// the foreground, as last reported to the manager (archive scan).
    shell_in_foreground: Option<bool>,
    next_fg_poll: Instant,
    /// `Kill` sent SIGHUP at this instant; `try_wait` is polled every
    /// `KILL_POLL` until the child is gone.
    kill_since: Option<Instant>,
    kill_forced: bool,
    next_kill_poll: Instant,
    /// `try_wait` reaped the child but PTY EOF has not been seen yet.
    reaped_at: Option<Instant>,
    child_exit_code: Option<i32>,
    journal: Option<JournalWriter>,
    stopped: bool,
    /// Shared with `ActorHandle::input`; released after each write.
    input_budget: Arc<InputBudget>,
}

/// Start a session with a fresh PTY. The receiver yields the spawn result.
pub(crate) fn spawn_live(
    mgr: Arc<Manager>,
    meta: &SessionMeta,
    spawn: PtySpawn,
    cfg: ActorConfig,
) -> std::io::Result<(ActorHandle, oneshot::Receiver<Result<(), String>>)> {
    let (init_tx, init_rx) = oneshot::channel();
    let actor = Actor::new(mgr, meta, cfg);
    let handle = start_thread(actor, move |actor| match actor.start_pty(&spawn) {
        Ok(()) => {
            let _ = init_tx.send(Ok(()));
            true
        }
        Err(e) => {
            let _ = init_tx.send(Err(e));
            false
        }
    })?;
    Ok((handle, init_rx))
}

/// Start an actor for a session that has no PTY (restored / dormant from a
/// previous daemon life); its snapshot is loaded on the actor thread.
pub(crate) fn spawn_restored(
    mgr: Arc<Manager>,
    meta: &SessionMeta,
    cfg: ActorConfig,
) -> std::io::Result<ActorHandle> {
    let actor = Actor::new(mgr, meta, cfg);
    start_thread(actor, |actor| {
        actor.load_restored();
        true
    })
}

fn start_thread(
    mut actor: Actor,
    init: impl FnOnce(&mut Actor) -> bool + Send + 'static,
) -> std::io::Result<ActorHandle> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let input = actor.input_budget.clone();
    let join = std::thread::Builder::new()
        .name(format!("session-{}", actor.id.short()))
        .spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                if init(&mut actor) {
                    actor.run(&rx);
                }
            }));
            if let Err(panic) = outcome {
                actor.crashed(panic_message(panic.as_ref()));
            }
        })?;
    Ok(ActorHandle {
        tx,
        join: Some(join),
        input,
    })
}

impl Actor {
    fn new(mgr: Arc<Manager>, meta: &SessionMeta, cfg: ActorConfig) -> Actor {
        let now = Instant::now();
        Actor {
            id: meta.id,
            mgr,
            cfg,
            persist: meta.persist,
            live: None,
            term: None,
            term_colors_seen: 0,
            prefix: Vec::new(),
            restored_styles: StyleInterner::new(),
            subs: Vec::new(),
            attached_dims: HashMap::new(),
            dims: sane_dims(Dims {
                cols: meta.cols,
                rows: meta.rows,
            }),
            seq: 0,
            screen_dirty: false,
            flush_at: None,
            snap_dirty: false,
            snapshot_written: false,
            last_snapshot: now,
            last_output: None,
            last_input: None,
            last_input_signal: None,
            last_activity_signal: None,
            silence_reported: true,
            fg_name: None,
            shell_in_foreground: None,
            next_fg_poll: now,
            kill_since: None,
            kill_forced: false,
            next_kill_poll: now,
            reaped_at: None,
            child_exit_code: None,
            journal: None,
            stopped: false,
            input_budget: Arc::default(),
        }
    }

    fn load_restored(&mut self) {
        match self.mgr.store.read_snapshot(self.id) {
            Ok(Some(snap)) => {
                if snap.format_version != SNAPSHOT_FORMAT_VERSION {
                    // Left by an older berthd (the one a restart replaced).
                    tracing::info!(
                        session = %self.id,
                        format = snap.format_version,
                        "snapshot read in an older format"
                    );
                }
                let (prefix, table) = prefix_from_snapshot(snap, self.cfg.max_restored_lines);
                self.prefix = prefix;
                self.restored_styles = StyleInterner::from_table(table);
                self.snapshot_written = true;
            }
            Ok(None) => tracing::debug!(session = %self.id, "no snapshot to restore"),
            Err(e) => tracing::warn!(session = %self.id, error = %e, "cannot load snapshot"),
        }
    }

    /// Spawn the child and a fresh terminal below the existing history.
    fn start_pty(&mut self, spawn: &PtySpawn) -> Result<(), String> {
        let (pty, rx) = PtyHandle::spawn(spawn).map_err(|e| e.to_string())?;
        let pid = pty.child_pid();
        let pty = Arc::new(RwLock::new(pty));
        let closed = Arc::new(AtomicBool::new(false));
        let writer = spawn_writer(
            self.id,
            Arc::downgrade(&pty),
            closed.clone(),
            self.input_budget.clone(),
        )
        .map_err(|e| format!("cannot start the pty writer thread: {e}"))?;
        let interner = match self.term.take() {
            Some(old) => self.fold_into_prefix(old),
            None => std::mem::replace(&mut self.restored_styles, StyleInterner::new()),
        };
        self.dims = sane_dims(Dims {
            cols: spawn.cols,
            rows: spawn.rows,
        });
        let mut term = Terminal::new(TerminalConfig {
            cols: self.dims.cols,
            rows: self.dims.rows,
            scrollback_lines: self.cfg.scrollback,
            kitty_keyboard: true,
        });
        *term.interner() = interner;
        term.set_clipboard_store_allowed(self.cfg.osc52_store);
        self.term = Some(term);
        self.term_colors_seen = 0;
        self.live = Some(Live {
            pty,
            pid,
            rx,
            writer,
            closed,
        });
        self.child_exit_code = None;
        self.clear_exit_tracking();
        self.fg_name = None;
        self.shell_in_foreground = None;
        self.next_fg_poll = Instant::now() + FOREGROUND_POLL;
        self.snap_dirty = true;
        if self.persist.journal {
            self.open_journal();
        }
        Ok(())
    }

    /// Append an old terminal's scrollback + screen to the prefix and hand
    /// back its interner (its ids are what those lines reference).
    fn fold_into_prefix(&mut self, mut old: Terminal) -> StyleInterner {
        let hl = old.history_len();
        let history = old.history(0, hl);
        let mut screen = old.screen().lines;
        trim_trailing_blank(&mut screen);
        self.prefix.extend(history);
        self.prefix.extend(screen);
        cap_front(&mut self.prefix, self.cfg.max_restored_lines);
        std::mem::replace(old.interner(), StyleInterner::new())
    }

    fn open_journal(&mut self) {
        match self.mgr.store.open_journal(self.id) {
            Ok(j) => self.journal = Some(j),
            Err(e) => tracing::warn!(session = %self.id, error = %e, "cannot open journal"),
        }
    }

    fn run(&mut self, rx: &Receiver<SessionCmd>) {
        let never = crossbeam_channel::never();
        while !self.stopped {
            self.on_timers(Instant::now());
            let timeout = self
                .next_deadline()
                .map(|d| d.saturating_duration_since(Instant::now()))
                .unwrap_or(IDLE_WAKE);
            let pty_rx = self
                .live
                .as_ref()
                .map(|l| l.rx.clone())
                .unwrap_or_else(|| never.clone());
            select! {
                recv(rx) -> cmd => match cmd {
                    Ok(cmd) => self.handle_cmd(cmd),
                    // Manager is gone: behave like a graceful shutdown.
                    Err(_) => self.stop(true),
                },
                recv(pty_rx) -> out => match out {
                    Ok(PtyOutput::Data(bytes)) => self.handle_output(&bytes),
                    Ok(PtyOutput::Eof) | Err(_) => self.on_exit(),
                },
                default(timeout) => {}
            }
        }
        tracing::debug!(session = %self.id, "session actor stopped");
    }

    fn handle_cmd(&mut self, cmd: SessionCmd) {
        match cmd {
            SessionCmd::Input(data) => self.input(data),
            SessionCmd::Attach {
                conn,
                outbox,
                dims,
                reply_to,
            } => {
                self.attached_dims.insert(conn, dims);
                self.recompute_dims();
                self.subs.retain(|s| !(s.conn == conn && s.is_full()));
                self.subs.push(Subscriber::full(conn, outbox, reply_to));
                let idx = self.subs.len() - 1;
                self.send_full(idx);
            }
            SessionCmd::Detach { conn } => {
                self.subs.retain(|s| !(s.conn == conn && s.is_full()));
                self.attached_dims.remove(&conn);
                self.recompute_dims();
            }
            SessionCmd::Resize { conn, dims } => {
                self.attached_dims.insert(conn, dims);
                self.recompute_dims();
            }
            SessionCmd::Subscribe {
                conn,
                outbox,
                mode,
                reply_to,
            } => match mode {
                SubscribeMode::Full => {
                    self.subs.retain(|s| !(s.conn == conn && s.is_full()));
                    self.subs.push(Subscriber::full(conn, outbox, reply_to));
                    let idx = self.subs.len() - 1;
                    self.send_full(idx);
                }
                SubscribeMode::Preview { rows, max_hz } => {
                    self.subs.retain(|s| !(s.conn == conn && !s.is_full()));
                    self.subs
                        .push(Subscriber::preview(conn, outbox, rows, max_hz, reply_to));
                    let idx = self.subs.len() - 1;
                    self.send_preview(idx, Instant::now(), true);
                }
            },
            SessionCmd::Unsubscribe { conn } | SessionCmd::ConnClosed { conn } => {
                self.subs.retain(|s| s.conn != conn);
                self.attached_dims.remove(&conn);
                self.recompute_dims();
            }
            SessionCmd::FetchLines {
                start,
                count,
                reply,
            } => {
                let _ = reply.send(self.fetch_lines(start, count));
            }
            SessionCmd::Kill => self.kill(),
            SessionCmd::Revive { spawn, reply } => {
                let result = self.revive(&spawn);
                let started = result.is_ok();
                if reply.send(result).is_err() && started {
                    // The manager stopped waiting and rolled the session
                    // back: do not leave a child running that it does not
                    // know about.
                    tracing::warn!(session = %self.id, "revive reply not delivered; hanging up the new child");
                    self.kill();
                }
            }
            SessionCmd::SetPersist(policy) => self.set_persist(policy),
            SessionCmd::Shutdown { reply } => {
                self.stop(true);
                let _ = reply.send(());
            }
            SessionCmd::Remove { reply } => {
                self.stop(false);
                let _ = reply.send(());
            }
            #[cfg(test)]
            SessionCmd::Crash => panic!("injected session actor panic"),
            #[cfg(test)]
            SessionCmd::Stall(d) => std::thread::sleep(d),
        }
    }
}

fn sane_dims(d: Dims) -> Dims {
    if d.cols < 2 || d.rows < 1 {
        Dims { cols: 80, rows: 24 }
    } else {
        d
    }
}

impl Actor {
    // -- input / output -----------------------------------------------------

    /// Hand input (already accounted by `Manager::input`) to the writer;
    /// never blocks.
    fn input(&mut self, data: Vec<u8>) {
        let Some(live) = &self.live else {
            self.input_budget.release(data.len());
            return;
        };
        if let Err(e) = live.writer.try_send(data) {
            tracing::warn!(session = %self.id, "pty writer gone; input dropped");
            self.input_budget.release(e.into_inner().len());
        }
        let now = Instant::now();
        self.last_input = Some(now);
        if self
            .last_input_signal
            .is_none_or(|t| now.duration_since(t) >= ACTIVITY_MIN_INTERVAL)
        {
            self.last_input_signal = Some(now);
            self.mgr.apply_signal(self.id, Signal::UserInput);
            self.mgr.touch(self.id);
        }
    }

    fn handle_output(&mut self, bytes: &[u8]) {
        let now = Instant::now();
        if let Some(journal) = &mut self.journal {
            if let Err(e) = journal.append(now_ms(), bytes) {
                tracing::warn!(session = %self.id, error = %e, "journal append failed; journal disabled");
                self.journal = None;
            }
        }
        self.sync_term_colors();
        let Some(term) = self.term.as_mut() else {
            return;
        };
        let outcome = term.process(bytes);
        self.handle_outcome(outcome);
        self.mark_screen_dirty(now);
        self.snap_dirty = true;
        self.last_output = Some(now);
        self.silence_reported = false;
        let echo = self
            .last_input
            .is_some_and(|t| now.duration_since(t) < ECHO_WINDOW);
        if !echo
            && self
                .last_activity_signal
                .is_none_or(|t| now.duration_since(t) >= ACTIVITY_MIN_INTERVAL)
        {
            self.last_activity_signal = Some(now);
            self.mgr.apply_signal(self.id, Signal::OutputActivity);
            self.mgr.touch(self.id);
        }
    }

    /// Hand the terminal the GUI's latest colors (`Manager::set_term_colors`)
    /// before it parses output, which may query them.
    fn sync_term_colors(&mut self) {
        let Some(term) = self.term.as_mut() else {
            return;
        };
        if let Some((generation, colors)) = self.mgr.term_colors_since(self.term_colors_seen) {
            term.set_default_colors(colors);
            self.term_colors_seen = generation;
        }
    }

    fn handle_outcome(&mut self, outcome: ProcessOutcome) {
        for ev in outcome.osc {
            match ev {
                OscEvent::Cwd(path) => self.mgr.set_cwd(self.id, path),
                OscEvent::Prompt(_) => self.mgr.apply_signal(self.id, Signal::Osc(ev)),
                OscEvent::Notify { title, body } => self.mgr.broadcast(Event::Notify {
                    session: self.id,
                    title,
                    body,
                }),
            }
        }
        for ev in outcome.events {
            match ev {
                TermEvent::Title(title) => self.mgr.set_title(self.id, title.unwrap_or_default()),
                TermEvent::Bell => self.mgr.broadcast(Event::Bell { session: self.id }),
                TermEvent::PtyWrite(bytes) => self.reply(bytes),
                // Only surfaced with `[terminal] osc52_store = true`. The
                // protocol has no clipboard event yet: log the size, never
                // the text.
                TermEvent::ClipboardStore(text) => tracing::debug!(
                    session = %self.id,
                    bytes = text.len(),
                    "OSC 52 clipboard store (no protocol event yet)"
                ),
                TermEvent::CursorBlinkingChanged => self.mark_screen_dirty(Instant::now()),
                TermEvent::ChildExit(code) => self.child_exit_code = Some(code),
            }
        }
    }

    /// A terminal reply (DA, DSR, ...) joins the input queue, within the
    /// same budget: a child that does not read its input does not get
    /// replies either.
    fn reply(&self, bytes: Vec<u8>) {
        let Some(live) = &self.live else {
            return;
        };
        let n = bytes.len();
        if !self.input_budget.reserve(n) {
            tracing::debug!(session = %self.id, bytes = n, "input queue full; terminal reply dropped");
            return;
        }
        if live.writer.try_send(bytes).is_err() {
            self.input_budget.release(n);
        }
    }

    fn mark_screen_dirty(&mut self, now: Instant) {
        self.screen_dirty = true;
        if self.flush_at.is_none() {
            self.flush_at = Some(now + BATCH);
        }
    }

    // -- screen / preview delivery -----------------------------------------

    fn style_table(&mut self) -> &StyleTable {
        match self.term.as_mut() {
            Some(term) => term.interner().table(),
            None => self.restored_styles.table(),
        }
    }

    /// Send pending damage to `Full` subscribers; mark previews pending.
    fn flush_screen(&mut self) {
        self.flush_at = None;
        if !std::mem::take(&mut self.screen_dirty) {
            return;
        }
        for sub in &mut self.subs {
            if let SubKind::Preview { pending, .. } = &mut sub.kind {
                *pending = true;
            }
        }
        let prefix_len = self.prefix.len() as u64;
        let Some(term) = self.term.as_mut() else {
            return;
        };
        let damage = term.take_damage();
        let needs_full = self.subs.iter().any(|s| {
            matches!(
                s.kind,
                SubKind::Full {
                    needs_full: true,
                    ..
                }
            )
        });
        if !self.subs.iter().any(Subscriber::is_full) || (damage == Damage::None && !needs_full) {
            term.interner().take_pending();
            return;
        }
        let full_lines: Option<Vec<(u16, LineSnapshot)>> =
            (damage == Damage::Full || needs_full).then(|| enumerate_rows(term.screen().lines));
        let delta = match &damage {
            Damage::Lines(rows) => term.lines(rows),
            _ => Vec::new(),
        };
        let cursor = term.cursor();
        let modes = term.modes();
        let dims = term.dims();
        let history_len = prefix_len + term.history_len() as u64;
        self.seq += 1;
        let seq = self.seq;
        let table = term.interner().table();
        for sub in &mut self.subs {
            let SubKind::Full {
                needs_full,
                styles_sent,
            } = &mut sub.kind
            else {
                continue;
            };
            let (full, lines) = match &full_lines {
                Some(all) if damage == Damage::Full || *needs_full => (true, all.clone()),
                _ => (false, delta.clone()),
            };
            let styles = table.entries().skip(*styles_sent).collect();
            *styles_sent = table.len();
            *needs_full = false;
            sub.outbox.push(DaemonMsg {
                reply_to: sub.reply_to.take(),
                event: Event::Screen(ScreenUpdate {
                    session: self.id,
                    seq,
                    dims,
                    full,
                    lines,
                    cursor,
                    modes,
                    display_offset: 0,
                    history_len,
                    styles,
                }),
            });
        }
        term.interner().take_pending();
    }

    /// Full screen (every row + every style) to one `Full` subscriber.
    fn send_full(&mut self, idx: usize) {
        self.seq += 1;
        let prefix_len = self.prefix.len() as u64;
        let (dims, lines, cursor, modes, history_len) = match self.term.as_mut() {
            Some(term) => {
                let screen = term.screen();
                (
                    term.dims(),
                    enumerate_rows(screen.lines),
                    screen.cursor,
                    screen.modes,
                    prefix_len + screen.history_len,
                )
            }
            None => {
                // History-only session: show the tail of the prefix as the
                // screen; everything above it is fetchable history.
                let rows = usize::from(self.dims.rows);
                let shown = self.prefix.len().min(rows);
                let start = self.prefix.len() - shown;
                let mut lines = self.prefix[start..].to_vec();
                lines.resize(rows, LineSnapshot::blank());
                let cursor = CursorState {
                    visible: false,
                    ..Default::default()
                };
                (
                    self.dims,
                    enumerate_rows(lines),
                    cursor,
                    TermModes::empty(),
                    start as u64,
                )
            }
        };
        let seq = self.seq;
        let id = self.id;
        let table = self.style_table();
        let styles: Vec<_> = table.entries().collect();
        let len = table.len();
        let sub = &mut self.subs[idx];
        if let SubKind::Full {
            needs_full,
            styles_sent,
        } = &mut sub.kind
        {
            *needs_full = false;
            *styles_sent = len;
        }
        sub.outbox.push(DaemonMsg {
            reply_to: sub.reply_to.take(),
            event: Event::Screen(ScreenUpdate {
                session: id,
                seq,
                dims,
                full: true,
                lines,
                cursor,
                modes,
                display_offset: 0,
                history_len,
                styles,
            }),
        });
    }

    fn service_previews(&mut self, now: Instant) {
        for idx in 0..self.subs.len() {
            if self.subs[idx].preview_due().is_some_and(|due| due <= now) {
                self.send_preview(idx, now, false);
            }
        }
    }

    fn send_preview(&mut self, idx: usize, now: Instant, force: bool) {
        let SubKind::Preview { rows, .. } = self.subs[idx].kind else {
            return;
        };
        let lines = self.tail_lines(rows);
        let styles = referenced_styles(&lines, self.style_table());
        let id = self.id;
        let sub = &mut self.subs[idx];
        let SubKind::Preview {
            last_sent,
            pending,
            last_lines,
            ..
        } = &mut sub.kind
        else {
            return;
        };
        *pending = false;
        if !force && last_lines.as_ref() == Some(&lines) {
            return;
        }
        *last_sent = Some(now);
        *last_lines = Some(lines.clone());
        sub.outbox.push(DaemonMsg {
            reply_to: sub.reply_to.take(),
            event: Event::Preview {
                session: id,
                lines,
                styles,
            },
        });
    }

    /// The last `n` content lines: screen (trailing blanks trimmed), then
    /// live scrollback, then the restored prefix.
    fn tail_lines(&mut self, n: usize) -> Vec<LineSnapshot> {
        let mut out: Vec<LineSnapshot> = Vec::new();
        if let Some(term) = self.term.as_mut() {
            let mut screen = term.screen().lines;
            trim_trailing_blank(&mut screen);
            let keep = screen.len().min(n);
            out = screen.split_off(screen.len() - keep);
            if out.len() < n {
                let need = n - out.len();
                let hl = term.history_len();
                let mut hist = term.history(hl.saturating_sub(need), need);
                hist.append(&mut out);
                out = hist;
            }
        }
        if out.len() < n && !self.prefix.is_empty() {
            let mut tail_src = &self.prefix[..];
            if out.is_empty() {
                let end = tail_src
                    .iter()
                    .rposition(|l| !l.is_blank())
                    .map_or(0, |i| i + 1);
                tail_src = &tail_src[..end];
            }
            let need = (n - out.len()).min(tail_src.len());
            let mut tail = tail_src[tail_src.len() - need..].to_vec();
            tail.append(&mut out);
            out = tail;
        }
        out
    }

    /// Lines `[start, start + count)` of the virtual line space (restored
    /// prefix ++ live scrollback ++ screen). `start` / `count` come straight
    /// from the client: they are clamped to the space before any arithmetic,
    /// so absurd values yield an empty reply, never an overflow.
    fn fetch_lines(&mut self, start: u64, count: u32) -> Event {
        let count = u64::from(count.min(MAX_FETCH_LINES));
        let prefix_len = self.prefix.len() as u64;
        let hist_len = self.term.as_ref().map_or(0, |t| t.history_len() as u64);
        let screen_rows = self.term.as_ref().map_or(0, |t| u64::from(t.dims().rows));
        let screen_start = prefix_len + hist_len;
        let total = screen_start + screen_rows;
        let start = start.min(total);
        let end = start.saturating_add(count).min(total);
        let mut lines: Vec<LineSnapshot> = Vec::new();
        if start < prefix_len {
            lines.extend_from_slice(&self.prefix[start as usize..end.min(prefix_len) as usize]);
        }
        if let Some(term) = self.term.as_mut() {
            let (from, to) = (start.max(prefix_len), end.min(screen_start));
            if from < to {
                lines.extend(term.history((from - prefix_len) as usize, (to - from) as usize));
            }
            let from = start.max(screen_start);
            if from < end {
                let screen = term.screen().lines;
                let (a, b) = (
                    (from - screen_start) as usize,
                    (end - screen_start) as usize,
                );
                if a < b.min(screen.len()) {
                    lines.extend_from_slice(&screen[a..b.min(screen.len())]);
                }
            }
        }
        let styles = referenced_styles(&lines, self.style_table());
        Event::Lines {
            session: self.id,
            start,
            lines,
            styles,
        }
    }

    fn recompute_dims(&mut self) {
        let effective = self
            .attached_dims
            .values()
            .filter(|d| d.cols >= 2 && d.rows >= 1)
            .fold(None, |acc: Option<Dims>, d| {
                Some(match acc {
                    None => *d,
                    Some(a) => Dims {
                        cols: a.cols.min(d.cols),
                        rows: a.rows.min(d.rows),
                    },
                })
            });
        let Some(dims) = effective else {
            return;
        };
        if dims == self.dims {
            return;
        }
        self.dims = dims;
        if let Some(live) = &self.live {
            if let Err(e) = live.pty.read().resize(dims.cols, dims.rows) {
                tracing::warn!(session = %self.id, error = %e, "pty resize failed");
            }
        }
        self.mgr.set_dims(self.id, dims);
        match self.term.as_mut() {
            Some(term) => {
                term.resize(dims.cols, dims.rows);
                self.mark_screen_dirty(Instant::now());
            }
            None => {
                for idx in 0..self.subs.len() {
                    if self.subs[idx].is_full() {
                        self.send_full(idx);
                    }
                }
            }
        }
    }
}

impl Actor {
    // -- lifecycle ------------------------------------------------------------

    /// berth-vt's kill flow: SIGHUP now, then `poll_child` checks `try_wait`
    /// every `KILL_POLL` and sends SIGKILL after `KILL_GRACE`.
    fn kill(&mut self) {
        let Some(live) = &self.live else {
            return;
        };
        let result = match live.pty.try_write() {
            Some(mut pty) => pty.kill().map_err(|e| e.to_string()),
            // A write is in flight (the child is not reading): signal the
            // groups directly. Not after a reap: the pid may be reused.
            None if self.reaped_at.is_none() => {
                signal_groups(&live.pty, live.pid, libc::SIGHUP).map_err(|e| e.to_string())
            }
            None => Ok(()),
        };
        if let Err(e) = result {
            tracing::warn!(session = %self.id, error = %e, "SIGHUP failed");
        }
        let now = Instant::now();
        // A repeated Kill keeps the original grace period.
        self.kill_since.get_or_insert(now);
        self.next_kill_poll = now + KILL_POLL;
    }

    fn clear_exit_tracking(&mut self) {
        self.kill_since = None;
        self.kill_forced = false;
        self.reaped_at = None;
    }

    /// Reap the child if it is gone (independent of PTY EOF) and escalate a
    /// pending `Kill` to SIGKILL once `KILL_GRACE` has passed.
    fn poll_child(&mut self, now: Instant) {
        let Some(live) = self.live.as_ref() else {
            return;
        };
        if self.kill_since.is_some() {
            self.next_kill_poll = now + KILL_POLL;
        }
        if self.reaped_at.is_some() {
            return;
        }
        // A write in flight holds the handle: no reap this round (so the pid
        // stays the child's), but the escalation below still happens.
        let mut pty = live.pty.try_write();
        if let Some(pty) = pty.as_mut() {
            match pty.try_wait() {
                Ok(Some(code)) => {
                    tracing::debug!(session = %self.id, code, "child reaped");
                    self.reaped_at = Some(now);
                    self.kill_since = None;
                    return;
                }
                Ok(None) => {}
                Err(e) => tracing::debug!(session = %self.id, error = %e, "try_wait failed"),
            }
        }
        if let Some(since) = self.kill_since {
            if !self.kill_forced && now >= since + KILL_GRACE {
                self.kill_forced = true;
                tracing::info!(session = %self.id, "SIGHUP ignored; sending SIGKILL");
                let result = match pty.as_mut() {
                    Some(pty) => pty.force_kill().map_err(|e| e.to_string()),
                    None => {
                        signal_groups(&live.pty, live.pid, libc::SIGKILL).map_err(|e| e.to_string())
                    }
                };
                if let Err(e) = result {
                    tracing::warn!(session = %self.id, error = %e, "SIGKILL failed");
                }
            }
        }
    }

    /// When a reaped child's PTY EOF is due: once output has been quiet for
    /// `EOF_GRACE` after the reap.
    fn eof_overdue_at(&self) -> Option<Instant> {
        let reaped = self.reaped_at?;
        let quiet_since = self.last_output.map_or(reaped, |t| t.max(reaped));
        Some(quiet_since + EOF_GRACE)
    }

    /// The child is gone but its tty stays open: apply what the reader has
    /// already delivered, then finish exactly like on EOF.
    fn finish_without_eof(&mut self) {
        loop {
            let next = match &self.live {
                Some(live) => live.rx.try_recv(),
                None => return,
            };
            match next {
                Ok(PtyOutput::Data(bytes)) => self.handle_output(&bytes),
                Ok(PtyOutput::Eof) | Err(_) => break,
            }
        }
        tracing::info!(session = %self.id, "child exited but its tty stays open; finishing");
        self.on_exit();
    }

    fn revive(&mut self, spawn: &PtySpawn) -> Result<(), String> {
        if self.live.is_some() {
            return Err("session is live".into());
        }
        self.start_pty(spawn)?;
        for sub in &mut self.subs {
            if let SubKind::Full { needs_full, .. } = &mut sub.kind {
                *needs_full = true;
            }
        }
        self.mark_screen_dirty(Instant::now());
        Ok(())
    }

    fn set_persist(&mut self, policy: PersistPolicy) {
        self.persist = policy;
        if policy.snapshot {
            self.snap_dirty = true;
        } else if let Err(e) = self.mgr.store.delete_snapshot(self.id) {
            tracing::warn!(session = %self.id, error = %e, "cannot delete snapshot");
        }
        if !policy.journal {
            self.journal = None;
        } else if self.journal.is_none() && self.live.is_some() {
            self.open_journal();
        }
    }

    fn stop(&mut self, snapshot: bool) {
        self.flush_screen();
        if snapshot && self.snap_dirty {
            self.write_snapshot(true);
        }
        if let Some(live) = self.live.take() {
            self.hang_up(live);
        }
        if let Some(mut j) = self.journal.take() {
            if let Err(e) = j.flush() {
                tracing::warn!(session = %self.id, error = %e, "journal flush failed");
            }
        }
        self.stopped = true;
    }

    /// Let go of the PTY without waiting: SIGHUP now; dropping the handle
    /// reaps the child in the background (SIGKILL after `KILL_GRACE` if
    /// SIGHUP is ignored). A write in flight keeps the handle alive until
    /// that write fails, so then a watchdog sends the SIGKILL instead.
    fn hang_up(&self, live: Live) {
        // From here on the writer discards instead of starting a write.
        live.closed.store(true, Ordering::Release);
        let in_flight = match live.pty.try_write() {
            Some(mut pty) => {
                if let Err(e) = pty.kill() {
                    tracing::debug!(session = %self.id, error = %e, "SIGHUP on stop failed");
                }
                false
            }
            None => true,
        };
        if in_flight && self.reaped_at.is_none() {
            if let Err(e) = signal_groups(&live.pty, live.pid, libc::SIGHUP) {
                tracing::debug!(session = %self.id, error = %e, "SIGHUP on stop failed");
            }
            spawn_kill_watchdog(self.id, Arc::downgrade(&live.pty), live.pid);
        }
    }

    /// PTY EOF: final screen, exit code, Dormant + Exited, final snapshot.
    fn on_exit(&mut self) {
        let Some(live) = self.live.take() else {
            return;
        };
        self.flush_screen();
        // Queued input has nowhere to go any more.
        live.closed.store(true, Ordering::Release);
        let code = wait_exit(&live.pty, EXIT_CODE_WAIT).or(self.child_exit_code);
        drop(live);
        self.clear_exit_tracking();
        if let Some(mut j) = self.journal.take() {
            if let Err(e) = j.flush() {
                tracing::warn!(session = %self.id, error = %e, "journal flush failed");
            }
        }
        self.mgr.session_exited(self.id, code);
        // Snapshot before announcing the exit so observers find it on disk.
        self.write_snapshot(true);
        self.mgr.broadcast(Event::Exited {
            session: self.id,
            code,
        });
    }

    /// `at_exit` also snapshots an alternate-screen terminal (normally
    /// deferred: the vt layer hides the primary scrollback while a full
    /// screen TUI runs, so such a snapshot would lose history).
    fn write_snapshot(&mut self, at_exit: bool) {
        self.last_snapshot = Instant::now();
        if !self.persist.snapshot {
            self.snap_dirty = false;
            return;
        }
        if let Some(term) = &self.term {
            if !at_exit && self.snapshot_written && term.modes().contains(TermModes::ALT_SCREEN) {
                return;
            }
        }
        let Some(meta) = self.mgr.meta(self.id) else {
            return;
        };
        let prefix_len = self.prefix.len();
        let mut history = std::mem::take(&mut self.prefix);
        let (screen, styles) = match self.term.as_mut() {
            Some(term) => {
                let hl = term.history_len();
                history.extend(term.history(0, hl));
                (Some(term.screen()), term.interner().table().clone())
            }
            None => (None, self.restored_styles.table().clone()),
        };
        let excess = history.len().saturating_sub(self.cfg.max_restored_lines);
        let dropped: Vec<LineSnapshot> = history.drain(..excess).collect();
        let snap = SessionSnapshotFile {
            format_version: SNAPSHOT_FORMAT_VERSION,
            saved_at_ms: now_ms(),
            // A copy of the metadata as of this write, not its latest state:
            // the registry is the authority (the archive mark included), and
            // restoring does not read it (only history, screen and styles;
            // the store checks its id).
            session: meta,
            styles,
            history,
            screen,
        };
        let result = self.mgr.store.write_snapshot(&snap);
        let mut history = snap.history;
        history.splice(0..0, dropped);
        history.truncate(prefix_len);
        self.prefix = history;
        match result {
            Ok(()) => {
                self.snap_dirty = false;
                self.snapshot_written = true;
            }
            Err(e) => tracing::warn!(session = %self.id, error = %e, "snapshot write failed"),
        }
    }

    // -- timers ---------------------------------------------------------------

    fn on_timers(&mut self, now: Instant) {
        let sync_expired = self
            .term
            .as_ref()
            .and_then(Terminal::sync_deadline)
            .is_some_and(|d| d <= now);
        if sync_expired {
            // The output held back is parsed now.
            self.sync_term_colors();
            if let Some(term) = self.term.as_mut() {
                let outcome = term.flush_expired_sync();
                self.handle_outcome(outcome);
                self.mark_screen_dirty(now);
            }
        }
        if self.flush_at.is_some_and(|t| t <= now) {
            self.flush_screen();
        }
        self.service_previews(now);
        if self.snap_dirty && now >= self.last_snapshot + self.cfg.snapshot_interval {
            self.write_snapshot(false);
        }
        if self.live.is_some() && !self.silence_reported {
            if let Some(last) = self.last_output {
                if now >= last + SILENCE {
                    self.silence_reported = true;
                    let cursor_at_line_start =
                        self.term.as_ref().is_some_and(|t| t.cursor().col == 0);
                    self.mgr.apply_signal(
                        self.id,
                        Signal::Silence {
                            secs: now.duration_since(last).as_secs(),
                            cursor_at_line_start,
                        },
                    );
                }
            }
        }
        let mut poll_child = self.kill_since.is_some() && now >= self.next_kill_poll;
        if let Some(live) = &self.live {
            if now >= self.next_fg_poll {
                self.next_fg_poll = now + FOREGROUND_POLL;
                poll_child = true;
                let fg = live.pty.read().foreground_process();
                // The child is the session's shell and its pid its process
                // group: in the foreground, it is at its prompt (a job
                // control shell gives every command a group of its own).
                let shell = fg
                    .as_ref()
                    .map(|p| p.pid == live.pid && is_shell_program(&p.name));
                if shell != self.shell_in_foreground {
                    self.shell_in_foreground = shell;
                    self.mgr.set_shell_in_foreground(self.id, shell);
                }
                let name = fg.map(|p| p.name);
                if name != self.fg_name {
                    self.fg_name = name.clone();
                    if let Some(name) = name {
                        self.mgr
                            .apply_signal(self.id, Signal::ForegroundProcess(name));
                    }
                }
            }
        }
        if poll_child {
            self.poll_child(now);
        }
        if self.live.is_some() && self.eof_overdue_at().is_some_and(|t| t <= now) {
            self.finish_without_eof();
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut consider = |t: Option<Instant>| {
            if let Some(t) = t {
                next = Some(next.map_or(t, |n| n.min(t)));
            }
        };
        consider(self.flush_at);
        consider(self.term.as_ref().and_then(Terminal::sync_deadline));
        for sub in &self.subs {
            consider(sub.preview_due());
        }
        if self.snap_dirty {
            consider(Some(self.last_snapshot + self.cfg.snapshot_interval));
        }
        if self.live.is_some() {
            if !self.silence_reported {
                consider(self.last_output.map(|t| t + SILENCE));
            }
            consider(Some(self.next_fg_poll));
            if self.kill_since.is_some() {
                consider(Some(self.next_kill_poll));
            }
            consider(self.eof_overdue_at());
        }
        next
    }
}

impl Actor {
    /// The actor thread panicked. Salvage what can be salvaged — a final
    /// snapshot (itself guarded: the state may be inconsistent), the journal
    /// — hang up the child (dropping the handle does that), and let the
    /// manager forget this actor: the next request starts a history-only
    /// actor from the snapshot, so Attach / FetchLines / Revive / Delete keep
    /// working.
    fn crashed(&mut self, what: String) {
        tracing::error!(session = %self.id, panic = %what, "session actor panicked");
        let was_live = self.live.is_some();
        let saved = catch_unwind(AssertUnwindSafe(|| self.write_snapshot(true)));
        if saved.is_err() {
            tracing::error!(session = %self.id, "final snapshot after the panic failed");
        }
        if let Some(live) = self.live.take() {
            self.hang_up(live);
        }
        if let Some(mut j) = self.journal.take() {
            if let Err(e) = j.flush() {
                tracing::warn!(session = %self.id, error = %e, "journal flush failed");
            }
        }
        self.subs.clear();
        self.mgr.actor_crashed(self.id, was_live, &what);
    }
}

fn panic_message(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".into())
}

fn enumerate_rows(lines: Vec<LineSnapshot>) -> Vec<(u16, LineSnapshot)> {
    lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| (i as u16, l))
        .collect()
}

/// Poll for the child's exit code for up to `max`. A write still in flight
/// (it fails once the tty is gone) only delays the polls.
fn wait_exit(pty: &RwLock<PtyHandle>, max: Duration) -> Option<i32> {
    let deadline = Instant::now() + max;
    loop {
        if let Some(mut pty) = pty.try_write() {
            match pty.try_wait() {
                Ok(Some(code)) => return Some(code),
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "try_wait failed");
                    return None;
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The PTY's writer thread: writes queued input in order, off the actor
/// thread, and releases each job's bytes from the session's budget once
/// written or discarded. After a failed write (the tty is gone), once the
/// PTY is let go (`closed`) or the handle is dropped, the rest is discarded.
/// Ends when the actor drops the queue's sender.
fn spawn_writer(
    id: SessionId,
    pty: Weak<RwLock<PtyHandle>>,
    closed: Arc<AtomicBool>,
    budget: Arc<InputBudget>,
) -> std::io::Result<Sender<Vec<u8>>> {
    let (tx, jobs) = crossbeam_channel::unbounded::<Vec<u8>>();
    std::thread::Builder::new()
        .name(format!("pty-writer-{}", id.short()))
        .spawn(move || {
            let mut broken = false;
            for bytes in jobs {
                if !broken {
                    broken = match pty.upgrade() {
                        None => true,
                        Some(pty) => {
                            let pty = pty.read();
                            // Checked under the lock: `hang_up` sets `closed`
                            // before its `try_write`, so no write can start
                            // after the hang-up found the handle idle.
                            if closed.load(Ordering::Acquire) {
                                true
                            } else if let Err(e) = pty.write(&bytes) {
                                tracing::debug!(session = %id, error = %e, "pty write failed; discarding queued input");
                                true
                            } else {
                                false
                            }
                        }
                    };
                }
                budget.release(bytes.len());
            }
        })?;
    Ok(tx)
}

/// `PtyHandle::kill` / `force_kill` for when a write in flight holds the
/// handle: signal the child's process group and the tty's foreground group.
/// Callers make sure the child has not been reaped — only `try_wait` under
/// the write lock reaps it — so `pid` still names it.
#[allow(unsafe_code)]
fn signal_groups(pty: &RwLock<PtyHandle>, pid: u32, signal: libc::c_int) -> std::io::Result<()> {
    let own = libc::pid_t::try_from(pid)
        .map_err(|_| std::io::Error::other(format!("pid {pid} does not fit pid_t")))?;
    // `foreground_process` names a live member of the foreground group.
    let foreground = pty
        .read()
        .foreground_process()
        .and_then(|p| libc::pid_t::try_from(p.pid).ok())
        // SAFETY: getpgid takes a plain integer.
        .map(|p| unsafe { libc::getpgid(p) })
        .filter(|&group| group != own);
    let result = kill_group(own, signal);
    if let Some(group) = foreground {
        if let Err(e) = kill_group(group, signal) {
            tracing::debug!(group, signal, error = %e, "signalling the foreground group failed");
        }
    }
    result
}

/// `killpg(2)`; `ESRCH` (group already gone) is success. Never our own group
/// (0), init (1) or an error value.
#[allow(unsafe_code)]
fn kill_group(group: libc::pid_t, signal: libc::c_int) -> std::io::Result<()> {
    // SAFETY: getpgrp takes no arguments and cannot fail.
    if group <= 1 || group == unsafe { libc::getpgrp() } {
        return Err(std::io::Error::other(format!(
            "refusing to signal process group {group}"
        )));
    }
    // SAFETY: killpg takes plain integers.
    if unsafe { libc::killpg(group, signal) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(err)
    }
}

/// A hung-up child whose handle a stuck write still holds: SIGKILL its
/// groups after `KILL_GRACE` unless the handle is gone by then. The child's
/// death takes the tty with it, which fails the write and lets the handle
/// drop (and reap the child). While the handle lives nobody reaps the child,
/// so its pid is still its own.
fn spawn_kill_watchdog(id: SessionId, pty: Weak<RwLock<PtyHandle>>, pid: u32) {
    let spawned = std::thread::Builder::new()
        .name(format!("pty-kill-{}", id.short()))
        .spawn(move || {
            std::thread::sleep(KILL_GRACE);
            if let Some(pty) = pty.upgrade() {
                tracing::info!(session = %id, "SIGHUP ignored; sending SIGKILL");
                if let Err(e) = signal_groups(&pty, pid, libc::SIGKILL) {
                    tracing::warn!(session = %id, error = %e, "SIGKILL failed");
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(session = %id, error = %e, "cannot start the kill watchdog thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review medium #7: pending input is capped; an empty queue always
    /// takes one message; one warning per overflow episode.
    #[test]
    fn input_budget_caps_pending_bytes() {
        let b = InputBudget::default();
        assert!(
            b.reserve(3 * MAX_PENDING_INPUT),
            "one big paste into an empty queue"
        );
        assert!(!b.reserve(1), "anything behind it waits for the write");
        assert!(b.first_drop());
        assert!(!b.first_drop(), "warned once");
        b.release(3 * MAX_PENDING_INPUT);
        assert_eq!(b.pending(), 0);
        assert!(b.first_drop(), "a new episode warns again");
        b.release(0);

        let chunk = 64 * 1024;
        let admitted = (0..40).filter(|_| b.reserve(chunk)).count();
        assert_eq!(admitted, MAX_PENDING_INPUT / chunk);
        assert_eq!(b.pending(), MAX_PENDING_INPUT);
        b.release(chunk);
        assert!(b.reserve(chunk), "room again after a write");
    }
}
