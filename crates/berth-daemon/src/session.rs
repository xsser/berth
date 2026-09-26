//! Session actor: one std thread per session owning `PtyHandle` +
//! `Terminal`. The loop selects over PTY output, commands and deadlines
//! (4 ms render batch, preview throttle, 5 s snapshot, silence / foreground
//! heuristics, kill escalation) — no fixed-rate ticker, so idle sessions
//! don't wake up needlessly.
//!
//! Virtual line space (`FetchLines`, `history_len`):
//! `[restored prefix 0..R) ++ [live scrollback) ++ [screen rows)`. The prefix
//! is what earlier lives left behind (loaded lazily from the snapshot for
//! restored sessions, or folded from the old terminal on revive); style ids
//! stay valid across lives because the new terminal inherits the interner.

use std::collections::HashMap;
use std::sync::Arc;
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
    TerminalConfig,
};
use crossbeam_channel::{select, Receiver, Sender};
use tokio::sync::oneshot;

use crate::agent_state::{Signal, SILENCE_IDLE_SECS};
use crate::manager::Manager;
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
const KILL_GRACE: Duration = Duration::from_secs(1);
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
}

#[derive(Clone, Debug)]
pub(crate) struct ActorConfig {
    pub scrollback: usize,
    pub snapshot_interval: Duration,
    pub max_restored_lines: usize,
}

pub(crate) struct ActorHandle {
    pub tx: Sender<SessionCmd>,
    pub join: Option<JoinHandle<()>>,
}

struct Live {
    pty: PtyHandle,
    rx: Receiver<PtyOutput>,
}

pub(crate) struct Actor {
    id: SessionId,
    mgr: Arc<Manager>,
    cfg: ActorConfig,
    persist: PersistPolicy,
    live: Option<Live>,
    term: Option<Terminal>,
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
    next_fg_poll: Instant,
    kill_deadline: Option<Instant>,
    child_exit_code: Option<i32>,
    journal: Option<JournalWriter>,
    stopped: bool,
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
    let join = std::thread::Builder::new()
        .name(format!("session-{}", actor.id.short()))
        .spawn(move || {
            if init(&mut actor) {
                actor.run(rx);
            }
        })?;
    Ok(ActorHandle {
        tx,
        join: Some(join),
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
            next_fg_poll: now,
            kill_deadline: None,
            child_exit_code: None,
            journal: None,
            stopped: false,
        }
    }

    fn load_restored(&mut self) {
        match self.mgr.store.read_snapshot(self.id) {
            Ok(Some(snap)) => {
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
        self.term = Some(term);
        self.live = Some(Live { pty, rx });
        self.child_exit_code = None;
        self.kill_deadline = None;
        self.fg_name = None;
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

    fn run(mut self, rx: Receiver<SessionCmd>) {
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
            SessionCmd::Input(data) => self.input(&data),
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
                let _ = reply.send(self.revive(&spawn));
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

    fn input(&mut self, data: &[u8]) {
        let Some(live) = &self.live else {
            return;
        };
        if let Err(e) = live.pty.write(data) {
            tracing::warn!(session = %self.id, error = %e, "pty write failed");
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
                TermEvent::PtyWrite(bytes) => {
                    if let Some(live) = &self.live {
                        if let Err(e) = live.pty.write(&bytes) {
                            tracing::warn!(session = %self.id, error = %e, "pty reply write failed");
                        }
                    }
                }
                TermEvent::ClipboardStore(_) => {
                    tracing::debug!(session = %self.id, "OSC 52 clipboard store ignored")
                }
                TermEvent::CursorBlinkingChanged => self.mark_screen_dirty(Instant::now()),
                TermEvent::ChildExit(code) => self.child_exit_code = Some(code),
            }
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

    fn fetch_lines(&mut self, start: u64, count: u32) -> Event {
        let count = u64::from(count.min(MAX_FETCH_LINES));
        let prefix_len = self.prefix.len() as u64;
        let mut lines: Vec<LineSnapshot> = Vec::new();
        let end_prefix = (start + count).min(prefix_len);
        if start < end_prefix {
            lines.extend_from_slice(&self.prefix[start as usize..end_prefix as usize]);
        }
        if let Some(term) = self.term.as_mut() {
            let hist_len = term.history_len() as u64;
            let mut pos = start.max(prefix_len);
            let end = start + count;
            if pos < end && pos < prefix_len + hist_len {
                let take = (end.min(prefix_len + hist_len) - pos) as usize;
                lines.extend(term.history((pos - prefix_len) as usize, take));
                pos += take as u64;
            }
            let screen_start = prefix_len + hist_len;
            if pos < end && pos >= screen_start {
                let screen = term.screen().lines;
                let from = (pos - screen_start) as usize;
                let to = ((end - screen_start) as usize).min(screen.len());
                if from < to {
                    lines.extend_from_slice(&screen[from..to]);
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
            if let Err(e) = live.pty.resize(dims.cols, dims.rows) {
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

    fn kill(&mut self) {
        if let Some(live) = &mut self.live {
            if let Err(e) = live.pty.kill() {
                tracing::warn!(session = %self.id, error = %e, "SIGHUP failed");
            }
            self.kill_deadline = Some(Instant::now() + KILL_GRACE);
        }
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
        if let Some(mut live) = self.live.take() {
            // Dropping the handle reaps the child in the background
            // (SIGKILL after berth-vt's grace period if SIGHUP is ignored).
            if let Err(e) = live.pty.kill() {
                tracing::debug!(session = %self.id, error = %e, "SIGHUP on stop failed");
            }
        }
        if let Some(mut j) = self.journal.take() {
            if let Err(e) = j.flush() {
                tracing::warn!(session = %self.id, error = %e, "journal flush failed");
            }
        }
        self.stopped = true;
    }

    /// PTY EOF: final screen, exit code, Dormant + Exited, final snapshot.
    fn on_exit(&mut self) {
        let Some(mut live) = self.live.take() else {
            return;
        };
        self.flush_screen();
        let code = wait_exit(&mut live.pty, EXIT_CODE_WAIT).or(self.child_exit_code);
        drop(live);
        self.kill_deadline = None;
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
        if let Some(term) = self.term.as_mut() {
            if term.sync_deadline().is_some_and(|d| d <= now) {
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
        if let Some(live) = &self.live {
            if now >= self.next_fg_poll {
                self.next_fg_poll = now + FOREGROUND_POLL;
                let name = live.pty.foreground_process().map(|p| p.name);
                if name != self.fg_name {
                    self.fg_name = name.clone();
                    if let Some(name) = name {
                        self.mgr
                            .apply_signal(self.id, Signal::ForegroundProcess(name));
                    }
                }
            }
        }
        if self.kill_deadline.is_some_and(|d| d <= now) {
            self.kill_deadline = None;
            if let Some(live) = &mut self.live {
                if let Err(e) = live.pty.force_kill() {
                    tracing::warn!(session = %self.id, error = %e, "SIGKILL failed");
                }
            }
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
        }
        consider(self.kill_deadline);
        next
    }
}

fn enumerate_rows(lines: Vec<LineSnapshot>) -> Vec<(u16, LineSnapshot)> {
    lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| (i as u16, l))
        .collect()
}

/// Poll for the child's exit code for up to `max`.
fn wait_exit(pty: &mut PtyHandle, max: Duration) -> Option<i32> {
    let deadline = Instant::now() + max;
    loop {
        match pty.try_wait() {
            Ok(Some(code)) => return Some(code),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => return None,
            Err(e) => {
                tracing::debug!(error = %e, "try_wait failed");
                return None;
            }
        }
    }
}
