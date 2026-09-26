//! winit application (DESIGN §8): one window and one wgpu device/surface
//! shared by the terminal grid renderer and the egui sidebar.
//!
//! Run modes:
//! - interactive (default): renders continuously for 5 s with a smooth cursor
//!   fade and prints frame statistics to stderr, then idles with a classic
//!   530 ms blink and a 4 Hz sidebar tick.
//! - `--bench <secs>`: two continuous phases with every frame synchronised to
//!   the GPU (true per-frame cost): cached shaping, then every line reshaped
//!   every frame (a fully changing screen). Prints statistics and exits.
//! - `--screenshot <png>`: renders 3 frames, reads the surface texture of the
//!   third back, writes a PNG and exits.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use berth_core::CursorShape;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState};
use winit::window::{ImePurpose, Window, WindowId};

use crate::config::Config;
use crate::fixture::{COLS, ROWS};
use crate::ime::{ImeOutcome, ImeState};
use crate::input::{self, Mods};
use crate::renderer::{CellMetrics, FrameInput, GridLayout, GridRenderer, PrepareStats};
use crate::sidebar::Sidebar;
use crate::stats::{FrameStats, FrameTiming};
use crate::terminal::Terminal;
use crate::theme::Theme;

/// Padding around the grid (Ghostty's default `window-padding-x/y = 2`).
pub const PADDING_PT: f32 = 2.0;
const MEASURE_SECS: f64 = 5.0;
const BLINK: Duration = Duration::from_millis(530);
const SIDEBAR_TICK: Duration = Duration::from_millis(250);
const TARGET_MS: f64 = 8.0;
const SCREENSHOT_FRAMES: u64 = 3;
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, Default)]
pub struct GuiOptions {
    pub screenshot: Option<PathBuf>,
    pub bench_secs: Option<f64>,
    pub font_family: Option<String>,
    pub font_size: Option<f32>,
    pub no_vsync: bool,
    /// Cursor shape override (Ghostty `cursor-style`).
    pub cursor_style: Option<CursorShape>,
    /// Inject a synthetic `Ime::Preedit` at startup (screenshot check of the
    /// preedit overlay; real IME events go through the same handler).
    pub demo_preedit: Option<String>,
}

