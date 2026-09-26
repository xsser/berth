//! winit application (DESIGN §8; integrate.md §1–§4): one window and one
//! wgpu device/surface shared by the terminal grid renderer and the egui
//! sidebar, driven by the connection to `berthd`.
//!
//! Run modes:
//! - interactive (default): connects to berthd (launching it when nothing
//!   listens), shows the focused session and the sidebar, renders when
//!   something changed, blinks the cursor and ticks the sidebar every
//!   250 ms. A lost connection is retried with backoff (1 s … 10 s),
//!   launching the daemon again if needed.
//! - `--screenshot <png>`: waits for the session list and the focused
//!   screen (plus `--screenshot-delay-ms`), writes the frame and exits.
//!   Works while the screen is locked: frames are then rendered offscreen.
//! - `--stats`: every frame is GPU-synchronised and timed; a report goes to
//!   stderr every 5 s and at exit (offscreen when occluded).
//! - `--bench <secs>`: renderer benchmark on the static fixture, no daemon.
//!
//! Daemon messages arrive as `UserEvent`s through the `EventLoopProxy`
//! from the client's reader thread. Each connection has a generation;
//! events of older connections are dropped, and events that beat the
//! `Connected` event of their own connection are replayed after it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use berth_core::{
    ClientRole, CursorShape, Dims, Paths, Request, ScreenSnapshot, StyleTable, TermModes,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey};
use winit::window::{ImePurpose, Window, WindowId};

use crate::client::{self, Client, ClientEvent};
use crate::config::Config;
use crate::controller::{Controller, Effect, Outbound};
use crate::dock::DockBadge;
use crate::fixture::{Fixture, COLS, ROWS};
use crate::ime::{ImeOutcome, ImeState};
use crate::input::{self, ImeGate, KeyAction, KeyPress, Mods, ScrollKey, Shortcut};
use crate::mismatch::Mismatch;
use crate::mouse::{self, Button, MouseEvent, WheelAccum};
use crate::notify::Notifier;
use crate::renderer::{CellMetrics, FrameInput, GridLayout, GridRenderer, PrepareStats};
use crate::selection::{Point, Selection, SelectionKind};
use crate::sidebar::{Chrome, Sidebar, UiAction};
use crate::stats::{FrameStats, FrameTiming};
use crate::theme::Theme;

/// Padding around the grid (Ghostty's default `window-padding-x/y = 2`).
pub const PADDING_PT: f32 = 2.0;
const BLINK: Duration = Duration::from_millis(530);
const SIDEBAR_TICK: Duration = Duration::from_millis(250);
const TARGET_MS: f64 = 8.0;
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(30);
const STATS_WINDOW: Duration = Duration::from_secs(5);
/// Offscreen frames have no vsync pacing: at most one per 8 ms.
const OFFSCREEN_INTERVAL: Duration = Duration::from_millis(8);
const RECONNECT_MAX: Duration = Duration::from_secs(10);
const MULTI_CLICK: Duration = Duration::from_millis(400);

#[derive(Clone, Debug, Default)]
pub struct GuiOptions {
    pub screenshot: Option<PathBuf>,
    /// Extra wait once the screenshot content is there (previews, glyphs).
    pub screenshot_delay: Duration,
    /// Focus this session id (or unique prefix) at start.
    pub session: Option<String>,
    /// Scroll the focused session this many lines up once it is shown.
    pub scroll: Option<u32>,
    pub stats: bool,
    pub exit_after: Option<Duration>,
    pub bench_secs: Option<f64>,
    /// Initial window size in cells (default 120×40).
    pub cells: Option<(u16, u16)>,
    pub font_family: Option<String>,
    pub font_size: Option<f32>,
    pub no_vsync: bool,
    /// Cursor shape override (Ghostty `cursor-style`).
    pub cursor_style: Option<CursorShape>,
    /// Inject a synthetic `Ime::Preedit` at startup (screenshot check of the
    /// preedit overlay; real IME events go through the same handler).
    pub demo_preedit: Option<String>,
    /// Show this session's hover details without a pointer (screenshot
    /// check; a screenshot waits for its events).
    pub demo_hover: Option<String>,
    /// Press 「重启 berthd」 as soon as the version-mismatch banner shows
    /// (end-to-end check; a screenshot waits for the restarted daemon's
    /// session list instead of the banner).
    pub demo_restart: bool,
}

/// Events from other threads.
pub enum UserEvent {
    Connected {
        generation: u64,
        client: Box<Client>,
        launched: bool,
    },
    ConnectFailed {
        generation: u64,
        error: String,
        /// berthd answered `Hello` with another protocol version.
        refused: Option<client::Incompatible>,
    },
    /// The stop of 「重启 berthd」 finished (`Ok(None)`: nothing was running).
    DaemonStopped(std::result::Result<Option<client::Stopped>, String>),
    Daemon {
        generation: u64,
        event: ClientEvent,
    },
    /// ⌘⇧N folder picker: `Ok(None)` when cancelled.
    FolderPicked(std::result::Result<Option<PathBuf>, String>),
}

