//! Left sidebar and terminal-area overlays (DESIGN §8.3, integrate.md §3):
//! egui + egui-winit + egui-wgpu sharing the terminal's device, queue,
//! surface and render pass.
//!
//! Data comes read-only from the [`Controller`]; clicks come back as
//! [`UiAction`]s that the app applies after the frame. Each card reports
//! whether it is on screen, which drives the preview subscriptions.
//!
//! State source styling (DESIGN §9): hook-sourced states are drawn in full
//! color, shell-integration ones slightly muted, heuristic ones faint with a
//! dashed preview bar and marked "推断". An agent leaving (kind back to
//! `Shell`) switches the kind glyph back to the shell's. A shell's busy
//! state (OSC 133 C: a command runs) reads "运行".
//!
//! Hovering a live card shows its details: state, since when, source and
//! confidence, and its newest events (fetched on hover). A dormant card
//! that can resume its agent shows the command berthd would run (display
//! only; nothing runs until "Resume" is clicked).
//!
//! IME ownership: the terminal owns the window IME. egui-winit toggles
//! `Window::set_ime_allowed` from `PlatformOutput::ime`, so the sidebar clears
//! that field every frame and is never fed keyboard or IME events; modal
//! dialogs get Enter / Esc from the app.
//!
//! Fonts: egui's bundled fonts have no CJK, so PingFang SC (fallback Hiragino
//! Sans GB / Heiti SC) is located through cosmic-text's fontdb and handed to
//! egui zero-copy (the memory-mapped face is kept alive for the process).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use berth_core::{
    AgentKind, AgentState, LineSnapshot, ReviveMode, SessionId, SessionMeta, SessionStatus,
    StateSource, StyleTable, WorkspaceId,
};
use cosmic_text::{fontdb, FontSystem};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{
    Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, Id, Margin, Order, Pos2, Rect,
    Sense, Stroke, Vec2,
};
use winit::window::Window;

use crate::controller::{self, Confirm, Controller, NoticeKind, ResumePreview, PREVIEW_ROWS};
use crate::setup_hooks::command_line;
use crate::theme::{mix, Rgb, Theme};
use crate::timefmt::local_clock;

const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
const ORANGE: Rgb = [0xde, 0x93, 0x5f];

fn c32(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    fg: Color32,
    dim: Color32,
    faint: Color32,
    select: Color32,
    hover: Color32,
    preview_bg: Color32,
    preview_fg: Color32,
    border: Color32,
    red: Color32,
    green: Color32,
    yellow: Color32,
    blue: Color32,
    magenta: Color32,
    cyan: Color32,
    orange: Color32,
}

impl Palette {
    fn from_theme(t: &Theme) -> Self {
        let black = [0, 0, 0];
        let bg = mix(t.background, black, 0.22);
        Self {
            bg: c32(bg),
            fg: c32(mix(t.foreground, t.background, 0.08)),
            dim: c32(mix(t.foreground, t.background, 0.45)),
            faint: c32(mix(t.foreground, t.background, 0.62)),
            select: c32(mix(t.background, t.foreground, 0.12)),
            hover: c32(mix(t.background, t.foreground, 0.05)),
            preview_bg: c32(mix(t.background, black, 0.40)),
            preview_fg: c32(mix(t.foreground, t.background, 0.25)),
            border: c32(mix(bg, t.foreground, 0.10)),
            red: c32(t.palette[1]),
            green: c32(t.palette[2]),
            yellow: c32(t.palette[3]),
            blue: c32(t.palette[4]),
            magenta: c32(t.palette[5]),
            cyan: c32(t.palette[6]),
            orange: c32(ORANGE),
        }
    }
}

/// Compact elapsed-time label ("12s", "1m35s", "15m", "1h05m", "3d").
pub fn format_elapsed(ms: i64) -> String {
    let s = (ms.max(0) / 1000) as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=599 => format!("{}m{:02}s", s / 60, s % 60),
        600..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d", s / 86_400),
    }
}

/// Badge glyph for an agent state (DESIGN §8.3). `tick` animates Thinking.
pub fn badge(state: &AgentState, tick: u64) -> &'static str {
    match state {
        AgentState::Idle => "○",
        AgentState::Thinking | AgentState::Compacting => SPINNER[(tick % 4) as usize],
        AgentState::ToolRunning { .. } => "⚙",
        AgentState::WaitingPermission { .. } => "⏳",
        AgentState::WaitingInput => "✎",
        AgentState::Done => "✓",
        AgentState::Error { .. } => "✗",
        AgentState::Exited { .. } => "⏹",
    }
}

/// Glyph of the program kind; an agent leaving switches back to the shell's.
pub fn kind_glyph(kind: &AgentKind) -> &'static str {
    match kind {
        AgentKind::Shell => "›",
        AgentKind::Claude => "✻",
        AgentKind::Codex => "◇",
        AgentKind::Other(_) => "◆",
    }
}

fn status_text(kind: &AgentKind, state: &AgentState, elapsed: &str) -> String {
    match state {
        AgentState::Idle => elapsed.to_string(),
        AgentState::Thinking if *kind == AgentKind::Shell => format!("运行 {elapsed}"),
        AgentState::Thinking => format!("思考 {elapsed}"),
        AgentState::ToolRunning { tool } => format!("{tool} {elapsed}"),
        AgentState::WaitingPermission { .. } => format!("等授权 {elapsed}"),
        AgentState::WaitingInput => format!("等输入 {elapsed}"),
        AgentState::Done => format!("完成 {elapsed}前"),
        AgentState::Error { .. } => "出错".to_string(),
        AgentState::Compacting => format!("压缩 {elapsed}"),
        AgentState::Exited { code: Some(c) } => format!("退出 {c}"),
        AgentState::Exited { code: None } => "已退出".to_string(),
    }
}

/// Status of a session that is not live.
fn dormant_text(status: &SessionStatus) -> String {
    match status {
        SessionStatus::Live => String::new(),
        SessionStatus::Dormant {
            exit_code: Some(c), ..
        } => format!("已退出 {c}"),
        SessionStatus::Dormant { .. } => "已退出".into(),
        SessionStatus::Restored => "已恢复 · 只读".into(),
    }
}