pub fn run(opts: GuiOptions) -> Result<()> {
    let t0 = Instant::now();
    let mut config = Config::load();
    if let Some(family) = &opts.font_family {
        config.font.family = family.clone();
    }
    if let Some(size) = opts.font_size {
        config.font.size = size;
    }
    let event_loop = EventLoop::new().context("creating the event loop")?;
    let mut app = App::new(opts, config, t0);
    app.loop_started = Some(Instant::now());
    event_loop
        .run_app(&mut app)
        .context("running the event loop")?;
    match app.error.take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A continuous-rendering measurement window.
struct Measure {
    label: String,
    secs: f64,
    until: Option<Instant>,
    stats: FrameStats,
    /// Drop the shaping cache every frame (every line reshaped).
    reshape: bool,
    /// Wait for the GPU after every submit (true per-frame cost).
    gpu_sync: bool,
}

impl Measure {
    fn new(label: impl Into<String>, secs: f64, reshape: bool, gpu_sync: bool) -> Box<Self> {
        Box::new(Self {
            label: label.into(),
            secs,
            until: None,
            stats: FrameStats::default(),
            reshape,
            gpu_sync,
        })
    }
}

enum Phase {
    Measure(Box<Measure>),
    Idle,
    Screenshot {
        path: PathBuf,
        frames: u64,
        deadline: Instant,
    },
    Exiting,
}

struct App {
    opts: GuiOptions,
    config: Config,
    t0: Instant,
    gfx: Option<Gfx>,
    phase: Phase,
    pending: VecDeque<Box<Measure>>,
    /// Exit when the measurement phases are done (bench) instead of idling.
    exit_after_measure: bool,
    error: Option<anyhow::Error>,
    /// When `run_app` was entered (startup breakdown: launch → resumed).
    loop_started: Option<Instant>,
}

impl App {
    fn new(opts: GuiOptions, config: Config, t0: Instant) -> Self {
        let vsync = if opts.no_vsync { "no vsync" } else { "vsync" };
        let mut pending = VecDeque::new();
        let (phase, exit_after_measure) = if let Some(path) = &opts.screenshot {
            (
                Phase::Screenshot {
                    path: path.clone(),
                    frames: 0,
                    deadline: Instant::now() + SCREENSHOT_TIMEOUT,
                },
                true,
            )
        } else if let Some(secs) = opts.bench_secs {
            pending.push_back(Measure::new(
                format!("bench B: every line reshaped every frame ({secs}s, gpu-synced, {vsync})"),
                secs,
                true,
                true,
            ));
            (
                Phase::Measure(Measure::new(
                    format!("bench A: cursor fade, cached shaping ({secs}s, gpu-synced, {vsync})"),
                    secs,
                    false,
                    true,
                )),
                true,
            )
        } else {
            (
                Phase::Measure(Measure::new(
                    format!("continuous render, cursor fade ({MEASURE_SECS}s, {vsync})"),
                    MEASURE_SECS,
                    false,
                    false,
                )),
                false,
            )
        };
        Self {
            opts,
            config,
            t0,
            gfx: None,
            phase,
            pending,
            exit_after_measure,
            error: None,
            loop_started: None,
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, err: anyhow::Error) {
        tracing::error!("{err:#}");
        if self.error.is_none() {
            self.error = Some(err);
        }
        self.phase = Phase::Exiting;
        event_loop.exit();
    }

    fn next_phase(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(next) = self.pending.pop_front() {
            self.phase = Phase::Measure(next);
        } else if self.exit_after_measure {
            self.phase = Phase::Exiting;
            event_loop.exit();
        } else {
            eprintln!(
                "[idle] measurement done; now idle (cursor blink {} ms, sidebar tick {} ms). Type or use an IME; BERTH_IME_DEBUG=1 prints IME events.",
                BLINK.as_millis(),
                SIDEBAR_TICK.as_millis()
            );
            self.phase = Phase::Idle;
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let Some(gfx) = self.gfx.as_mut() else {
            return Ok(());
        };
        let start = Instant::now();
        let blinking = gfx.terminal.screen.cursor.blinking && (gfx.focused || gfx.force_focused);
        let since_input = start.saturating_duration_since(gfx.blink_epoch);
        let cursor_alpha = match &self.phase {
            Phase::Screenshot { .. } | Phase::Exiting => 1.0,
            _ if !blinking => 1.0,
            Phase::Measure(_) => {
                let t = since_input.as_secs_f32() / (2.0 * BLINK.as_secs_f32());
                0.5 + 0.5 * (t * std::f32::consts::TAU).cos()
            }
            Phase::Idle => {
                if (since_input.as_millis() / BLINK.as_millis()).is_multiple_of(2) {
                    1.0
                } else {
                    0.0
                }
            }
        };
        let (reshape, gpu_sync) = match &self.phase {
            Phase::Measure(m) => (m.reshape, m.gpu_sync),
            _ => (false, false),
        };
        let capture = matches!(&self.phase, Phase::Screenshot { frames, .. } if frames + 1 >= SCREENSHOT_FRAMES);
        if reshape {
            gfx.grid.text.clear_shape_cache();
        }
        let allow_offscreen = !matches!(self.phase, Phase::Idle | Phase::Exiting);
        let rendered = gfx.render_frame(cursor_alpha, gpu_sync, capture, allow_offscreen)?;
        // Idle deadlines are computed from the last attempt, drawn or not.
        gfx.last_frame = Instant::now();
        let Some((timing, captured)) = rendered else {
            return Ok(()); // no drawable this time; retried on the next redraw
        };
        gfx.frames += 1;
        if gfx.frames == 1 {
            gfx.report_startup(self.t0, &timing);
        }
        match &mut self.phase {
            Phase::Measure(m) => {
                let until = *m
                    .until
                    .get_or_insert(start + Duration::from_secs_f64(m.secs));
                m.stats.record(start, timing);
                if Instant::now() >= until {
                    eprint!("{}", m.stats.report(&m.label, TARGET_MS));
                    let (glyphs, mask, color, rebuilds) = gfx.grid.atlas_summary();
                    eprintln!(
                        "[stats]   atlas: {glyphs} glyphs, mask page {mask}², color page {color}², {rebuilds} rebuilds"
                    );
                    self.next_phase(event_loop);
                }
            }
            Phase::Screenshot { path, frames, .. } => {
                *frames += 1;
                if let Some(cap) = captured {
                    let path = path.clone();
                    let (w, h, source) = (cap.width, cap.height, cap.source);
                    gfx.finish_capture(cap, &path)?;
                    eprintln!(
                        "[screenshot] wrote {} ({w}×{h} px, frame {frames}, read back from the {source} texture)",
                        path.display()
                    );
                    self.phase = Phase::Exiting;
                    event_loop.exit();
                }
            }
            Phase::Idle | Phase::Exiting => {}
        }
        Ok(())
    }
}

impl ApplicationHandler for App {
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
                self.gfx = Some(gfx);
            }
            Err(e) => self.fail(event_loop, e.context("initialising the window and GPU")),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gfx) = self.gfx.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => {
                self.phase = Phase::Exiting;
                event_loop.exit();
            }
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
                if is_synthetic || event.state != ElementState::Pressed {
                    return;
                }
                if gfx.mods.super_key() {
                    // ⌘ chords are GUI shortcuts; ⌘Q / ⌘W quit the spike.
                    if let Key::Character(c) = &event.logical_key {
                        if c.eq_ignore_ascii_case("q") || c.eq_ignore_ascii_case("w") {
                            self.phase = Phase::Exiting;
                            event_loop.exit();
                        }
                    }
                    return;
                }
                // Option composes characters (Ghostty default `macos-option-as-alt`
                // unset); the composed text is already in `logical_key`.
                let mods = Mods::from_winit(gfx.mods, false);
                let Some(key) = input::key_from_winit(&event.logical_key, event.text.as_deref())
                else {
                    return;
                };
                let Some(bytes) = input::encode(&key, mods, gfx.terminal.screen.modes) else {
                    return;
                };
                let desc = input::describe(&key, mods);
                if gfx.ime.debug() {
                    eprintln!("[key] {desc} -> {:?}", input::caret_notation(&bytes));
                }
                gfx.terminal.input_bytes(&desc, &bytes);
                gfx.blink_epoch = Instant::now();
                gfx.window.request_redraw();
            }
            WindowEvent::Ime(ime) => {
                if let ImeOutcome::Commit(text) = gfx.ime.handle(&ime) {
                    gfx.terminal.input_bytes("IME commit", text.as_bytes());
                    gfx.blink_epoch = Instant::now();
                }
                if gfx.ime.debug() {
                    eprintln!(
                        "[ime] state: enabled={} preedit={:?}",
                        gfx.ime.enabled(),
                        gfx.ime.preedit()
                    );
                }
                gfx.window.request_redraw();
            }
            WindowEvent::ModifiersChanged(m) => {
                gfx.mods = m.state();
                gfx.sidebar
                    .on_window_event(&gfx.window, &WindowEvent::ModifiersChanged(m));
            }
            WindowEvent::Occluded(occluded) => {
                gfx.occluded = occluded;
                if !occluded {
                    gfx.window.request_redraw();
                }
            }
            WindowEvent::Focused(focused) => {
                gfx.focused = focused;
                gfx.blink_epoch = Instant::now();
                gfx.sidebar
                    .on_window_event(&gfx.window, &WindowEvent::Focused(focused));
                gfx.window.request_redraw();
            }
            WindowEvent::Resized(size) => {
                gfx.resize(size);
                gfx.sidebar
                    .on_window_event(&gfx.window, &WindowEvent::Resized(size));
                gfx.window.request_redraw();
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
                gfx.window.request_redraw();
            }
            other => {
                // Pointer events drive the sidebar. Keyboard and IME events never
                // reach egui: the terminal owns them.
                if gfx.sidebar.on_window_event(&gfx.window, &other) {
                    gfx.window.request_redraw();
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(gfx) = self.gfx.as_ref() else { return };
        match &self.phase {
            Phase::Measure(_) => {
                event_loop.set_control_flow(ControlFlow::Poll);
                gfx.window.request_redraw();
            }
            Phase::Screenshot { deadline, .. } => {
                if Instant::now() > *deadline {
                    self.fail(
                        event_loop,
                        anyhow!("no frame could be presented within {SCREENSHOT_TIMEOUT:?}"),
                    );
                    return;
                }
                event_loop.set_control_flow(ControlFlow::Poll);
                gfx.window.request_redraw();
            }
            Phase::Idle if gfx.occluded => {
                // Nothing is visible: sleep until an event (e.g. un-occlusion).
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Phase::Idle => {
                let next = gfx.next_deadline();
                if Instant::now() >= next {
                    gfx.window.request_redraw();
                }
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    next.max(Instant::now() + Duration::from_millis(1)),
                ));
            }
            Phase::Exiting => event_loop.exit(),
        }
    }
}

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
    terminal: Terminal,
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
}