pub fn run(opts: GuiOptions) -> Result<()> {
    let t0 = Instant::now();
    let paths = Paths::resolve();
    let (mut config, config_warning) = Config::load_from(&paths.config_file);
    if let Some(family) = &opts.font_family {
        config.font.family = family.clone();
    }
    if let Some(size) = opts.font_size {
        config.font.size = size;
    }
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("creating the event loop")?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(opts, config, config_warning, paths, proxy, t0);
    app.loop_started = Some(Instant::now());
    event_loop
        .run_app(&mut app)
        .context("running the event loop")?;
    if let Some(live) = app.live_stats.take() {
        live.print_final(&traffic_label(&mut app.ctl));
    }
    match app.error.take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A continuous-rendering measurement window (`--bench`).
struct Measure {
    label: String,
    secs: f64,
    until: Option<Instant>,
    stats: FrameStats,
    /// Drop the shaping cache every frame (every line reshaped).
    reshape: bool,
}

impl Measure {
    fn new(label: impl Into<String>, secs: f64, reshape: bool) -> Box<Self> {
        Box::new(Self {
            label: label.into(),
            secs,
            until: None,
            stats: FrameStats::default(),
            reshape,
        })
    }
}

enum Mode {
    Interactive,
    Screenshot {
        path: PathBuf,
        ready_since: Option<Instant>,
        deadline: Instant,
    },
    Bench {
        current: Box<Measure>,
        pending: VecDeque<Box<Measure>>,
        fixture: Box<Fixture>,
    },
    Exiting,
}

/// `--stats` in interactive mode: consecutive 5 s windows.
struct LiveStats {
    window: FrameStats,
    started: Instant,
    total_frames: usize,
    windows: usize,
}

impl LiveStats {
    fn new() -> Self {
        Self {
            window: FrameStats::default(),
            started: Instant::now(),
            total_frames: 0,
            windows: 0,
        }
    }

    /// Record a frame; true when the current window is complete.
    fn record(&mut self, start: Instant, t: FrameTiming) -> bool {
        self.window.record(start, t);
        self.total_frames += 1;
        self.started.elapsed() >= STATS_WINDOW
    }

    /// Print the current window and start the next one.
    fn report(&mut self, traffic: &str) {
        self.windows += 1;
        let title = format!("live window {} ({traffic})", self.windows);
        eprint!("{}", self.window.report(&title, TARGET_MS));
        self.window = FrameStats::default();
        self.started = Instant::now();
    }

    fn print_final(&self, traffic: &str) {
        if self.window.frames() > 0 {
            let title = format!("live, last partial window ({traffic})");
            eprint!("{}", self.window.report(&title, TARGET_MS));
        }
        eprintln!(
            "[stats] {} frames rendered in total ({} full windows)",
            self.total_frames, self.windows
        );
    }
}

/// What the daemon sent during a stats window.
fn traffic_label(ctl: &mut Controller) -> String {
    let c = ctl.take_counters();
    let dims = ctl.view().map(|v| v.dims()).unwrap_or_default();
    format!(
        "focused {}×{}: {} screen updates applied; sidebar: {} preview updates from {} sessions",
        dims.cols,
        dims.rows,
        c.screens,
        c.previews,
        c.preview_sessions.len()
    )
}

/// Requests while no connection exists fail with a visible error.
struct NoConnection;

impl Outbound for NoConnection {
    fn send(&mut self, _req: Request) -> Result<u32> {
        bail!("未连接 berthd")
    }
}

macro_rules! out {
    ($app:expr) => {
        match $app.client.as_mut() {
            Some(c) => c as &mut dyn Outbound,
            None => &mut $app.no_conn as &mut dyn Outbound,
        }
    };
}

struct App {
    opts: GuiOptions,
    config: Config,
    config_warning: Option<String>,
    t0: Instant,
    paths: Paths,
    proxy: EventLoopProxy<UserEvent>,
    gfx: Option<Gfx>,
    ctl: Controller,
    client: Option<Client>,
    no_conn: NoConnection,
    generation: u64,
    /// Events of the current generation that arrived before `Connected`.
    early: Vec<ClientEvent>,
    connecting: bool,
    reconnect_at: Option<Instant>,
    attempts: u32,
    last_connect_error: Option<String>,
    /// A berthd of another protocol version, and its restart.
    mismatch: Mismatch,
    notifier: Option<Notifier>,
    /// Dock badge (not in bench runs, which have no daemon).
    dock: Option<DockBadge>,
    mode: Mode,
    palette_open: bool,
    picking_folder: bool,
    live_stats: Option<LiveStats>,
    exit_at: Option<Instant>,
    /// `--scroll`: applied once the focused screen is there.
    pending_scroll: Option<i64>,
    error: Option<anyhow::Error>,
    /// When `run_app` was entered (startup breakdown: launch → resumed).
    loop_started: Option<Instant>,
}

impl App {
    fn new(
        opts: GuiOptions,
        config: Config,
        config_warning: Option<String>,
        paths: Paths,
        proxy: EventLoopProxy<UserEvent>,
        t0: Instant,
    ) -> Self {
        let mut ctl = Controller::new(config.notify_on.clone());
        if let Some(want) = &opts.session {
            ctl.want_session(want.clone());
        }
        let vsync = if opts.no_vsync { "no vsync" } else { "vsync" };
        let mode = if let Some(path) = &opts.screenshot {
            Mode::Screenshot {
                path: path.clone(),
                ready_since: None,
                deadline: Instant::now() + SCREENSHOT_TIMEOUT,
            }
        } else if let Some(secs) = opts.bench_secs {
            let mut pending = VecDeque::new();
            pending.push_back(Measure::new(
                format!("bench B: every line reshaped every frame ({secs}s, gpu-synced, {vsync})"),
                secs,
                true,
            ));
            Mode::Bench {
                current: Measure::new(
                    format!("bench A: cursor fade, cached shaping ({secs}s, gpu-synced, {vsync})"),
                    secs,
                    false,
                ),
                pending,
                fixture: Box::new(Fixture::build()),
            }
        } else {
            Mode::Interactive
        };
        // No desktop notifications from one-shot runs.
        let notifier = match mode {
            Mode::Interactive => match Notifier::start() {
                Ok(n) => Some(n),
                Err(e) => {
                    ctl.error(format!("通知线程无法启动：{e:#}"));
                    None
                }
            },
            _ => None,
        };
        let exit_at = opts.exit_after.map(|d| t0 + d);
        let dock = (!matches!(mode, Mode::Bench { .. })).then(DockBadge::default);
        Self {
            live_stats: opts.stats.then(LiveStats::new),
            pending_scroll: opts.scroll.map(i64::from),
            opts,
            config,
            config_warning,
            t0,
            paths,
            proxy,
            gfx: None,
            ctl,
            client: None,
            no_conn: NoConnection,
            generation: 0,
            early: Vec::new(),
            connecting: false,
            reconnect_at: None,
            mismatch: Mismatch::None,
            attempts: 0,
            last_connect_error: None,
            notifier,
            dock,
            mode,
            palette_open: false,
            picking_folder: false,
            exit_at,
            error: None,
            loop_started: None,
        }
    }

    fn is_bench(&self) -> bool {
        matches!(self.mode, Mode::Bench { .. })
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, err: anyhow::Error) {
        tracing::error!("{err:#}");
        if self.error.is_none() {
            self.error = Some(err);
        }
        self.mode = Mode::Exiting;
        event_loop.exit();
    }

    fn quit(&mut self, event_loop: &ActiveEventLoop) {
        self.mode = Mode::Exiting;
        event_loop.exit();
    }

    fn redraw_soon(&mut self) {
        if let Some(gfx) = self.gfx.as_mut() {
            gfx.dirty = true;
        }
    }

    // -- connection --------------------------------------------------------

    fn start_connect(&mut self) {
        self.generation += 1;
        let generation = self.generation;
        self.connecting = true;
        self.early.clear();
        let proxy = self.proxy.clone();
        let paths = self.paths.clone();
        let spawned = std::thread::Builder::new()
            .name("berth-connect".into())
            .spawn(move || {
                let mut launch = client::launch_berthd;
                let result =
                    client::connect_or_spawn(&paths, &mut launch).and_then(|(stream, launched)| {
                        let sink = proxy.clone();
                        let client = Client::start(stream, ClientRole::Gui, move |event| {
                            let _ = sink.send_event(UserEvent::Daemon { generation, event });
                        })?;
                        Ok((client, launched))
                    });
                let event = match result {
                    Ok((client, launched)) => UserEvent::Connected {
                        generation,
                        client: Box::new(client),
                        launched,
                    },
                    Err(e) => UserEvent::ConnectFailed {
                        generation,
                        error: format!("{e:#}"),
                        refused: client::incompatible(&e),
                    },
                };
                let _ = proxy.send_event(event);
            });
        if let Err(e) = spawned {
            self.connecting = false;
            self.ctl.error(format!("无法启动连接线程：{e}"));
            self.schedule_reconnect();
        }
    }

    /// 「重启 berthd」: stop the berthd of another protocol version on a
    /// thread; [`App::on_daemon_stopped`] connects afterwards, which starts
    /// this build's berthd through the usual launch path.
    fn restart_daemon(&mut self) {
        if !self.mismatch.restart() {
            return;
        }
        // Answers to attempts under way are dropped, and none starts until
        // the stop is done: it would meet the stopping daemon, or start a
        // berthd that finds the lock still held.
        self.generation += 1;
        self.connecting = false;
        self.reconnect_at = None;
        let proxy = self.proxy.clone();
        let paths = self.paths.clone();
        let spawned = std::thread::Builder::new()
            .name("berth-restart".into())
            .spawn(move || {
                let result =
                    client::stop_daemon(&paths, client::STOP_WAIT).map_err(|e| format!("{e:#}"));
                let _ = proxy.send_event(UserEvent::DaemonStopped(result));
            });
        if let Err(e) = spawned {
            self.on_daemon_stopped(Err(format!("无法启动重启线程：{e}")));
        }
    }

    fn on_daemon_stopped(&mut self, result: std::result::Result<Option<client::Stopped>, String>) {
        match &result {
            Ok(Some(s)) => tracing::info!(
                version = %s.version,
                protocol = s.protocol,
                pid = ?s.pid,
                "stopped berthd for the restart"
            ),
            Ok(None) => tracing::info!("berthd was gone before the restart stopped it"),
            Err(e) => tracing::warn!(error = %e, "stopping berthd failed"),
        }
        self.mismatch.stopped(result.map(|_| ()));
        if matches!(self.mismatch, Mismatch::Starting(_)) {
            self.attempts = 0;
            self.start_connect();
        } else {
            self.schedule_reconnect();
        }
    }

    fn schedule_reconnect(&mut self) {
        let delay = Duration::from_secs(1u64 << self.attempts.min(4)).min(RECONNECT_MAX);
        self.attempts += 1;
        self.reconnect_at = Some(Instant::now() + delay);
    }

    fn on_user_event(&mut self, event: UserEvent) {
        match event {
            UserEvent::Connected {
                generation,
                client,
                launched,
            } if generation == self.generation => {
                self.connecting = false;
                self.attempts = 0;
                self.last_connect_error = None;
                tracing::info!(
                    daemon = client.daemon_version(),
                    launched,
                    "connected to berthd"
                );
                match self.mismatch.connected() {
                    Some(restarted) => self.ctl.info(restarted),
                    None if launched => self.ctl.info("已启动 berthd"),
                    None => {}
                }
                self.client = Some(*client);
                self.ctl.on_connected(out!(self));
                let early = std::mem::take(&mut self.early);
                for ev in early {
                    self.on_daemon(ev);
                }
            }
            UserEvent::ConnectFailed {
                generation,
                error,
                refused,
            } if generation == self.generation => {
                self.connecting = false;
                self.mismatch.connect_failed(refused);
                // A refusal is explained by the mismatch banner instead.
                if refused.is_none() && self.last_connect_error.as_deref() != Some(error.as_str()) {
                    self.ctl.error(format!("无法连接 berthd：{error}"));
                }
                self.last_connect_error = Some(error);
                self.schedule_reconnect();
                if self.opts.demo_restart && matches!(self.mismatch, Mismatch::Seen(_)) {
                    self.restart_daemon();
                }
            }
            UserEvent::DaemonStopped(result) => self.on_daemon_stopped(result),
            UserEvent::Daemon { generation, event } if generation == self.generation => {
                if self.client.is_none() {
                    self.early.push(event);
                } else {
                    self.on_daemon(event);
                }
            }
            UserEvent::Connected { .. }
            | UserEvent::ConnectFailed { .. }
            | UserEvent::Daemon { .. } => {} // an older connection
            UserEvent::FolderPicked(result) => {
                self.picking_folder = false;
                match result {
                    Ok(Some(dir)) => self.ctl.new_workspace(out!(self), dir),
                    Ok(None) => {}
                    Err(e) => self.ctl.error(format!("目录选择失败：{e}")),
                }
            }
        }
        self.redraw_soon();
    }

    fn on_daemon(&mut self, event: ClientEvent) {
        match event {
            ClientEvent::Msg(msg) => {
                let effects = self.ctl.handle(out!(self), *msg, Instant::now());
                for Effect::Notify { title, body } in effects {
                    if let Some(n) = &self.notifier {
                        n.send(title, body);
                    }
                }
            }
            ClientEvent::Closed(reason) => {
                self.client = None;
                self.ctl.on_disconnected(&reason);
                self.schedule_reconnect();
            }
        }
        self.update_dock();
    }

    /// Dock badge: sessions needing attention (none while disconnected:
    /// the list may be out of date).
    fn update_dock(&mut self) {
        let count = if self.ctl.is_connected() {
            self.ctl.attention_count()
        } else {
            0
        };
        if let Some(dock) = self.dock.as_mut() {
            dock.update(count);
        }
    }

    /// `--demo-hover` resolved against the session list.
    fn demo_hover(&self) -> Option<berth_core::SessionId> {
        let want = self.opts.demo_hover.as_deref()?;
        self.ctl.find_session(want).ok()
    }

    // -- keyboard ----------------------------------------------------------

    fn on_key(&mut self, event_loop: &ActiveEventLoop, press: KeyPress) {
        let Some(gfx) = self.gfx.as_mut() else { return };
        let now = Instant::now();
        // Open dialogs take Enter / Esc; ⌘Q still quits.
        if press.pressed && (self.ctl.confirm().is_some() || self.palette_open) {
            if press.mods.super_key()
                && matches!(press.logical, WinitKey::Character(c) if c.eq_ignore_ascii_case("q"))
            {
                self.quit(event_loop);
                return;
            }
            match press.logical {
                WinitKey::Named(NamedKey::Escape) => {
                    if self.ctl.confirm().is_some() {
                        self.ctl.answer_confirm(out!(self), false);
                    } else {
                        self.palette_open = false;
                    }
                }
                WinitKey::Named(NamedKey::Enter) if self.ctl.confirm().is_some() => {
                    self.ctl.answer_confirm(out!(self), true);
                }
                _ => {}
            }
            self.redraw_soon();
            return;
        }
        let gate = ImeGate {
            enabled: gfx.ime.enabled(),
            composing: gfx.ime.preedit().is_some(),
        };
        let modes = self.ctl.view().map(|v| v.modes()).unwrap_or_default();
        match input::decide_key(&press, gate, modes) {
            KeyAction::Forward { desc, bytes } => {
                if gfx.ime.debug() {
                    eprintln!("[key] {desc} -> {:?}", input::caret_notation(&bytes));
                }
                gfx.blink_epoch = now;
                self.ctl.input(out!(self), bytes, now);
            }
            KeyAction::Scroll(key) => {
                let page = self.ctl.view().map_or(1, |v| v.page());
                let lines = match key {
                    ScrollKey::PageUp => page,
                    ScrollKey::PageDown => -page,
                    ScrollKey::Top => i64::MAX / 2,
                    ScrollKey::Bottom => i64::MIN / 2,
                };
                self.ctl.scroll(out!(self), lines, now);
            }
            KeyAction::Shortcut(s) => self.on_shortcut(event_loop, s),
            KeyAction::SwallowedByIme { desc } => {
                if gfx.ime.debug() {
                    eprintln!("[key] {desc} swallowed by the IME composition");
                }
            }
            KeyAction::Ignore => {}
        }
        self.redraw_soon();
    }

    fn on_shortcut(&mut self, event_loop: &ActiveEventLoop, s: Shortcut) {
        let now = Instant::now();
        match s {
            Shortcut::Quit => self.quit(event_loop),
            Shortcut::NewSession => self.ctl.new_session(out!(self)),
            Shortcut::NewWorkspace => self.pick_folder(),
            Shortcut::Close => self.ctl.request_close(out!(self)),
            Shortcut::Jump(n) => self.ctl.jump(out!(self), usize::from(n), now),
            Shortcut::Palette => self.palette_open = !self.palette_open,
            Shortcut::Copy => {
                if let Some(text) = self.ctl.copy_text() {
                    if let Err(e) = arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                        self.ctl.error(format!("无法写入剪贴板：{e}"));
                    }
                }
            }
            Shortcut::Paste => match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                Ok(text) => self.ctl.paste(out!(self), &text, now),
                Err(arboard::Error::ContentNotAvailable) => self.ctl.info("剪贴板里没有文本"),
                Err(e) => self.ctl.error(format!("无法读取剪贴板：{e}")),
            },
            Shortcut::Unbound(chord) => {
                tracing::debug!(chord, "unbound GUI shortcut");
            }
        }
    }

    /// ⌘⇧N: macOS folder chooser on a background thread.
    fn pick_folder(&mut self) {
        if self.picking_folder {
            return;
        }
        self.picking_folder = true;
        let proxy = self.proxy.clone();
        let spawned = std::thread::Builder::new()
            .name("berth-folder-picker".into())
            .spawn(move || {
                let _ = proxy.send_event(UserEvent::FolderPicked(choose_folder()));
            });
        if let Err(e) = spawned {
            self.picking_folder = false;
            self.ctl.error(format!("无法打开目录选择：{e}"));
        }
    }

    // -- mouse ---------------------------------------------------------------

    /// Pointer events for the terminal area (egui got them first).
    fn on_pointer(&mut self, event: &WindowEvent) {
        let Some(gfx) = self.gfx.as_mut() else { return };
        if let WindowEvent::CursorMoved { position, .. } = event {
            gfx.mouse.pos = Some(*position);
        }
        let dialog = self.ctl.confirm().is_some() || self.palette_open;
        let over_egui = gfx.sidebar.wants_pointer();
        let now = Instant::now();
        let Some(pos) = gfx.mouse.pos else { return };
        let in_grid = gfx.in_grid(pos);
        let Some(view) = self.ctl.view() else { return };
        if !view.has_screen() {
            return;
        }
        let modes = view.modes();
        let dims = view.dims();
        let cols = gfx.layout.cols.min(dims.cols);
        let rows = gfx.layout.rows.min(dims.rows);
        let m = gfx.grid.metrics();
        let (col, row) = mouse::cell_at(
            pos.x - f64::from(gfx.layout.origin[0]),
            pos.y - f64::from(gfx.layout.origin[1]),
            f64::from(m.cell_w),
            f64::from(m.cell_h),
            cols,
            rows,
        );
        let mods = Mods::from_winit(gfx.mods, true);
        // ⇧ forces local selection even when the program reads the mouse.
        let reporting = modes.mouse_reporting() && !mods.shift;
        let top = view.top_line();
        match event {
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => Button::Left,
                    MouseButton::Middle => Button::Middle,
                    MouseButton::Right => Button::Right,
                    _ => return,
                };
                let pressed = *state == ElementState::Pressed;
                if pressed && (dialog || over_egui || !in_grid) {
                    return;
                }
                if pressed {
                    gfx.mouse.held = Some(b);
                } else if gfx.mouse.held == Some(b) {
                    gfx.mouse.held = None;
                }
                if reporting || (!pressed && gfx.mouse.reported_press) {
                    gfx.mouse.reported_press = pressed;
                    gfx.mouse.last_cell = Some((col, row));
                    let ev = if pressed {
                        MouseEvent::Press(b)
                    } else {
                        MouseEvent::Release(b)
                    };
                    if let Some(bytes) = mouse::report(ev, col, row, mods, modes) {
                        self.ctl.report(out!(self), bytes, now);
                    }
                    return;
                }
                if b != Button::Left {
                    return;
                }
                if pressed {
                    let count = match gfx.mouse.last_click {
                        Some((t, cell, n)) if now - t < MULTI_CLICK && cell == (col, row) => {
                            n % 3 + 1
                        }
                        _ => 1,
                    };
                    gfx.mouse.last_click = Some((now, (col, row), count));
                    let kind = match count {
                        2 => SelectionKind::Semantic,
                        3 => SelectionKind::Lines,
                        _ if gfx.mods.alt_key() => SelectionKind::Block,
                        _ => SelectionKind::Simple,
                    };
                    gfx.mouse.selecting = true;
                    let point = Point::new(top + u64::from(row), col);
                    if let Some(v) = self.ctl.view_mut() {
                        v.selection = Some(Selection::new(kind, point));
                    }
                } else if gfx.mouse.selecting {
                    gfx.mouse.selecting = false;
                    if let Some(v) = self.ctl.view_mut() {
                        if v.selection.as_ref().is_some_and(Selection::is_empty) {
                            v.selection = None;
                        }
                    }
                }
                gfx.dirty = true;
            }
            WindowEvent::CursorMoved { .. } => {
                if gfx.mouse.selecting {
                    let point = Point::new(top + u64::from(row), col);
                    if let Some(v) = self.ctl.view_mut() {
                        if let Some(sel) = v.selection.as_mut() {
                            sel.update(point);
                        }
                    }
                    gfx.dirty = true;
                    return;
                }
                if !reporting || dialog || over_egui || !in_grid {
                    return;
                }
                if gfx.mouse.last_cell == Some((col, row)) {
                    return;
                }
                gfx.mouse.last_cell = Some((col, row));
                let ev = MouseEvent::Motion {
                    held: gfx.mouse.held,
                };
                if let Some(bytes) = mouse::report(ev, col, row, mods, modes) {
                    self.ctl.report(out!(self), bytes, now);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if dialog || over_egui || !in_grid {
                    return;
                }
                let lines = gfx.mouse.wheel.lines(*delta, f64::from(m.cell_h));
                if lines == 0 {
                    return;
                }
                if reporting {
                    let ev = if lines > 0 {
                        MouseEvent::WheelUp
                    } else {
                        MouseEvent::WheelDown
                    };
                    let mut bytes = Vec::new();
                    for _ in 0..lines.unsigned_abs().min(20) {
                        if let Some(b) = mouse::report(ev, col, row, mods, modes) {
                            bytes.extend(b);
                        }
                    }
                    self.ctl.report(out!(self), bytes, now);
                } else if let Some(bytes) = mouse::alternate_scroll(lines, modes) {
                    self.ctl.report(out!(self), bytes, now);
                } else {
                    self.ctl.scroll(out!(self), i64::from(lines), now);
                    gfx.dirty = true;
                }
            }
            _ => {}
        }
    }

    // -- frames ------------------------------------------------------------

    fn placeholder(&self) -> Option<String> {
        if self.is_bench() {
            return None;
        }
        if !self.ctl.is_connected() {
            return Some(match &self.last_connect_error {
                Some(e) if !self.connecting => format!("未连接 berthd：{e}"),
                _ => "正在连接 berthd…".into(),
            });
        }
        if !self.ctl.is_loaded() {
            return Some("正在读取 session 列表…".into());
        }
        match self.ctl.focused() {
            None => Some("没有 session：按 ⌘N 新建".into()),
            Some(_) if !self.ctl.view().is_some_and(|v| v.has_screen()) => {
                Some("正在打开 session…".into())
            }
            Some(_) => None,
        }
    }

    fn status_line(&self) -> Option<String> {
        if self.is_bench() || self.ctl.is_connected() {
            return None;
        }
        if let Some(status) = self.mismatch.status() {
            return Some(status.into());
        }
        Some(match (&self.last_connect_error, self.reconnect_at) {
            (_, Some(at)) => format!(
                "未连接，{} s 后重试",
                at.saturating_duration_since(Instant::now()).as_secs() + 1
            ),
            _ => "正在连接 berthd…".into(),
        })
    }

    fn window_title(&self) -> String {
        match self.ctl.focused() {
            Some(sid) => format!("{} — berth", self.ctl.qualified_title(sid)),
            None => "berth".into(),
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let start = Instant::now();
        let demo_hover = self.demo_hover();
        let placeholder = self.placeholder();
        let mismatch = self.mismatch.banner();
        let status = self.status_line();
        let title = self.window_title();
        let Some(gfx) = self.gfx.as_mut() else {
            return Ok(());
        };
        gfx.dirty = false;
        if gfx.title != title {
            gfx.window.set_title(&title);
            gfx.title = title;
        }
        if let Some(v) = self.ctl.view_mut() {
            v.set_cursor_override(self.opts.cursor_style);
        }
        let focused = gfx.focused || gfx.force_focused;
        let since_input = start.saturating_duration_since(gfx.blink_epoch);
        let blinking =
            self.ctl.view().is_some_and(|v| v.has_screen()) && gfx.cursor_blinking && focused;
        let cursor_alpha = match &self.mode {
            Mode::Screenshot { .. } | Mode::Exiting => 1.0,
            _ if !blinking => 1.0,
            Mode::Bench { .. } => {
                let t = since_input.as_secs_f32() / (2.0 * BLINK.as_secs_f32());
                0.5 + 0.5 * (t * std::f32::consts::TAU).cos()
            }
            Mode::Interactive => {
                if (since_input.as_millis() / BLINK.as_millis()).is_multiple_of(2) {
                    1.0
                } else {
                    0.0
                }
            }
        };
        let reshape = matches!(&self.mode, Mode::Bench { current, .. } if current.reshape);
        let gpu_sync = matches!(self.mode, Mode::Bench { .. }) || self.live_stats.is_some();
        let capture = matches!(
            &self.mode,
            Mode::Screenshot { ready_since: Some(t), .. } if start >= *t + self.opts.screenshot_delay
        );
        let allow_offscreen =
            !matches!(self.mode, Mode::Interactive | Mode::Exiting) || self.live_stats.is_some();
        if reshape {
            gfx.grid.text.clear_shape_cache();
        }
        let chrome = Chrome {
            grid_rect: gfx.grid_rect_points(),
            palette_open: self.palette_open,
            status: status.as_deref(),
            placeholder: placeholder.as_deref(),
            mismatch: mismatch.as_ref(),
            demo_hover,
        };
        let fixture = match &self.mode {
            Mode::Bench { fixture, .. } => Some(fixture.as_ref()),
            _ => None,
        };
        let rendered = gfx.render_frame(
            &mut self.ctl,
            fixture,
            &chrome,
            cursor_alpha,
            gpu_sync,
            capture,
            allow_offscreen,
        )?;
        gfx.last_frame = Instant::now();
        let (frame, actions) = rendered;
        let Some((timing, captured)) = frame else {
            // No drawable this time (occluded); retried on the next redraw.
            self.apply_ui(actions);
            return Ok(());
        };
        gfx.frames += 1;
        if gfx.frames == 1 {
            gfx.report_startup(self.t0, &timing);
        }
        gfx.cursor_blinking = self
            .ctl
            .view()
            .is_some_and(|v| v.has_screen() && v.cursor_blinking());
        self.apply_ui(actions);
        if let Some(live) = self.live_stats.as_mut() {
            if live.record(start, timing) {
                live.report(&traffic_label(&mut self.ctl));
            }
        }
        match &mut self.mode {
            Mode::Bench { current, .. } => {
                let until = *current
                    .until
                    .get_or_insert(start + Duration::from_secs_f64(current.secs));
                current.stats.record(start, timing);
                if Instant::now() >= until {
                    eprint!("{}", current.stats.report(&current.label, TARGET_MS));
                    if let Some(gfx) = self.gfx.as_ref() {
                        let (glyphs, mask, color, rebuilds) = gfx.grid.atlas_summary();
                        eprintln!(
                            "[stats]   atlas: {glyphs} glyphs, mask page {mask}², color page {color}², {rebuilds} rebuilds"
                        );
                    }
                    self.next_bench_phase(event_loop);
                }
            }
            Mode::Screenshot { path, .. } => {
                if let Some(cap) = captured {
                    let path = path.clone();
                    let (w, h, source) = (cap.width, cap.height, cap.source);
                    if let Some(gfx) = self.gfx.as_ref() {
                        gfx.finish_capture(cap, &path)?;
                    }
                    eprintln!(
                        "[screenshot] wrote {} ({w}×{h} px, read back from the {source} texture)",
                        path.display()
                    );
                    self.quit(event_loop);
                }
            }
            Mode::Interactive | Mode::Exiting => {}
        }
        Ok(())
    }

    fn next_bench_phase(&mut self, event_loop: &ActiveEventLoop) {
        if let Mode::Bench {
            current, pending, ..
        } = &mut self.mode
        {
            if let Some(next) = pending.pop_front() {
                *current = next;
                return;
            }
        }
        self.quit(event_loop);
    }

    fn apply_ui(&mut self, actions: Vec<UiAction>) {
        let now = Instant::now();
        let mut changed = false;
        for a in actions {
            // Reported every frame: no redraw of their own.
            changed |= !matches!(a, UiAction::Visible(_) | UiAction::Hover(_));
            match a {
                UiAction::Focus(sid) => self.ctl.focus(out!(self), sid, now),
                UiAction::Hover(sid) => {
                    if !self.is_bench() {
                        self.ctl.hover(out!(self), sid)
                    }
                }
                UiAction::Revive(sid, mode) => self.ctl.revive(out!(self), sid, mode, now),
                UiAction::NewSession => self.ctl.new_session(out!(self)),
                UiAction::NewSessionIn(ws) => self.ctl.new_session_in(out!(self), ws),
                UiAction::NewWorkspace => self.pick_folder(),
                UiAction::Confirm(yes) => self.ctl.answer_confirm(out!(self), yes),
                UiAction::DismissNotice(i) => self.ctl.dismiss_notice(i),
                UiAction::ClosePalette => self.palette_open = false,
                UiAction::RestartDaemon => self.restart_daemon(),
                UiAction::Visible(ids) => {
                    if !self.is_bench() {
                        self.ctl.set_visible(out!(self), &ids, now)
                    }
                }
            }
        }
        if changed {
            self.redraw_soon();
        }
    }

    fn screenshot_ready(&self) -> bool {
        match self.mismatch {
            // The banner is the picture, unless the restart is demonstrated.
            Mismatch::Seen(_) => return !self.opts.demo_restart,
            Mismatch::Failed { .. } => return true,
            Mismatch::Stopping(_) | Mismatch::Starting(_) => return false,
            Mismatch::None => {}
        }
        self.ctl.is_connected()
            && self.ctl.is_loaded()
            && self.pending_scroll.is_none()
            && match self.ctl.focused() {
                None => true,
                Some(_) => self.ctl.view().is_some_and(|v| v.visible_complete()),
            }
            && self.demo_hover_ready()
    }

    /// `--demo-hover`: the session is listed and its events arrived.
    fn demo_hover_ready(&self) -> bool {
        if self.opts.demo_hover.is_none() {
            return true;
        }
        self.demo_hover()
            .and_then(|sid| self.ctl.recent_events(sid))
            .is_some_and(|r| r.events.is_some() || r.error.is_some())
    }
}