/// The state in words, for the hover details.
fn state_label(kind: &AgentKind, state: &AgentState) -> String {
    match state {
        AgentState::Idle => "空闲".into(),
        AgentState::Thinking if *kind == AgentKind::Shell => "运行命令".into(),
        AgentState::Thinking => "思考".into(),
        AgentState::ToolRunning { tool } => format!("运行工具 {tool}"),
        AgentState::WaitingPermission { tool: Some(t) } => format!("等授权：{t}"),
        AgentState::WaitingPermission { tool: None } => "等授权".into(),
        AgentState::WaitingInput => "等输入".into(),
        AgentState::Done => "完成".into(),
        AgentState::Error { message } if message.is_empty() => "出错".into(),
        AgentState::Error { message } => format!("出错：{message}"),
        AgentState::Compacting => "压缩上下文".into(),
        AgentState::Exited { code: Some(c) } => format!("已退出 {c}"),
        AgentState::Exited { code: None } => "已退出".into(),
    }
}

fn source_text(source: StateSource) -> &'static str {
    match source {
        StateSource::Hook => "hook",
        StateSource::ShellIntegration => "shell 集成（OSC 133）",
        StateSource::Heuristic => "推断（输出活动）",
    }
}

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn kind_label(meta: &SessionMeta) -> String {
    match &meta.agent.kind {
        AgentKind::Shell => meta
            .command
            .first()
            .map(|c| c.to_string())
            .or_else(|| std::env::var("SHELL").ok())
            .as_deref()
            .and_then(|c| Path::new(c).file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("shell")
            .to_string(),
        AgentKind::Claude => "claude".into(),
        AgentKind::Codex => "codex".into(),
        AgentKind::Other(name) => name.clone(),
    }
}

fn tilde(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rest) = path.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

/// Text format of a preview run: the cell's colors, with non-default
/// backgrounds drawn behind the text (so reverse video, e.g. vim's status
/// line, stays readable on the dark card).
fn preview_format(theme: &Theme, style: &berth_core::Style) -> TextFormat {
    let colors = theme.resolve(style, false);
    let mut format = TextFormat {
        font_id: mono(11.0),
        color: c32(colors.fg).gamma_multiply(colors.fg_alpha),
        ..Default::default()
    };
    if !colors.bg_is_default {
        format.background = c32(colors.bg);
    }
    format
}

/// A click or a visibility report from the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiAction {
    Focus(SessionId),
    /// A live card is hovered (its details want its newest events).
    Hover(SessionId),
    Revive(SessionId, ReviveMode),
    NewSession,
    NewSessionIn(WorkspaceId),
    NewWorkspace,
    Confirm(bool),
    DismissNotice(usize),
    ClosePalette,
    /// Session cards on screen this frame.
    Visible(Vec<SessionId>),
}

/// What the app shows besides the controller's data.
pub struct Chrome<'a> {
    /// Terminal area in logical points (where overlays go).
    pub grid_rect: Rect,
    pub palette_open: bool,
    /// Connection line (shown when not connected).
    pub status: Option<&'a str>,
    /// Centered in the terminal area when there is no screen to show.
    pub placeholder: Option<&'a str>,
    /// Show this card's hover details without a pointer (`--demo-hover`).
    pub demo_hover: Option<SessionId>,
}

/// A system face handed to egui without copying.
struct SystemFace {
    family: String,
    path: String,
    data: &'static [u8],
    index: u32,
}

fn system_face(
    fs: &mut FontSystem,
    families: &[&str],
    weight: fontdb::Weight,
) -> Option<SystemFace> {
    for family in families {
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            weight,
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        };
        let Some(id) = fs.db().query(&query) else {
            continue;
        };
        let Some(face) = fs.db().face(id) else {
            continue;
        };
        let index = face.index;
        let path = match &face.source {
            fontdb::Source::File(p) | fontdb::Source::SharedFile(p, _) => p.display().to_string(),
            fontdb::Source::Binary(_) => "<memory>".into(),
        };
        let Some(font) = fs.get_font(id, weight) else {
            tracing::warn!(family, path, "font face listed but failed to load");
            continue;
        };
        // Keep the mmap alive for the process; egui borrows the bytes.
        let font: &'static Arc<cosmic_text::Font> = Box::leak(Box::new(font));
        return Some(SystemFace {
            family: family.to_string(),
            path,
            data: font.data(),
            index,
        });
    }
    None
}

fn font_data(face: &SystemFace) -> Arc<FontData> {
    let mut data = FontData::from_static(face.data);
    data.index = face.index;
    Arc::new(data)
}

pub struct FontSetup {
    pub cjk: Option<String>,
    pub symbols: Vec<String>,
    pub mono: Option<String>,
}

/// Build egui font definitions: system CJK + symbols + the terminal's mono
/// face in front of egui's bundled fonts.
fn font_definitions(fs: &mut FontSystem, mono_family: &str) -> (FontDefinitions, FontSetup) {
    let mut defs = FontDefinitions::default();
    let cjk_families = [
        "PingFang SC",
        "Hiragino Sans GB",
        "Heiti SC",
        "Noto Sans CJK SC",
        "Source Han Sans SC",
    ];
    let cjk = system_face(fs, &cjk_families, fontdb::Weight::NORMAL);
    let cjk_bold = system_face(fs, &cjk_families, fontdb::Weight::SEMIBOLD);
    // Symbol fallbacks in order: geometric shapes / misc technical, then
    // dingbats (✓ ✗ ✎ ✻), then Menlo's broad symbol coverage.
    let symbols: Vec<(&str, SystemFace)> = [
        ("symbols", "Apple Symbols"),
        ("dingbats", "Zapf Dingbats"),
        ("menlo", "Menlo"),
    ]
    .into_iter()
    .filter_map(|(key, family)| {
        system_face(fs, &[family], fontdb::Weight::NORMAL).map(|f| (key, f))
    })
    .collect();
    let mono = system_face(
        fs,
        &[mono_family, "SF Mono", "Menlo"],
        fontdb::Weight::NORMAL,
    );

    let mut front: Vec<String> = Vec::new();
    let mut front_bold: Vec<String> = Vec::new();
    if let Some(f) = &cjk {
        defs.font_data.insert("cjk".into(), font_data(f));
        front.push("cjk".into());
        tracing::info!(family = %f.family, path = %f.path, index = f.index, "sidebar CJK font");
    } else {
        tracing::warn!(tried = ?cjk_families, "no CJK font found; sidebar Chinese will render as tofu");
    }
    match &cjk_bold {
        Some(f) => {
            defs.font_data.insert("cjk-bold".into(), font_data(f));
            front_bold.push("cjk-bold".into());
        }
        None => front_bold.extend(front.iter().cloned()),
    }
    for (key, f) in &symbols {
        defs.font_data.insert((*key).into(), font_data(f));
        front.push((*key).into());
        front_bold.push((*key).into());
    }
    let proportional = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let monospace = defs
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let mut mono_chain: Vec<String> = Vec::new();
    if let Some(f) = &mono {
        defs.font_data.insert("mono".into(), font_data(f));
        mono_chain.push("mono".into());
    }
    mono_chain.extend(front.iter().cloned());
    mono_chain.extend(monospace);
    let mut prop_chain = front.clone();
    prop_chain.extend(proportional.iter().cloned());
    let mut bold_chain = front_bold;
    bold_chain.extend(proportional);
    defs.families.insert(FontFamily::Proportional, prop_chain);
    defs.families.insert(FontFamily::Monospace, mono_chain);
    defs.families
        .insert(FontFamily::Name("bold".into()), bold_chain);
    let setup = FontSetup {
        cjk: cjk.map(|f| f.family),
        symbols: symbols.into_iter().map(|(_, f)| f.family).collect(),
        mono: mono.map(|f| f.family),
    };
    (defs, setup)
}