fn content_size(m: &CellMetrics, scale: f32, sidebar_width: f32) -> PhysicalSize<u32> {
    let pad = (PADDING_PT * scale).round() as u32;
    let sidebar = (sidebar_width * scale).round() as u32;
    PhysicalSize::new(
        sidebar + 2 * pad + COLS as u32 * m.cell_w,
        2 * pad + ROWS as u32 * m.cell_h,
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

        let size = content_size(&grid.metrics(), scale, config.sidebar_width);
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
            terminal: Terminal::fixture(),
            theme,
            ime: ImeState::from_env(),
            mods: ModifiersState::empty(),
            focused: true,
            scale,
            sidebar_width: config.sidebar_width,
            layout: GridLayout {
                origin: [0.0, 0.0],
                cols: COLS,
                rows: ROWS,
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
        };
        gfx.update_layout();
        if let Some(shape) = opts.cursor_style {
            gfx.terminal.screen.cursor.shape = shape;
        }
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
            self.terminal.set_grid_size(cols, rows);
        }
    }

    /// Next idle wake-up: cursor blink toggle, sidebar tick, or egui's request.
    fn next_deadline(&self) -> Instant {
        let base = self.last_frame;
        let mut next = next_multiple(self.anim_epoch, base, SIDEBAR_TICK);
        if self.terminal.screen.cursor.blinking && self.focused {
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

    /// Render one frame. `None` when no drawable was available and offscreen
    /// rendering is not allowed.
    ///
    /// `allow_offscreen`: when the window is occluded (screen locked, display
    /// asleep, window hidden) wgpu 30's Metal backend refuses `nextDrawable`;
    /// measurement and screenshot modes then render the identical frame into
    /// an offscreen texture of the surface's format and size, synchronised
    /// with the GPU, and skip only the present.
    fn render_frame(
        &mut self,
        cursor_alpha: f32,
        gpu_sync: bool,
        capture: bool,
        allow_offscreen: bool,
    ) -> Result<Option<(FrameTiming, Option<Capture>)>> {
        let t0 = Instant::now();
        let size = [self.surface_config.width, self.surface_config.height];
        let input = FrameInput {
            screen: &self.terminal.screen,
            styles: self.terminal.styles(),
            theme: &self.theme,
            selection: self.terminal.selection,
            cursor_alpha,
            focused: self.focused || self.force_focused,
            preedit: self.ime.preedit(),
        };
        self.last_prepare =
            self.grid
                .prepare(&self.device, &self.queue, &input, &self.layout, size);
        let caret = self.grid.ime_caret_rect(&input, &self.layout);
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        let sidebar_cmds = self.sidebar.prepare(
            &self.window,
            &self.device,
            &self.queue,
            &mut encoder,
            size,
            berth_core::now_ms(),
        );
        let t_prepared = Instant::now();

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => Some(f),
            status @ (wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded) => {
                if !allow_offscreen {
                    return Ok(None);
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
                return Ok(None);
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
        Ok(Some((timing, captured)))
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
            "[fonts] grid glyphs by font (fallback via cosmic-text): {}",
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
    fn window_fits_120x40_plus_sidebar_and_padding() {
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
        let size = content_size(&m, 2.0, 280.0);
        assert_eq!(size, PhysicalSize::new(560 + 8 + 120 * 16, 8 + 40 * 31));
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
        let dir = std::env::temp_dir().join(format!("berth-png-{}", std::process::id()));
        let path = dir.join("t.png");
        let rgba: Vec<u8> = (0..4 * 3 * 2).map(|i| i as u8).collect();
        write_png(&path, 3, 2, &rgba).unwrap();
        let decoder =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (3, 2));
        assert_eq!(&buf[..info.buffer_size()], &rgba[..]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