/// `osascript` folder chooser (no extra dependency; returns None when
/// cancelled).
fn choose_folder() -> std::result::Result<Option<PathBuf>, String> {
    let out = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "POSIX path of (choose folder with prompt \"选择 workspace 目录\")",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("osascript: {e}"))?;
    if out.status.success() {
        let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if raw.is_empty() {
            return Ok(None);
        }
        let trimmed = raw.trim_end_matches('/');
        return Ok(Some(PathBuf::from(if trimmed.is_empty() {
            "/"
        } else {
            trimmed
        })));
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("-128") {
        Ok(None) // User canceled.
    } else {
        Err(err.trim().to_string())
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let resumed_at = Instant::now();
        match Gfx::new(event_loop, &self.config, &self.opts) {
            Ok(mut gfx) => {
                if let Some(started) = self.loop_started {
                    gfx.startup
                        .insert(0, ("launch (run_app → resumed)", resumed_at - started));
                }
                gfx.window.request_redraw();
                let now = Instant::now();
                self.ctl.set_grid(gfx.grid_dims(), now);
                self.gfx = Some(gfx);
                if let Some(w) = self.config_warning.take() {
                    self.ctl.error(w);
                }
                if !self.is_bench() {
                    self.start_connect();
                }
            }
            Err(e) => self.fail(event_loop, e.context("initialising the window and GPU")),
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        self.on_user_event(event);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gfx) = self.gfx.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => self.quit(event_loop),
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.redraw(event_loop) {
                    self.fail(event_loop, e);
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                let mods = gfx.mods;
                let press = KeyPress {
                    logical: &event.logical_key,
                    text: event.text.as_deref(),
                    pressed: event.state == ElementState::Pressed,
                    synthetic: is_synthetic,
                    mods,
                };
                self.on_key(event_loop, press);
            }
            WindowEvent::Ime(ime) => {
                if let ImeOutcome::Commit(text) = gfx.ime.handle(&ime) {
                    gfx.blink_epoch = Instant::now();
                    if self.ctl.confirm().is_none() && !self.palette_open {
                        self.ctl
                            .input(out!(self), text.into_bytes(), Instant::now());
                    }
                }
                if let Some(gfx) = self.gfx.as_mut() {
                    if gfx.ime.debug() {
                        eprintln!(
                            "[ime] state: enabled={} preedit={:?}",
                            gfx.ime.enabled(),
                            gfx.ime.preedit()
                        );
                    }
                    gfx.dirty = true;
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                gfx.mods = m.state();
                gfx.sidebar
                    .on_window_event(&gfx.window, &WindowEvent::ModifiersChanged(m));
            }
            WindowEvent::Occluded(occluded) => {
                gfx.occluded = occluded;
                if !occluded {
                    gfx.dirty = true;
                }
            }
            WindowEvent::Focused(focused) => {
                gfx.focused = focused;
                gfx.blink_epoch = Instant::now();
                gfx.dirty = true;
                gfx.sidebar
                    .on_window_event(&gfx.window, &WindowEvent::Focused(focused));
                let modes = self.ctl.view().map(|v| v.modes()).unwrap_or_default();
                if modes.contains(TermModes::FOCUS_IN_OUT) {
                    let seq = if focused { b"\x1b[I" } else { b"\x1b[O" };
                    self.ctl.report(out!(self), seq.to_vec(), Instant::now());
                }
                self.ctl.set_window_focused(out!(self), focused);
            }
            WindowEvent::Resized(size) => {
                gfx.resize(size);
                gfx.sidebar
                    .on_window_event(&gfx.window, &WindowEvent::Resized(size));
                gfx.dirty = true;
                let dims = gfx.grid_dims();
                self.ctl.set_grid(dims, Instant::now());
            }
            WindowEvent::ScaleFactorChanged {
                scale_factor,
                inner_size_writer,
            } => {
                gfx.scale = scale_factor as f32;
                gfx.grid.set_scale(gfx.scale);
                gfx.update_layout();
                let ev = WindowEvent::ScaleFactorChanged {
                    scale_factor,
                    inner_size_writer,
                };
                gfx.sidebar.on_window_event(&gfx.window, &ev);
                gfx.dirty = true;
                let dims = gfx.grid_dims();
                self.ctl.set_grid(dims, Instant::now());
            }
            other => {
                // Pointer events: egui first (sidebar, overlays, dialogs),
                // then the terminal unless egui wants them. Keyboard and IME
                // events never reach egui: the terminal owns them.
                if gfx.sidebar.on_window_event(&gfx.window, &other) {
                    gfx.dirty = true;
                }
                if matches!(
                    other,
                    WindowEvent::CursorMoved { .. }
                        | WindowEvent::MouseInput { .. }
                        | WindowEvent::MouseWheel { .. }
                ) {
                    self.on_pointer(&other);
                }
                if let WindowEvent::CursorLeft { .. } = other {
                    gfx_mouse_left(self.gfx.as_mut());
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if self.exit_at.is_some_and(|t| now >= t) {
            self.quit(event_loop);
            return;
        }
        if self.reconnect_at.is_some_and(|t| now >= t) && !self.connecting {
            self.reconnect_at = None;
            self.start_connect();
        }
        if !self.is_bench() {
            self.ctl.tick(out!(self), now);
            if let Some(lines) = self.pending_scroll {
                if self.ctl.view().is_some_and(|v| v.has_screen()) {
                    self.pending_scroll = None;
                    self.ctl.scroll(out!(self), lines, now);
                    self.redraw_soon();
                }
            }
        }
        let ready = self.screenshot_ready();
        if let Mode::Screenshot {
            ready_since,
            deadline,
            ..
        } = &mut self.mode
        {
            if now > *deadline {
                let what = if self.mismatch.status().is_some() {
                    "the berthd restart did not finish"
                } else if !self.ctl.is_connected() {
                    "berthd could not be reached"
                } else if self.opts.demo_hover.is_some() && self.ctl.is_loaded() {
                    "the --demo-hover session (a live one, by id prefix) or its events did not arrive"
                } else {
                    "the session list or the focused screen did not arrive"
                };
                self.fail(
                    event_loop,
                    anyhow!("screenshot: {what} within {SCREENSHOT_TIMEOUT:?}"),
                );
                return;
            }
            if ready && ready_since.is_none() {
                *ready_since = Some(now);
            }
        }
        let Some(gfx) = self.gfx.as_mut() else { return };
        match &self.mode {
            Mode::Exiting => event_loop.exit(),
            Mode::Bench { .. } => {
                event_loop.set_control_flow(ControlFlow::Poll);
                gfx.window.request_redraw();
            }
            Mode::Screenshot { .. } => {
                // Keep frames coming (offscreen when locked) until captured.
                let next = gfx.last_frame + OFFSCREEN_INTERVAL.max(Duration::from_millis(16));
                if now >= next {
                    gfx.window.request_redraw();
                }
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    next.max(now + Duration::from_millis(1)),
                ));
            }
            Mode::Interactive => {
                let timers = [self.ctl.next_deadline(), self.reconnect_at, self.exit_at]
                    .into_iter()
                    .flatten()
                    .min();
                let pace = if gfx.occluded && self.live_stats.is_none() {
                    // Nothing is visible: sleep until an event (e.g.
                    // un-occlusion) or a controller timer.
                    Pace::Sleep(timers)
                } else {
                    let frame_deadline = gfx.next_deadline();
                    pace(
                        now,
                        gfx.dirty || now >= frame_deadline,
                        gfx.presented_last,
                        gfx.last_frame,
                        frame_deadline,
                        timers,
                    )
                };
                event_loop.set_control_flow(match pace {
                    Pace::Redraw => {
                        gfx.window.request_redraw();
                        ControlFlow::Wait
                    }
                    Pace::Sleep(Some(t)) => {
                        ControlFlow::WaitUntil(t.max(now + Duration::from_millis(1)))
                    }
                    Pace::Sleep(None) => ControlFlow::Wait,
                });
            }
        }
    }
}

/// What the interactive loop does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pace {
    Redraw,
    /// Until this instant (`None`: until an event).
    Sleep(Option<Instant>),
}