/// Which font in `family`'s chain covers each character of `text`.
fn coverage(
    defs: &FontDefinitions,
    family: &FontFamily,
    text: &str,
    per_font: &mut BTreeMap<String, u32>,
    missing: &mut Vec<char>,
) {
    let chain = defs.families.get(family).cloned().unwrap_or_default();
    let faces: Vec<(String, swash::FontRef<'_>)> = chain
        .iter()
        .filter_map(|name| {
            let data = defs.font_data.get(name)?;
            swash::FontRef::from_index(&data.font, data.index as usize).map(|f| (name.clone(), f))
        })
        .collect();
    for c in text
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
    {
        match faces.iter().find(|(_, f)| f.charmap().map(c) != 0) {
            Some((name, _)) => *per_font.entry(name.clone()).or_default() += 1,
            None => {
                if !missing.contains(&c) {
                    missing.push(c);
                }
            }
        }
    }
}

pub struct Sidebar {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    fonts: FontDefinitions,
    pub font_setup: FontSetup,
    palette: Palette,
    theme: Theme,
    width_pt: f32,
    dormant_open: bool,
    jobs: Vec<egui::ClippedPrimitive>,
    screen: egui_wgpu::ScreenDescriptor,
    to_free: Vec<egui::TextureId>,
    repaint_delay: Option<Duration>,
    coverage_done: bool,
    /// `Chrome::demo_hover` of the frame being built.
    demo_hover: Option<SessionId>,
}

/// Strings painted in one pass, with the family used (for the coverage log).
type Painted = Vec<(FontFamily, String)>;

/// Paint one line of text (truncated to `max_w`) and return its rect.
#[allow(clippy::too_many_arguments)]
fn paint_text(
    ui: &egui::Ui,
    painted: &mut Option<&mut Painted>,
    pos: Pos2,
    anchor: Align2,
    s: &str,
    font: FontId,
    color: Color32,
    max_w: f32,
) -> Rect {
    if let Some(p) = painted.as_deref_mut() {
        p.push((font.family.clone(), s.to_string()));
    }
    let mut job = LayoutJob::simple_singleline(s.to_string(), font, color);
    job.wrap = TextWrapping::truncate_at_width(max_w.max(1.0));
    let galley = ui.painter().layout_job(job);
    let rect = anchor.anchor_size(pos, galley.size());
    ui.painter().galley(rect.min, galley, color);
    rect
}

fn prop(size: f32) -> FontId {
    FontId::proportional(size)
}

fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

const LINE_H: f32 = 14.0;

impl Sidebar {
    pub fn new(
        window: &Window,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        fs: &mut FontSystem,
        mono_family: &str,
        width_pt: f32,
        theme: &Theme,
    ) -> Self {
        let ctx = egui::Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        let (fonts, font_setup) = font_definitions(fs, mono_family);
        ctx.set_fonts(fonts.clone());
        let max_texture_side = device.limits().max_texture_dimension_2d as usize;
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            None,
            Some(max_texture_side),
        );
        let renderer = egui_wgpu::Renderer::new(
            device,
            format,
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: false,
                predictable_texture_filtering: false,
            },
        );
        Self {
            ctx,
            state,
            renderer,
            fonts,
            font_setup,
            palette: Palette::from_theme(theme),
            theme: theme.clone(),
            width_pt,
            dormant_open: true,
            jobs: Vec::new(),
            screen: egui_wgpu::ScreenDescriptor {
                size_in_pixels: [1, 1],
                pixels_per_point: 1.0,
            },
            to_free: Vec::new(),
            repaint_delay: None,
            coverage_done: false,
            demo_hover: None,
        }
    }

    /// Forward a (non-keyboard, non-IME) window event. Returns true when egui
    /// wants a repaint.
    pub fn on_window_event(&mut self, window: &Window, event: &winit::event::WindowEvent) -> bool {
        self.state.on_window_event(window, event).repaint
    }

    /// egui is using the pointer (over the sidebar, an overlay or a dialog,
    /// or dragging one of its widgets): the terminal must not act on it.
    pub fn wants_pointer(&self) -> bool {
        self.ctx.egui_wants_pointer_input()
    }

    /// When egui asked to be repainted (hover animations etc.).
    pub fn repaint_delay(&self) -> Option<Duration> {
        self.repaint_delay
    }

    /// Run egui, tessellate, upload textures and buffers. The returned
    /// command buffers must be submitted before `encoder`'s.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &mut self,
        window: &Window,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        size_px: [u32; 2],
        ctl: &Controller,
        chrome: &Chrome,
        now_ms: i64,
    ) -> (Vec<wgpu::CommandBuffer>, Vec<UiAction>) {
        let raw = self.state.take_egui_input(window);
        let ctx = self.ctx.clone();
        let mut painted: Painted = Vec::new();
        let collect = !self.coverage_done && ctl.is_loaded();
        let mut actions = Vec::new();
        let mut full = ctx.run_ui(raw, |ui| {
            painted.clear();
            actions.clear();
            self.ui(
                ui,
                ctl,
                chrome,
                now_ms,
                collect.then_some(&mut painted),
                &mut actions,
            );
        });
        let mut platform = full.platform_output;
        platform.ime = None; // the terminal owns the IME
        self.state.handle_platform_output(window, platform);
        self.repaint_delay = full
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay);
        // Every delta must be consumed: `TexturesDelta` asserts on drop.
        let mut textures = std::mem::take(&mut full.textures_delta);
        for (id, deltas) in textures.set.drain() {
            for delta in &deltas {
                self.renderer.update_texture(device, queue, id, delta);
            }
        }
        self.to_free.extend(textures.free.drain());
        self.jobs = ctx.tessellate(full.shapes, full.pixels_per_point);
        self.screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: size_px,
            pixels_per_point: full.pixels_per_point,
        };
        if collect {
            self.coverage_done = true;
            self.log_coverage(&painted);
        }
        let cmds = self
            .renderer
            .update_buffers(device, queue, encoder, &self.jobs, &self.screen);
        (cmds, actions)
    }

    pub fn render(&self, pass: &mut wgpu::RenderPass<'static>) {
        self.renderer.render(pass, &self.jobs, &self.screen);
    }

    /// Free textures egui released this frame (after the frame was submitted).
    pub fn after_submit(&mut self) {
        for id in self.to_free.drain(..) {
            self.renderer.free_texture(&id);
        }
    }

    fn log_coverage(&self, painted: &Painted) {
        let mut per_font = BTreeMap::new();
        let mut missing = Vec::new();
        let mut chars = 0usize;
        for (family, text) in painted {
            chars += text.chars().filter(|c| !c.is_whitespace()).count();
            coverage(&self.fonts, family, text, &mut per_font, &mut missing);
        }
        eprintln!(
            "[fonts] sidebar: {} strings / {chars} glyphs; covered by {per_font:?}; missing {} {:?}",
            painted.len(),
            missing.len(),
            missing
        );
        if !missing.is_empty() {
            tracing::warn!(
                ?missing,
                "sidebar characters without any font (would render as tofu)"
            );
        }
    }

    fn state_color(&self, state: &AgentState, now_ms: i64) -> Color32 {
        let pal = self.palette;
        let pulse =
            0.55 + 0.45 * ((now_ms as f64 / 1000.0 * std::f64::consts::PI).sin().abs() as f32);
        match state {
            AgentState::Idle | AgentState::Exited { .. } => pal.dim,
            AgentState::Thinking => pal.blue,
            AgentState::Compacting => pal.magenta,
            AgentState::ToolRunning { .. } => pal.yellow,
            AgentState::WaitingPermission { .. } => pal.orange.gamma_multiply(pulse),
            AgentState::WaitingInput => pal.cyan,
            AgentState::Done => pal.green,
            AgentState::Error { .. } => pal.red,
        }
    }

    /// Preview line with the session's colors.
    fn preview_job(&self, line: &LineSnapshot, styles: &StyleTable, max_w: f32) -> LayoutJob {
        let mut job = LayoutJob::default();
        let trimmed_end = line.text().trim_end().chars().count();
        let mut taken = 0usize;
        for run in &line.runs {
            if taken >= trimmed_end {
                break;
            }
            let n = run.text.chars().count().min(trimmed_end - taken);
            let text: String = run.text.chars().take(n).collect();
            taken += n;
            job.append(
                &text,
                0.0,
                preview_format(&self.theme, &styles.get(run.style)),
            );
        }
        job.wrap = TextWrapping::truncate_at_width(max_w.max(1.0));
        job
    }

    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        ctl: &Controller,
        chrome: &Chrome,
        now_ms: i64,
        mut painted: Option<&mut Painted>,
        actions: &mut Vec<UiAction>,
    ) {
        let pal = self.palette;
        let ctx = ui.ctx().clone();
        let mut visible: Vec<SessionId> = Vec::new();
        self.demo_hover = chrome.demo_hover;
        egui::Panel::left("berth-sidebar")
            .exact_size(self.width_pt)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(pal.bg)
                    .inner_margin(Margin::symmetric(10, 10)),
            )
            .show(ui, |ui| {
                let full = ui.max_rect();
                ui.painter().vline(
                    full.right() + 9.5,
                    full.top() - 10.0..=full.bottom() + 10.0,
                    Stroke::new(1.0, pal.border),
                );
                self.header(ui, ctl, chrome, &mut painted);
                let footer_h = 40.0;
                let list_h = (ui.available_height() - footer_h).max(40.0);
                egui::ScrollArea::vertical()
                    .max_height(list_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.session_list(ui, ctl, now_ms, &mut painted, actions, &mut visible);
                    });
                self.footer(ui, ctl, &mut painted, actions);
            });
        actions.push(UiAction::Visible(visible));
        self.overlays(&ctx, ctl, chrome, &mut painted, actions);
    }

    fn header(
        &self,
        ui: &mut egui::Ui,
        ctl: &Controller,
        chrome: &Chrome,
        painted: &mut Option<&mut Painted>,
    ) {
        let pal = self.palette;
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::hover());
        paint_text(
            ui,
            painted,
            rect.left_center(),
            Align2::LEFT_CENTER,
            "berth",
            bold(15.0),
            pal.fg,
            80.0,
        );
        let live: Vec<&SessionMeta> = ctl
            .jump_order()
            .into_iter()
            .filter_map(|sid| ctl.session(sid))
            .filter(|m| m.is_live())
            .collect();
        let busy = live.iter().filter(|m| m.agent.state.is_busy()).count();
        let attention = live
            .iter()
            .filter(|m| m.agent.state.needs_attention())
            .count();
        paint_text(
            ui,
            painted,
            rect.right_center(),
            Align2::RIGHT_CENTER,
            &format!("{busy} 运行 · {attention} 需关注"),
            prop(11.0),
            if attention > 0 { pal.orange } else { pal.dim },
            170.0,
        );
        if let Some(status) = chrome.status {
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
            ui.painter()
                .circle_filled(Pos2::new(rect.left() + 4.0, rect.center().y), 3.5, pal.red);
            paint_text(
                ui,
                painted,
                Pos2::new(rect.left() + 12.0, rect.center().y),
                Align2::LEFT_CENTER,
                status,
                prop(11.0),
                pal.red,
                rect.width() - 12.0,
            );
        }
        ui.add_space(4.0);
    }

    fn session_list(
        &mut self,
        ui: &mut egui::Ui,
        ctl: &Controller,
        now_ms: i64,
        painted: &mut Option<&mut Painted>,
        actions: &mut Vec<UiAction>,
        visible: &mut Vec<SessionId>,
    ) {
        let pal = self.palette;
        let order = ctl.jump_order();
        let number = |sid: SessionId| order.iter().position(|s| *s == sid).map(|i| i + 1);
        for ws in ctl.workspaces() {
            // Workspace header: ▾ ● name  ~/root             [+]
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::hover());
            let y = rect.center().y;
            paint_text(
                ui,
                painted,
                Pos2::new(rect.left(), y),
                Align2::LEFT_CENTER,
                "▾",
                prop(12.0),
                pal.dim,
                12.0,
            );
            let dot = ws.color.map(c32).unwrap_or(pal.dim);
            ui.painter()
                .circle_filled(Pos2::new(rect.left() + 17.0, y), 3.5, dot);
            let name = paint_text(
                ui,
                painted,
                Pos2::new(rect.left() + 26.0, y),
                Align2::LEFT_CENTER,
                &ws.name,
                bold(13.0),
                pal.fg,
                120.0,
            );
            paint_text(
                ui,
                painted,
                Pos2::new(name.right() + 8.0, y),
                Align2::LEFT_CENTER,
                &tilde(&ws.root),
                prop(11.0),
                pal.faint,
                (rect.right() - 22.0 - name.right() - 8.0).max(10.0),
            );
            let plus = Rect::from_center_size(Pos2::new(rect.right() - 7.0, y), Vec2::splat(18.0));
            let resp = ui
                .interact(plus, Id::new(("ws-new", ws.id)), Sense::click())
                .on_hover_text("在这个 workspace 新建 session（⌘N）");
            if resp.hovered() {
                ui.painter().rect_filled(plus, 4.0, pal.hover);
            }
            if resp.clicked() {
                actions.push(UiAction::NewSessionIn(ws.id));
            }
            paint_text(
                ui,
                painted,
                plus.center(),
                Align2::CENTER_CENTER,
                "+",
                prop(14.0),
                pal.dim,
                14.0,
            );
            let sessions = ctl.live_in(ws.id);
            if sessions.is_empty() {
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
                paint_text(
                    ui,
                    painted,
                    Pos2::new(rect.left() + 24.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    "（没有运行中的 session）",
                    prop(11.0),
                    pal.faint,
                    rect.width() - 24.0,
                );
            }
            for m in sessions {
                self.card(ui, ctl, m, number(m.id), now_ms, painted, actions, visible);
            }
            ui.add_space(6.0);
        }
        let orphans = ctl.live_orphans();
        if !orphans.is_empty() {
            self.group_header(ui, painted, "（workspace 已删除）", None);
            for m in orphans {
                self.card(ui, ctl, m, number(m.id), now_ms, painted, actions, visible);
            }
        }
        let dormant = ctl.dormant();
        if !dormant.is_empty() {
            let title = format!("休眠 ({})  只读历史 · 可 Revive", dormant.len());
            if self.group_header(ui, painted, &title, Some(self.dormant_open)) {
                self.dormant_open = !self.dormant_open;
            }
            if self.dormant_open {
                for m in dormant {
                    self.card(ui, ctl, m, number(m.id), now_ms, painted, actions, visible);
                }
            }
        }
        if ctl.is_loaded() && order.is_empty() {
            ui.add_space(8.0);
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
            paint_text(
                ui,
                painted,
                rect.left_center(),
                Align2::LEFT_CENTER,
                "没有 session：⌘N 新建",
                prop(12.0),
                pal.dim,
                rect.width(),
            );
        }
    }

    /// A collapsible group header; returns true when clicked.
    fn group_header(
        &self,
        ui: &mut egui::Ui,
        painted: &mut Option<&mut Painted>,
        title: &str,
        open: Option<bool>,
    ) -> bool {
        let pal = self.palette;
        let sense = if open.is_some() {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), sense);
        let y = rect.center().y;
        let arrow = match open {
            Some(false) => "▸",
            _ => "▾",
        };
        paint_text(
            ui,
            painted,
            Pos2::new(rect.left(), y),
            Align2::LEFT_CENTER,
            arrow,
            prop(12.0),
            pal.dim,
            12.0,
        );
        paint_text(
            ui,
            painted,
            Pos2::new(rect.left() + 14.0, y),
            Align2::LEFT_CENTER,
            title,
            bold(12.0),
            pal.dim,
            rect.width() - 14.0,
        );
        resp.clicked()
    }

    #[allow(clippy::too_many_arguments)]
    fn card(
        &self,
        ui: &mut egui::Ui,
        ctl: &Controller,
        m: &SessionMeta,
        number: Option<usize>,
        now_ms: i64,
        painted: &mut Option<&mut Painted>,
        actions: &mut Vec<UiAction>,
        visible: &mut Vec<SessionId>,
    ) {
        let pal = self.palette;
        let live = m.is_live();
        let resume = controller::resumable(m);
        let resume_line = resume && ctl.card_details();
        let buttons_h = match (live, resume_line) {
            (true, _) => 0.0,
            (false, false) => 24.0,
            (false, true) => 24.0 + 16.0,
        };
        let preview_h = PREVIEW_ROWS as f32 * LINE_H + 6.0;
        let height = 4.0 + 18.0 + 16.0 + preview_h + buttons_h + 8.0;
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
        if ui.is_rect_visible(rect) {
            visible.push(m.id);
        }
        if resp.clicked() {
            actions.push(UiAction::Focus(m.id));
        }
        let focused = ctl.focused() == Some(m.id);
        if focused || resp.hovered() {
            ui.painter()
                .rect_filled(rect, 6.0, if focused { pal.select } else { pal.hover });
        }
        let agent = &m.agent;
        let state = &agent.state;
        let source = agent.source;
        let base = self.state_color(state, now_ms);
        let color = match source {
            StateSource::Hook => base,
            StateSource::ShellIntegration => base.gamma_multiply(0.8),
            StateSource::Heuristic => base.gamma_multiply(0.5),
        };
        let tick = (now_ms.max(0) / 250) as u64;
        let x0 = rect.left() + 6.0;
        let y1 = rect.top() + 4.0 + 9.0;
        if live {
            paint_text(
                ui,
                painted,
                Pos2::new(x0 + 6.0, y1),
                Align2::CENTER_CENTER,
                badge(state, tick),
                prop(14.0),
                color,
                20.0,
            );
        } else {
            paint_text(
                ui,
                painted,
                Pos2::new(x0 + 6.0, y1),
                Align2::CENTER_CENTER,
                "⏹",
                prop(13.0),
                pal.faint,
                20.0,
            );
        }
        let glyph = paint_text(
            ui,
            painted,
            Pos2::new(x0 + 16.0, y1),
            Align2::LEFT_CENTER,
            kind_glyph(&agent.kind),
            prop(12.0),
            if agent.kind.is_agent() {
                pal.orange
            } else {
                pal.dim
            },
            14.0,
        );
        let kind = paint_text(
            ui,
            painted,
            Pos2::new(glyph.right() + 3.0, y1),
            Align2::LEFT_CENTER,
            &kind_label(m),
            mono(11.0),
            pal.dim,
            60.0,
        );
        let status = if live {
            let mut s = status_text(&agent.kind, state, &format_elapsed(now_ms - agent.since_ms));
            if source == StateSource::Heuristic && *state != AgentState::Idle {
                s.push_str(" ·推断");
            }
            s
        } else {
            dormant_text(&m.status)
        };
        let status_color = if !live {
            pal.faint
        } else if state.needs_attention() {
            color
        } else {
            pal.dim
        };
        let status_rect = paint_text(
            ui,
            painted,
            Pos2::new(rect.right() - 6.0, y1),
            Align2::RIGHT_CENTER,
            &status,
            prop(11.0),
            status_color,
            110.0,
        );
        let title_font = if m.unread { bold(13.0) } else { prop(13.0) };
        let title_x = kind.right().max(x0 + 64.0) + 6.0;
        let dot_room = if m.unread { 12.0 } else { 0.0 };
        let mut title = m.title().to_string();
        if let Some(n) = number.filter(|n| *n <= 9) {
            title = format!("{title}  ⌘{n}");
        }
        let title_rect = paint_text(
            ui,
            painted,
            Pos2::new(title_x, y1),
            Align2::LEFT_CENTER,
            &title,
            title_font,
            if m.unread { pal.fg } else { pal.preview_fg },
            (status_rect.left() - 8.0 - dot_room - title_x).max(20.0),
        );
        if m.unread {
            ui.painter()
                .circle_filled(Pos2::new(title_rect.right() + 6.0, y1), 3.0, pal.blue);
        }
        let y2 = rect.top() + 4.0 + 18.0 + 8.0;
        paint_text(
            ui,
            painted,
            Pos2::new(x0 + 18.0, y2),
            Align2::LEFT_CENTER,
            &tilde(&m.cwd),
            prop(11.0),
            pal.faint,
            rect.width() - 36.0,
        );

        // Preview: the focused session from its full view, others from
        // their Preview subscription.
        let top = rect.top() + 4.0 + 18.0 + 16.0 + 2.0;
        let preview = Rect::from_min_max(
            Pos2::new(x0 + 16.0, top),
            Pos2::new(rect.right() - 6.0, top + preview_h),
        );
        ui.painter().rect_filled(preview, 4.0, pal.preview_bg);
        let bar = [
            Pos2::new(preview.left() + 1.0, preview.top() + 3.0),
            Pos2::new(preview.left() + 1.0, preview.bottom() - 3.0),
        ];
        let bar_color = if live { color } else { pal.faint };
        if live && source == StateSource::Heuristic {
            ui.painter().extend(egui::Shape::dashed_line(
                &bar,
                Stroke::new(2.0, bar_color),
                3.0,
                3.0,
            ));
        } else {
            ui.painter().line_segment(bar, Stroke::new(2.0, bar_color));
        }
        let focused_tail;
        let (lines, styles): (&[LineSnapshot], Option<&StyleTable>) = match ctl.view() {
            Some(v) if focused && v.has_screen() => {
                focused_tail = v.tail(PREVIEW_ROWS);
                (&focused_tail, Some(v.styles()))
            }
            _ => match ctl.preview(m.id) {
                Some(p) => (&p.lines, Some(&p.styles)),
                None => (&[], None),
            },
        };
        let skip = lines.len().saturating_sub(PREVIEW_ROWS);
        for (i, line) in lines[skip..].iter().enumerate() {
            let pos = Pos2::new(
                preview.left() + 8.0,
                preview.top() + 3.0 + i as f32 * LINE_H,
            );
            let job = match styles {
                Some(st) => self.preview_job(line, st, preview.width() - 12.0),
                None => LayoutJob::default(),
            };
            if let Some(p) = painted.as_deref_mut() {
                p.push((FontFamily::Monospace, line.text_trimmed()));
            }
            let galley = ui.painter().layout_job(job);
            ui.painter().galley(pos, galley, pal.preview_fg);
        }
        if !live {
            let y = preview.bottom() + 4.0;
            let mut x = preview.left();
            let mut button = |label: &str, mode: ReviveMode, tip: &str| {
                let w = 12.0 + label.chars().count() as f32 * 7.5;
                let r = Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, 20.0));
                x += w + 6.0;
                let b = egui::Button::new(egui::RichText::new(label).size(11.0));
                if ui.put(r, b).on_hover_text(tip).clicked() {
                    actions.push(UiAction::Revive(m.id, mode));
                }
            };
            button(
                "Revive",
                ReviveMode::Shell,
                "在原目录启动新的 shell，历史保留在上方",
            );
            if resume {
                let label = format!("Resume {}", kind_label(m));
                let tip = match ctl.resume_preview(m.id) {
                    Some(ResumePreview {
                        cwd,
                        command: Ok(argv),
                    }) => format!(
                        "在 {} 运行：\n{}\n（berthd 执行前再校验 id）",
                        tilde(cwd),
                        clean(&command_line(argv))
                    ),
                    Some(ResumePreview {
                        command: Err(e), ..
                    }) => {
                        format!("berthd 会拒绝：{}", clean(e))
                    }
                    None => "恢复 agent 会话（id 由 berthd 校验）".to_string(),
                };
                button(&label, ReviveMode::ResumeAgent, &tip);
            }
            if resume_line {
                let (text, color) = match ctl.resume_preview(m.id) {
                    Some(ResumePreview {
                        command: Ok(argv), ..
                    }) => (format!("$ {}", clean(&command_line(argv))), pal.dim),
                    Some(ResumePreview {
                        command: Err(e), ..
                    }) => (format!("不能恢复：{}", clean(e)), pal.orange),
                    None => ("$ …".to_string(), pal.faint),
                };
                paint_text(
                    ui,
                    painted,
                    Pos2::new(preview.left(), y + 20.0 + 4.0 + 7.0),
                    Align2::LEFT_CENTER,
                    &text,
                    mono(10.5),
                    color,
                    preview.width(),
                );
            }
        }
        if live {
            let demo = self.demo_hover == Some(m.id);
            if demo || resp.hovered() {
                actions.push(UiAction::Hover(m.id));
            }
            if demo {
                egui::Tooltip::for_widget(&resp).show(|ui| self.hover_details(ui, ctl, m, now_ms));
            } else {
                resp.on_hover_ui(|ui| self.hover_details(ui, ctl, m, now_ms));
            }
        }
        ui.add_space(2.0);
    }

    /// A live card's details: state, since, source, newest events.
    fn hover_details(&self, ui: &mut egui::Ui, ctl: &Controller, m: &SessionMeta, now_ms: i64) {
        let pal = self.palette;
        let a = &m.agent;
        ui.set_max_width(340.0);
        ui.label(
            egui::RichText::new(format!(
                "{} {} · {}",
                kind_glyph(&a.kind),
                kind_label(m),
                clean(&state_label(&a.kind, &a.state))
            ))
            .font(bold(13.0))
            .color(self.state_color(&a.state, now_ms)),
        );
        let small =
            |text: String, color: Color32| egui::RichText::new(text).font(prop(11.5)).color(color);
        ui.label(small(
            format!(
                "自 {}（{}前）",
                local_clock(a.since_ms),
                format_elapsed(now_ms - a.since_ms)
            ),
            pal.fg,
        ));
        ui.label(small(
            format!(
                "来源：{}（置信度 {:.2}）",
                source_text(a.source),
                a.confidence
            ),
            pal.fg,
        ));
        let mut extra: Vec<String> = Vec::new();
        if let Some(model) = &a.model {
            extra.push(clean(model));
        }
        if let Some(ctx) = a.context_pct {
            extra.push(format!("ctx {ctx:.0}%"));
        }
        if let Some(cost) = a.cost_usd {
            extra.push(format!("${cost:.2}"));
        }
        if !extra.is_empty() {
            ui.label(small(extra.join(" · "), pal.dim));
        }
        ui.separator();
        let recent = ctl.recent_events(m.id);
        match recent.map(|r| (&r.events, &r.error)) {
            Some((_, Some(e))) => {
                ui.label(small(format!("读取事件失败：{}", clean(e)), pal.orange));
            }
            Some((Some(events), None)) if events.is_empty() => {
                ui.label(small("（还没有事件）".into(), pal.dim));
            }
            Some((Some(events), None)) => {
                ui.label(small(format!("最近 {} 个事件", events.len()), pal.dim));
                for e in events.iter().rev() {
                    let mut line = format!(
                        "{}  {} → {}",
                        local_clock(e.at_ms),
                        clean(&e.kind),
                        clean(&e.state)
                    );
                    if let Some(d) = e.detail.as_deref().filter(|d| !d.is_empty()) {
                        line.push_str("  ");
                        line.push_str(&clean(d));
                    }
                    ui.label(egui::RichText::new(line).font(mono(11.0)).color(pal.fg));
                }
            }
            _ if !ctl.card_details() => {
                ui.label(small("（berthd 较旧：没有事件列表）".into(), pal.dim));
            }
            _ => {
                ui.label(small("读取事件…".into(), pal.dim));
            }
        }
    }

    fn footer(
        &self,
        ui: &mut egui::Ui,
        ctl: &Controller,
        painted: &mut Option<&mut Painted>,
        actions: &mut Vec<UiAction>,
    ) {
        let pal = self.palette;
        ui.add_space(4.0);
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), Sense::hover());
        // Focused agent details (model · context · cost) when known.
        let detail = ctl
            .focused_meta()
            .map(|m| {
                let a = &m.agent;
                let mut parts: Vec<String> = Vec::new();
                if let Some(model) = &a.model {
                    parts.push(model.clone());
                }
                if let Some(ctx) = a.context_pct {
                    parts.push(format!("ctx {ctx:.0}%"));
                }
                if let Some(cost) = a.cost_usd {
                    parts.push(format!("${cost:.2}"));
                }
                parts.join(" · ")
            })
            .unwrap_or_default();
        paint_text(
            ui,
            painted,
            rect.left_center(),
            Align2::LEFT_CENTER,
            &detail,
            prop(11.0),
            pal.dim,
            rect.width(),
        );
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
        let mut x = rect.left();
        for (label, action, tip) in [
            ("+ session", UiAction::NewSession, "⌘N"),
            ("+ workspace", UiAction::NewWorkspace, "⌘⇧N"),
        ] {
            let w = 10.0 + label.len() as f32 * 6.5;
            let r = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(w, 18.0));
            x += w + 6.0;
            let b = egui::Button::new(egui::RichText::new(label).size(11.0)).frame(false);
            if ui.put(r, b).on_hover_text(tip).clicked() {
                actions.push(action);
            }
        }
        paint_text(
            ui,
            painted,
            rect.right_center(),
            Align2::RIGHT_CENTER,
            "⌘K 面板",
            prop(11.0),
            pal.faint,
            60.0,
        );
    }

    fn overlays(
        &self,
        ctx: &egui::Context,
        ctl: &Controller,
        chrome: &Chrome,
        painted: &mut Option<&mut Painted>,
        actions: &mut Vec<UiAction>,
    ) {
        let pal = self.palette;
        let grid = chrome.grid_rect;
        if let Some(text) = chrome.placeholder {
            let painter =
                ctx.layer_painter(egui::LayerId::new(Order::Middle, Id::new("placeholder")));
            painter.text(
                grid.center(),
                Align2::CENTER_CENTER,
                text,
                prop(14.0),
                pal.dim,
            );
            if let Some(p) = painted.as_deref_mut() {
                p.push((FontFamily::Proportional, text.to_string()));
            }
        }
        let notices = ctl.notices();
        if !notices.is_empty() {
            egui::Area::new(Id::new("notices"))
                .order(Order::Foreground)
                .pivot(Align2::RIGHT_BOTTOM)
                .fixed_pos(grid.right_bottom() - Vec2::new(10.0, 10.0))
                .show(ctx, |ui| {
                    let width = (grid.width() - 20.0).clamp(120.0, 560.0);
                    ui.set_max_width(width);
                    for (i, n) in notices.iter().enumerate() {
                        let (fill, fg) = match n.kind {
                            NoticeKind::Error => (Color32::from_rgb(0x5a, 0x1d, 0x1d), pal.fg),
                            NoticeKind::Info => (Color32::from_rgb(0x26, 0x2c, 0x36), pal.fg),
                        };
                        egui::Frame::new()
                            .fill(fill)
                            .corner_radius(6.0)
                            .inner_margin(Margin::symmetric(10, 6))
                            .show(ui, |ui| {
                                ui.set_width(width - 20.0);
                                ui.horizontal(|ui| {
                                    let mut text = n.text.clone();
                                    if n.count > 1 {
                                        text.push_str(&format!("（×{}）", n.count));
                                    }
                                    if let Some(p) = painted.as_deref_mut() {
                                        p.push((FontFamily::Proportional, text.clone()));
                                    }
                                    let close = ui
                                        .add(egui::Button::new("×").frame(false))
                                        .on_hover_text("关闭");
                                    if close.clicked() {
                                        actions.push(UiAction::DismissNotice(i));
                                    }
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(text).color(fg).size(12.0),
                                        )
                                        .wrap(),
                                    );
                                });
                            });
                        ui.add_space(4.0);
                    }
                });
        }
        if let Some(confirm) = ctl.confirm() {
            let (title, body, yes) = match confirm {
                Confirm::Kill { title, what, .. } => (
                    format!("关闭「{title}」？"),
                    format!("{what}。关闭会结束其中的进程，历史保留在休眠组。"),
                    "关闭",
                ),
                Confirm::Delete { title, .. } => (
                    format!("删除「{title}」？"),
                    "这会删除这个 session 的全部历史（快照与记录），不可撤销。".to_string(),
                    "删除",
                ),
            };
            let resp = egui::Modal::new(Id::new("confirm")).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.label(egui::RichText::new(&title).size(15.0).strong());
                ui.add_space(6.0);
                ui.label(&body);
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(format!("{yes}（Enter）")).clicked() {
                        actions.push(UiAction::Confirm(true));
                    }
                    if ui.button("取消（Esc）").clicked() {
                        actions.push(UiAction::Confirm(false));
                    }
                });
            });
            if resp.should_close() && !actions.iter().any(|a| matches!(a, UiAction::Confirm(_))) {
                actions.push(UiAction::Confirm(false));
            }
            if let Some(p) = painted.as_deref_mut() {
                p.push((FontFamily::Proportional, format!("{title}{body}")));
            }
        }
        if chrome.palette_open {
            let resp = egui::Modal::new(Id::new("palette")).show(ctx, |ui| {
                ui.set_width(420.0);
                ui.label(egui::RichText::new("命令面板").size(15.0).strong());
                ui.label(
                    egui::RichText::new("搜索与命令在 M3 实现；目前可用的快捷键：").color(pal.dim),
                );
                ui.add_space(6.0);
                for (keys, what) in [
                    ("⌘N", "当前 workspace 新建 session"),
                    ("⌘⇧N", "新建 workspace（选择目录）"),
                    ("⌘W", "关闭 session（agent 运行时二次确认）"),
                    ("⌘1…⌘9", "跳到第 n 个 session"),
                    ("⌘C / ⌘V", "复制选区 / 粘贴"),
                    ("⇧PgUp / ⇧PgDn", "翻看历史"),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(keys).monospace().color(pal.fg));
                        ui.label(egui::RichText::new(what).color(pal.dim));
                    });
                }
                ui.add_space(6.0);
                if ui.button("关闭（Esc）").clicked() {
                    actions.push(UiAction::ClosePalette);
                }
            });
            if resp.should_close() {
                actions.push(UiAction::ClosePalette);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_video_previews_keep_a_readable_background() {
        use berth_core::{CellFlags, Color, Style};
        let theme = Theme::ghostty_default();
        let plain = preview_format(&theme, &Style::default());
        assert_eq!(plain.background, Color32::TRANSPARENT);
        assert_eq!(plain.color, c32(theme.foreground));
        let inverse = preview_format(
            &theme,
            &Style {
                flags: CellFlags::INVERSE,
                ..Style::default()
            },
        );
        assert_eq!(inverse.background, c32(theme.foreground));
        assert_eq!(inverse.color, c32(theme.background));
        let red_bg = preview_format(
            &theme,
            &Style {
                bg: Color::Indexed(1),
                ..Style::default()
            },
        );
        assert_eq!(red_bg.background, c32(theme.palette[1]));
    }

    #[test]
    fn elapsed_labels() {
        assert_eq!(format_elapsed(12_000), "12s");
        assert_eq!(format_elapsed(95_000), "1m35s");
        assert_eq!(format_elapsed(300_000), "5m00s");
        assert_eq!(format_elapsed(900_000), "15m");
        assert_eq!(format_elapsed(3_600_000), "1h00m");
        assert_eq!(format_elapsed(3 * 86_400_000), "3d");
        assert_eq!(format_elapsed(-5), "0s");
    }

    #[test]
    fn every_state_has_a_badge() {
        let states = [
            (AgentState::Idle, "○"),
            (AgentState::Thinking, "◐"),
            (
                AgentState::ToolRunning {
                    tool: "Bash".into(),
                },
                "⚙",
            ),
            (AgentState::WaitingPermission { tool: None }, "⏳"),
            (AgentState::WaitingInput, "✎"),
            (AgentState::Done, "✓"),
            (
                AgentState::Error {
                    message: "x".into(),
                },
                "✗",
            ),
            (AgentState::Exited { code: Some(0) }, "⏹"),
        ];
        for (state, glyph) in states {
            assert_eq!(badge(&state, 0), glyph, "{state:?}");
        }
        assert_eq!(badge(&AgentState::Thinking, 1), "◓", "spinner advances");
    }

    #[test]
    fn a_shell_running_a_command_reads_running() {
        let shell = AgentKind::Shell;
        let claude = AgentKind::Claude;
        assert_eq!(status_text(&shell, &AgentState::Thinking, "3s"), "运行 3s");
        assert_eq!(status_text(&claude, &AgentState::Thinking, "3s"), "思考 3s");
        assert_eq!(status_text(&shell, &AgentState::Idle, "3s"), "3s");
        assert_eq!(state_label(&shell, &AgentState::Thinking), "运行命令");
        assert_eq!(state_label(&claude, &AgentState::Thinking), "思考");
        assert_eq!(
            state_label(
                &claude,
                &AgentState::WaitingPermission {
                    tool: Some("Write".into())
                }
            ),
            "等授权：Write"
        );
        assert_eq!(
            source_text(StateSource::ShellIntegration),
            "shell 集成（OSC 133）"
        );
        assert_eq!(clean("a\u{1b}[2Jb"), "a?[2Jb");
    }

    #[test]
    fn home_is_abbreviated() {
        if let Some(home) = std::env::var_os("HOME") {
            let p = Path::new(&home).join("projects/berth");
            assert_eq!(tilde(&p), "~/projects/berth");
        }
        assert_eq!(tilde(Path::new("/tmp/x")), "/tmp/x");
    }
}