/// `frame_due`: something changed, or a frame timer (cursor blink, sidebar
/// tick, an egui animation) fired; `frame_deadline` is that timer when it
/// has not. Presented frames are paced by vsync; frames that were not
/// presented (offscreen while occluded) have no back-pressure, so they are
/// spaced by [`OFFSCREEN_INTERVAL`] whatever asked for them — egui
/// animations advance by wall-clock time and would otherwise ask for
/// hundreds of back-to-back frames.
fn pace(
    now: Instant,
    frame_due: bool,
    presented_last: bool,
    last_frame: Instant,
    frame_deadline: Instant,
    timers: Option<Instant>,
) -> Pace {
    let wake = |t: Instant| Some(timers.map_or(t, |o| o.min(t)));
    if !frame_due {
        return Pace::Sleep(wake(frame_deadline));
    }
    let earliest = if presented_last {
        now
    } else {
        last_frame + OFFSCREEN_INTERVAL
    };
    if now >= earliest {
        Pace::Redraw
    } else {
        Pace::Sleep(wake(earliest))
    }
}

fn gfx_mouse_left(gfx: Option<&mut Gfx>) {
    if let Some(gfx) = gfx {
        gfx.mouse.last_cell = None;
        gfx.mouse.wheel.reset();
    }
}

/// Pointer state for the terminal area.
#[derive(Default)]
struct MouseState {
    /// Last pointer position (physical px).
    pos: Option<PhysicalPosition<f64>>,
    held: Option<Button>,
    /// The press went to the program: its release does too.
    reported_press: bool,
    /// Last cell a motion was reported for.
    last_cell: Option<(u16, u16)>,
    /// A local selection drag is in progress.
    selecting: bool,
    /// Time, cell and count of the last click (double / triple click).
    last_click: Option<(Instant, (u16, u16), u8)>,
    wheel: WheelAccum,
}

/// A frame's timing and capture (when one was drawn) plus the sidebar's
/// actions.
type Rendered = (Option<(FrameTiming, Option<Capture>)>, Vec<UiAction>);

/// Pending surface readback for `--screenshot`.
struct Capture {
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    padded_bytes_per_row: u32,
    format: wgpu::TextureFormat,
    source: &'static str,
}

/// Window + GPU + everything drawn into it.
struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
    surface_copy_src: bool,
    adapter: String,
    grid: GridRenderer,
    sidebar: Sidebar,
    theme: Theme,
    ime: ImeState,
    mods: ModifiersState,
    focused: bool,
    scale: f32,
    sidebar_width: f32,
    layout: GridLayout,
    blink_epoch: Instant,
    anim_epoch: Instant,
    last_frame: Instant,
    ime_area: Option<[i32; 4]>,
    frames: u64,
    startup: Vec<(&'static str, Duration)>,
    last_prepare: PrepareStats,
    offscreen: Option<(wgpu::Texture, wgpu::TextureView, [u32; 2])>,
    warned_occluded: bool,
    /// Screenshots render the focused look regardless of window focus
    /// (unavailable while the screen is locked).
    force_focused: bool,
    /// `WindowEvent::Occluded`: idle rendering pauses while hidden.
    occluded: bool,
    /// Something changed since the last frame.
    dirty: bool,
    /// The last frame was presented (vsync paces the next one).
    presented_last: bool,
    cursor_blinking: bool,
    title: String,
    mouse: MouseState,
    empty: ScreenSnapshot,
    empty_styles: StyleTable,
}

fn content_size(
    m: &CellMetrics,
    scale: f32,
    sidebar_width: f32,
    cells: (u16, u16),
) -> PhysicalSize<u32> {
    let pad = (PADDING_PT * scale).round() as u32;
    let sidebar = (sidebar_width * scale).round() as u32;
    PhysicalSize::new(
        sidebar + 2 * pad + u32::from(cells.0) * m.cell_w,
        2 * pad + u32::from(cells.1) * m.cell_h,
    )
}

fn next_multiple(epoch: Instant, after: Instant, period: Duration) -> Instant {
    let elapsed = after.saturating_duration_since(epoch).as_nanos();
    let n = elapsed / period.as_nanos() + 1;
    epoch + Duration::from_nanos((n * period.as_nanos()) as u64)
}

impl Gfx {
    fn new(event_loop: &ActiveEventLoop, config: &Config, opts: &GuiOptions) -> Result<Self> {
        let mut startup = Vec::new();
        let mut lap = Instant::now();
        let mut mark = |name: &'static str, startup: &mut Vec<(&'static str, Duration)>| {
            let now = Instant::now();
            startup.push((name, now - lap));
            lap = now;
        };

        let attrs = Window::default_attributes()
            .with_title("berth")
            .with_inner_size(LogicalSize::new(1244.0, 624.0))
            .with_visible(false);
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .context("creating the window")?,
        );
        mark("window", &mut startup);

        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(window.clone())
            .context("creating the surface")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .context("no GPU adapter compatible with the window surface")?;
        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("berth"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .context("requesting the GPU device")?;
        let caps = surface.get_capabilities(&adapter);
        // Non-sRGB target: blending happens in gamma space like Ghostty's
        // native renderer; colors are written as-is.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| {
                matches!(
                    f,
                    wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
                )
            })
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| anyhow!("the surface reports no texture formats"))?;
        if format.is_srgb() {
            tracing::warn!(
                ?format,
                "no linear 8-bit surface format; text blending will differ from Ghostty"
            );
        }
        let present_mode = if opts.no_vsync
            && caps.present_modes.contains(&wgpu::PresentMode::Immediate)
        {
            wgpu::PresentMode::Immediate
        } else {
            if opts.no_vsync {
                tracing::warn!(modes = ?caps.present_modes, "Immediate present mode unavailable; using Fifo");
            }
            wgpu::PresentMode::Fifo
        };
        let surface_copy_src =
            opts.screenshot.is_some() && caps.usages.contains(wgpu::TextureUsages::COPY_SRC);
        let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        if surface_copy_src {
            usage |= wgpu::TextureUsages::COPY_SRC;
        }
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto)
        };
        mark("gpu", &mut startup);

        let scale = window.scale_factor() as f32;
        let grid = GridRenderer::new(&device, format, &config.font, scale)?;
        mark("fonts+grid", &mut startup);

        let cells = opts.cells.unwrap_or((COLS, ROWS));
        let size = content_size(&grid.metrics(), scale, config.sidebar_width, cells);
        let _ = window.request_inner_size(size);
        let actual = window.inner_size();
        let surface_config = wgpu::SurfaceConfiguration {
            usage,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: actual.width.max(1),
            height: actual.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: Vec::new(),
        };
        surface.configure(&device, &surface_config);

        let theme = Theme::ghostty_default();
        let mut grid = grid;
        let family = grid.text.family().to_string();
        let sidebar = Sidebar::new(
            &window,
            &device,
            format,
            grid.text.font_system(),
            &family,
            config.sidebar_width,
            &theme,
        );
        mark("sidebar", &mut startup);

        window.set_ime_allowed(true);
        window.set_ime_purpose(ImePurpose::Terminal);
        window.set_visible(true);
        window.focus_window();

        let now = Instant::now();
        let mut gfx = Self {
            window,
            surface,
            device,
            queue,
            surface_config,
            surface_copy_src,
            adapter: format!("{} ({:?}, {:?})", info.name, info.backend, info.device_type),
            grid,
            sidebar,
            theme,
            ime: ImeState::from_env(),
            mods: ModifiersState::empty(),
            focused: true,
            scale,
            sidebar_width: config.sidebar_width,
            layout: GridLayout {
                origin: [0.0, 0.0],
                cols: 0,
                rows: 0,
            },
            blink_epoch: now,
            anim_epoch: now,
            last_frame: now,
            ime_area: None,
            frames: 0,
            startup,
            last_prepare: PrepareStats::default(),
            offscreen: None,
            warned_occluded: false,
            force_focused: opts.screenshot.is_some(),
            occluded: false,
            dirty: true,
            presented_last: true,
            cursor_blinking: false,
            title: String::new(),
            mouse: MouseState::default(),
            empty: ScreenSnapshot::default(),
            empty_styles: StyleTable::new(),
        };
        gfx.update_layout();
        if let Some(text) = &opts.demo_preedit {
            gfx.ime.handle(&Ime::Enabled);
            gfx.ime
                .handle(&Ime::Preedit(text.clone(), Some((text.len(), text.len()))));
            eprintln!("[ime] synthetic Ime::Preedit({text:?}) injected through ImeState::handle (demo only)");
        }
        if gfx.ime.debug() {
            eprintln!("[ime] debug on: set_ime_allowed(true), purpose Terminal");
        }
        Ok(gfx)
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        if (size.width, size.height) != (self.surface_config.width, self.surface_config.height) {
            self.surface_config.width = size.width;
            self.surface_config.height = size.height;
            self.surface.configure(&self.device, &self.surface_config);
        }
        self.update_layout();
    }

    /// Recompute the grid origin and how many cells fit (HiDPI-aware).
    fn update_layout(&mut self) {
        let m = self.grid.metrics();
        let pad = (PADDING_PT * self.scale).round();
        let sidebar = (self.sidebar_width * self.scale).round();
        let w = self.surface_config.width as f32 - sidebar - 2.0 * pad;
        let h = self.surface_config.height as f32 - 2.0 * pad;
        let cols = (w / m.cell_w as f32).floor().clamp(0.0, u16::MAX as f32) as u16;
        let rows = (h / m.cell_h as f32).floor().clamp(0.0, u16::MAX as f32) as u16;
        let layout = GridLayout {
            origin: [sidebar + pad, pad],
            cols,
            rows,
        };
        if layout != self.layout {
            if (layout.cols, layout.rows) != (self.layout.cols, self.layout.rows) {
                tracing::info!(cols, rows, scale = self.scale, "grid size");
            }
            self.layout = layout;
        }
    }

    fn grid_dims(&self) -> Dims {
        Dims {
            cols: self.layout.cols,
            rows: self.layout.rows,
        }
    }

    /// The terminal area in logical points (for egui overlays).
    fn grid_rect_points(&self) -> egui::Rect {
        let s = self.scale.max(0.1);
        let x0 = (self.sidebar_width * self.scale).round() / s;
        egui::Rect::from_min_max(
            egui::pos2(x0, 0.0),
            egui::pos2(
                self.surface_config.width as f32 / s,
                self.surface_config.height as f32 / s,
            ),
        )
    }

    fn in_grid(&self, pos: PhysicalPosition<f64>) -> bool {
        let x0 = f64::from((self.sidebar_width * self.scale).round());
        pos.x >= x0
            && pos.x < f64::from(self.surface_config.width)
            && pos.y >= 0.0
            && pos.y < f64::from(self.surface_config.height)
    }

    /// Next idle wake-up: cursor blink toggle, sidebar tick, or egui's request.
    fn next_deadline(&self) -> Instant {
        let base = self.last_frame;
        let mut next = next_multiple(self.anim_epoch, base, SIDEBAR_TICK);
        if self.cursor_blinking && self.focused {
            next = next.min(next_multiple(self.blink_epoch, base, BLINK));
        }
        if let Some(delay) = self
            .sidebar
            .repaint_delay()
            .filter(|d| *d < Duration::from_secs(60))
        {
            next = next.min(base + delay);
        }
        next
    }

    /// Render one frame. The frame part is `None` when no drawable was
    /// available and offscreen rendering is not allowed; the sidebar's
    /// actions are returned either way (egui ran).
    ///
    /// `allow_offscreen`: when the window is occluded (screen locked, display
    /// asleep, window hidden) wgpu 30's Metal backend refuses `nextDrawable`;
    /// screenshot, stats and bench modes then render the identical frame
    /// into an offscreen texture of the surface's format and size,
    /// synchronised with the GPU, and skip only the present.
    #[allow(clippy::too_many_arguments)]
    fn render_frame(
        &mut self,
        ctl: &mut Controller,
        fixture: Option<&Fixture>,
        chrome: &Chrome,
        cursor_alpha: f32,
        gpu_sync: bool,
        capture: bool,
        allow_offscreen: bool,
    ) -> Result<Rendered> {
        let t0 = Instant::now();
        let size = [self.surface_config.width, self.surface_config.height];
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        let (sidebar_cmds, actions) = self.sidebar.prepare(
            &self.window,
            &self.device,
            &self.queue,
            &mut encoder,
            size,
            ctl,
            chrome,
            berth_core::now_ms(),
        );
        let focused = self.focused || self.force_focused;
        let fixture_spans;
        let session_spans;
        let input = match fixture {
            Some(f) => {
                fixture_spans = f.selection.map(|s| s.spans());
                FrameInput {
                    screen: &f.screen,
                    styles: f.interner.table(),
                    theme: &self.theme,
                    selection: fixture_spans.as_ref(),
                    cursor_alpha,
                    focused,
                    preedit: self.ime.preedit(),
                }
            }
            None => match ctl.view_mut().filter(|v| v.has_screen()) {
                Some(view) => {
                    session_spans = view.selection_spans();
                    let (screen, styles) = view.frame();
                    FrameInput {
                        screen,
                        styles,
                        theme: &self.theme,
                        selection: session_spans.as_ref(),
                        cursor_alpha,
                        focused,
                        preedit: self.ime.preedit(),
                    }
                }
                None => FrameInput {
                    screen: &self.empty,
                    styles: &self.empty_styles,
                    theme: &self.theme,
                    selection: None,
                    cursor_alpha,
                    focused,
                    preedit: None,
                },
            },
        };
        self.last_prepare =
            self.grid
                .prepare(&self.device, &self.queue, &input, &self.layout, size);
        let caret = self.grid.ime_caret_rect(&input, &self.layout);
        let t_prepared = Instant::now();

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => Some(f),
            status @ (wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded) => {
                if !allow_offscreen {
                    return Ok((None, actions));
                }
                if !self.warned_occluded {
                    self.warned_occluded = true;
                    let status = if matches!(status, wgpu::CurrentSurfaceTexture::Occluded) {
                        "Occluded"
                    } else {
                        "Timeout"
                    };
                    eprintln!(
                        "[warn] surface returned {status} (window occluded: screen locked / display asleep / hidden); \
                         rendering the same frames offscreen (same device, pipelines and format), present skipped"
                    );
                }
                None
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                tracing::warn!("surface outdated or lost; reconfiguring");
                self.surface.configure(&self.device, &self.surface_config);
                return Ok((None, actions));
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                bail!("surface validation error while acquiring a frame")
            }
        };
        let t_acquired = Instant::now();
        let offscreen = frame.is_none();

        let bg = self.theme.background;
        let clear = wgpu::Color {
            r: bg[0] as f64 / 255.0,
            g: bg[1] as f64 / 255.0,
            b: bg[2] as f64 / 255.0,
            a: 1.0,
        };
        let mut captured = None;
        match &frame {
            Some(f) => {
                let view = f
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                self.encode_pass(&mut encoder, &view, clear);
                if capture {
                    if self.surface_copy_src {
                        captured =
                            Some(self.encode_capture(&mut encoder, &f.texture, "surface")?);
                    } else {
                        self.ensure_offscreen();
                        let (texture, view) = self
                            .offscreen
                            .as_ref()
                            .map(|o| (&o.0, &o.1))
                            .expect("offscreen target");
                        self.encode_pass(&mut encoder, view, clear);
                        captured = Some(self.encode_capture(
                            &mut encoder,
                            texture,
                            "offscreen (surface lacks COPY_SRC)",
                        )?);
                    }
                }
            }
            None => {
                self.ensure_offscreen();
                let (texture, view) = self
                    .offscreen
                    .as_ref()
                    .map(|o| (&o.0, &o.1))
                    .expect("offscreen target");
                self.encode_pass(&mut encoder, view, clear);
                if capture {
                    captured = Some(self.encode_capture(
                        &mut encoder,
                        texture,
                        "offscreen (window occluded)",
                    )?);
                }
            }
        }
        if frame.is_some() {
            self.window.pre_present_notify();
        }
        let index = self.queue.submit(
            sidebar_cmds
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        self.presented_last = frame.is_some();
        if let Some(f) = frame {
            self.queue.present(f);
        }
        let t_submitted = Instant::now();
        // Offscreen frames have no vsync back-pressure: always wait so the
        // CPU cannot run ahead of the GPU.
        let gpu = if gpu_sync || offscreen {
            self.device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(index),
                    timeout: None,
                })
                .context("waiting for the GPU")?;
            Some(t_submitted.elapsed())
        } else {
            None
        };
        self.sidebar.after_submit();
        self.update_ime_area(caret);
        let timing = FrameTiming {
            prepare: t_prepared - t0,
            acquire: t_acquired - t_prepared,
            encode: t_submitted - t_acquired,
            gpu,
            offscreen,
        };
        Ok((Some((timing, captured)), actions))
    }

    /// Offscreen render target matching the surface (format and size).
    fn ensure_offscreen(&mut self) {
        let size = [self.surface_config.width, self.surface_config.height];
        if self.offscreen.as_ref().is_some_and(|o| o.2 == size) {
            return;
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen-frame"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.surface_config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.offscreen = Some((texture, view, size));
    }

    fn encode_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        clear: wgpu::Color,
    ) {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("grid+sidebar"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            })
            .forget_lifetime();
        self.grid.render(&mut pass);
        self.sidebar.render(&mut pass);
    }

    /// Copy `texture` (this frame) into a mappable buffer.
    fn encode_capture(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        source: &'static str,
    ) -> Result<Capture> {
        let (width, height) = (self.surface_config.width, self.surface_config.height);
        let format = self.surface_config.format;
        let bpp = format
            .block_copy_size(None)
            .ok_or_else(|| anyhow!("cannot read back format {format:?}"))?;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = (width * bpp).div_ceil(align) * align;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("screenshot"),
            size: padded_bytes_per_row as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        Ok(Capture {
            buffer,
            width,
            height,
            padded_bytes_per_row,
            format,
            source,
        })
    }

    /// Map the readback buffer and write it as an sRGB PNG.
    fn finish_capture(&self, cap: Capture, path: &Path) -> Result<()> {
        let slice = cap.buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .context("waiting for the readback")?;
        rx.recv()
            .context("readback callback dropped")?
            .context("mapping the readback buffer")?;
        let (w, h) = (cap.width as usize, cap.height as usize);
        let mut rgba = Vec::with_capacity(w * h * 4);
        {
            let data = slice
                .get_mapped_range()
                .context("reading the mapped buffer")?;
            for row in 0..h {
                let start = row * cap.padded_bytes_per_row as usize;
                rgba.extend_from_slice(&data[start..start + w * 4]);
            }
        }
        cap.buffer.unmap();
        let bgra = match cap.format {
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => true,
            wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb => false,
            other => bail!("unsupported readback format {other:?}"),
        };
        for px in rgba.as_chunks_mut::<4>().0 {
            if bgra {
                px.swap(0, 2);
            }
            px[3] = 255; // the window is opaque
        }
        write_png(path, cap.width, cap.height, &rgba)
    }

    fn update_ime_area(&mut self, caret: [f32; 4]) {
        let area = caret.map(|v| v.round() as i32);
        if self.ime_area != Some(area) {
            self.ime_area = Some(area);
            self.window.set_ime_cursor_area(
                PhysicalPosition::new(area[0], area[1]),
                PhysicalSize::new(area[2].max(1) as u32, area[3].max(1) as u32),
            );
            if self.ime.debug() {
                eprintln!("[ime] cursor area {area:?} (physical px)");
            }
        }
    }

    fn report_startup(&self, t0: Instant, first: &FrameTiming) {
        let parts: Vec<String> = self
            .startup
            .iter()
            .map(|(name, d)| format!("{name} {:.1} ms", d.as_secs_f64() * 1000.0))
            .collect();
        eprintln!(
            "[startup] {} · first frame {:.1} ms · process start → first frame {:.1} ms",
            parts.join(" · "),
            first.cpu().as_secs_f64() * 1000.0,
            t0.elapsed().as_secs_f64() * 1000.0
        );
        let m = self.grid.metrics();
        let font = self.grid.font();
        eprintln!(
            "[gpu] {} · surface {:?} {}×{} px, {:?}, scale {} · grid {}×{} at ({}, {}) px",
            self.adapter,
            self.surface_config.format,
            self.surface_config.width,
            self.surface_config.height,
            self.surface_config.present_mode,
            self.scale,
            self.layout.cols,
            self.layout.rows,
            self.layout.origin[0],
            self.layout.origin[1]
        );
        eprintln!(
            "[fonts] grid: \"{}\" {}pt → {:.0}px, cell {}×{} px, baseline {} ({} faces in fontdb)",
            self.grid.text.family(),
            font.size,
            m.font_px,
            m.cell_w,
            m.cell_h,
            m.baseline,
            self.grid.text.face_count()
        );
        let usage = self.grid.text.usage();
        let mut per_font: Vec<(String, u32)> = Vec::new();
        for (id, n) in &usage.per_font {
            let name = self.grid.text.font_name(*id);
            match per_font.iter_mut().find(|(existing, _)| *existing == name) {
                Some(entry) => entry.1 += n,
                None => per_font.push((name, *n)),
            }
        }
        per_font.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let list: Vec<String> = per_font
            .iter()
            .map(|(name, n)| format!("{name} {n}"))
            .collect();
        eprintln!(
            "[fonts] grid glyphs by font so far (fallback via cosmic-text): {}",
            list.join(", ")
        );
        eprintln!(
            "[fonts] grid procedural sprites (box drawing/blocks/powerline): {} · missing glyphs (tofu): {} {:?}",
            usage.sprites, usage.missing, usage.missing_chars
        );
        let p = self.last_prepare;
        eprintln!(
            "[grid] first frame instances: {} bg quads, {} glyphs, {} decoration quads; {} glyphs rasterized; {} atlas rebuilds",
            p.bg_quads, p.glyphs, p.deco_quads, p.rasterized, p.atlas_rebuilds
        );
        let fs = &self.sidebar.font_setup;
        eprintln!(
            "[fonts] sidebar: cjk {:?}, symbols {:?}, mono {:?}",
            fs.cjk, fs.symbols, fs.mono
        );
    }
}

fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    let mut writer = encoder.write_header().context("writing the PNG header")?;
    writer
        .write_image_data(rgba)
        .context("writing PNG pixels")?;
    writer.finish().context("finishing the PNG")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_fits_the_requested_cells_plus_sidebar_and_padding() {
        let m = CellMetrics {
            font_px: 26.0,
            cell_w: 16,
            cell_h: 31,
            baseline: 24,
            underline_top: 27,
            underline_thickness: 2,
            strikeout_top: 15,
            strikeout_thickness: 2,
            cursor_thickness: 2,
            box_thickness: 2,
        };
        let size = content_size(&m, 2.0, 280.0, (120, 40));
        assert_eq!(size, PhysicalSize::new(560 + 8 + 120 * 16, 8 + 40 * 31));
        let small = content_size(&m, 2.0, 280.0, (80, 24));
        assert_eq!(small, PhysicalSize::new(560 + 8 + 80 * 16, 8 + 24 * 31));
    }

    #[test]
    fn offscreen_frames_are_spaced_whatever_requested_them() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        // An egui animation (due now) right after an offscreen frame waits.
        assert_eq!(
            pace(t0 + ms(2), true, false, t0, t0, None),
            Pace::Sleep(Some(t0 + OFFSCREEN_INTERVAL))
        );
        assert_eq!(pace(t0 + ms(9), true, false, t0, t0, None), Pace::Redraw);
        // Presented frames are paced by vsync instead.
        assert_eq!(pace(t0 + ms(1), true, true, t0, t0, None), Pace::Redraw);
        // Nothing due: sleep until the frame timer or an earlier controller timer.
        assert_eq!(
            pace(t0, false, true, t0, t0 + ms(250), Some(t0 + ms(50))),
            Pace::Sleep(Some(t0 + ms(50)))
        );
        assert_eq!(
            pace(t0, false, true, t0, t0 + ms(250), None),
            Pace::Sleep(Some(t0 + ms(250)))
        );
        // Throttled, but a controller timer comes first.
        assert_eq!(
            pace(t0 + ms(1), true, false, t0, t0, Some(t0 + ms(3))),
            Pace::Sleep(Some(t0 + ms(3)))
        );
    }

    #[test]
    fn deadlines_land_on_the_period_grid() {
        let epoch = Instant::now();
        let p = Duration::from_millis(250);
        assert_eq!(next_multiple(epoch, epoch, p), epoch + p);
        assert_eq!(
            next_multiple(epoch, epoch + Duration::from_millis(260), p),
            epoch + 2 * p
        );
    }

    #[test]
    fn png_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.png");
        let rgba: Vec<u8> = (0..4 * 3 * 2).map(|i| i as u8).collect();
        write_png(&path, 3, 2, &rgba).unwrap();
        let decoder =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (3, 2));
        assert_eq!(&buf[..info.buffer_size()], &rgba[..]);
    }

    #[test]
    fn requests_without_a_connection_fail_visibly() {
        let mut c = Controller::new(vec![]);
        let mut none = NoConnection;
        c.on_connected(&mut none);
        assert!(c.notices().iter().any(|n| n.text.contains("未连接")));
    }
}
