//! Left sidebar (DESIGN §8.3): egui + egui-winit + egui-wgpu sharing the
//! terminal's device, queue, surface and render pass.
//!
//! IME ownership: the terminal owns the window IME. egui-winit toggles
//! `Window::set_ime_allowed` from `PlatformOutput::ime`, so the sidebar clears
//! that field every frame and is never fed keyboard or IME events. (M0 has no
//! egui text field; a future search box must arbitrate explicitly.)
//!
//! Fonts: egui's bundled fonts have no CJK, so PingFang SC (fallback Hiragino
//! Sans GB / Heiti SC) is located through cosmic-text's fontdb and handed to
//! egui zero-copy (the memory-mapped face is kept alive for the process).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use berth_core::{AgentKind, AgentState, SessionId, SessionMeta};
use cosmic_text::{fontdb, FontSystem};
use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, Margin, Pos2, Rect, Sense,
    Stroke, Vec2,
};
use winit::window::Window;

use crate::fixture::SidebarFixture;
use crate::theme::{mix, Rgb, Theme};

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

fn status_text(state: &AgentState, elapsed: &str) -> String {
    match state {
        AgentState::Idle => elapsed.to_string(),
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

fn kind_label(meta: &SessionMeta) -> String {
    match &meta.agent.kind {
        AgentKind::Shell => meta
            .command
            .first()
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
    fixture: SidebarFixture,
    palette: Palette,
    width_pt: f32,
    jobs: Vec<egui::ClippedPrimitive>,
    screen: egui_wgpu::ScreenDescriptor,
    to_free: Vec<egui::TextureId>,
    repaint_delay: Option<Duration>,
    coverage_done: bool,
}

/// Strings painted in one pass, with the family used (for the coverage log).
type Painted = Vec<(FontFamily, String)>;

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
            fixture: SidebarFixture::build(berth_core::now_ms()),
            palette: Palette::from_theme(theme),
            width_pt,
            jobs: Vec::new(),
            screen: egui_wgpu::ScreenDescriptor {
                size_in_pixels: [1, 1],
                pixels_per_point: 1.0,
            },
            to_free: Vec::new(),
            repaint_delay: None,
            coverage_done: false,
        }
    }

    /// Forward a (non-keyboard, non-IME) window event. Returns true when egui
    /// wants a repaint.
    pub fn on_window_event(&mut self, window: &Window, event: &winit::event::WindowEvent) -> bool {
        self.state.on_window_event(window, event).repaint
    }

    /// When egui asked to be repainted (hover animations etc.).
    pub fn repaint_delay(&self) -> Option<Duration> {
        self.repaint_delay
    }

    /// Run egui, tessellate, upload textures and buffers. The returned
    /// command buffers must be submitted before `encoder`'s.
    pub fn prepare(
        &mut self,
        window: &Window,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        size_px: [u32; 2],
        now_ms: i64,
    ) -> Vec<wgpu::CommandBuffer> {
        let raw = self.state.take_egui_input(window);
        let ctx = self.ctx.clone();
        let mut painted: Painted = Vec::new();
        let collect = !self.coverage_done;
        let mut clicked = None;
        let mut full = ctx.run_ui(raw, |ui| {
            painted.clear();
            clicked = self.ui(ui, now_ms, collect.then_some(&mut painted));
        });
        if let Some(id) = clicked {
            self.fixture.focused = id;
        }
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
        self.renderer
            .update_buffers(device, queue, encoder, &self.jobs, &self.screen)
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

    /// Paints the panel; returns a clicked session.
    fn ui(
        &self,
        ui: &mut egui::Ui,
        now_ms: i64,
        mut painted: Option<&mut Painted>,
    ) -> Option<SessionId> {
        let pal = self.palette;
        let mut clicked = None;
        let tick = (now_ms.max(0) / 250) as u64;
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
                let mut text = |ui: &egui::Ui,
                                pos: Pos2,
                                anchor: Align2,
                                s: &str,
                                font: FontId,
                                color: Color32,
                                max_w: f32| {
                    if let Some(p) = painted.as_deref_mut() {
                        p.push((font.family.clone(), s.to_string()));
                    }
                    let mut job = LayoutJob::simple_singleline(s.to_string(), font, color);
                    job.wrap = TextWrapping::truncate_at_width(max_w);
                    let galley = ui.painter().layout_job(job);
                    let rect = anchor.anchor_size(pos, galley.size());
                    ui.painter().galley(rect.min, galley, color);
                    rect
                };
                let prop = |size: f32| FontId::proportional(size);
                let bold = |size: f32| FontId::new(size, FontFamily::Name("bold".into()));
                let mono = |size: f32| FontId::monospace(size);

                // Header.
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::hover());
                text(
                    ui,
                    rect.left_center(),
                    Align2::LEFT_CENTER,
                    "berth",
                    bold(15.0),
                    pal.fg,
                    80.0,
                );
                let attention = self
                    .fixture
                    .sessions
                    .iter()
                    .filter(|s| s.agent.state.needs_attention())
                    .count();
                let busy = self
                    .fixture
                    .sessions
                    .iter()
                    .filter(|s| s.agent.state.is_busy())
                    .count();
                text(
                    ui,
                    rect.right_center(),
                    Align2::RIGHT_CENTER,
                    &format!("{busy} 运行 · {attention} 需关注"),
                    prop(11.0),
                    if attention > 0 { pal.orange } else { pal.dim },
                    170.0,
                );
                ui.add_space(4.0);

                for ws in &self.fixture.workspaces {
                    // Workspace header: ▾ ● name  ~/root             [+]
                    let (rect, _) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::hover());
                    let y = rect.center().y;
                    text(
                        ui,
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
                    let name = text(
                        ui,
                        Pos2::new(rect.left() + 26.0, y),
                        Align2::LEFT_CENTER,
                        &ws.name,
                        bold(13.0),
                        pal.fg,
                        120.0,
                    );
                    text(
                        ui,
                        Pos2::new(name.right() + 8.0, y),
                        Align2::LEFT_CENTER,
                        &tilde(&ws.root),
                        prop(11.0),
                        pal.faint,
                        (rect.right() - 22.0 - name.right() - 8.0).max(10.0),
                    );
                    text(
                        ui,
                        Pos2::new(rect.right(), y),
                        Align2::RIGHT_CENTER,
                        "+",
                        prop(14.0),
                        pal.dim,
                        14.0,
                    );

                    let mut sessions: Vec<(usize, &SessionMeta)> = self
                        .fixture
                        .sessions
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.workspace == ws.id)
                        .collect();
                    sessions.sort_by_key(|(_, s)| s.order);
                    for (idx, s) in sessions {
                        let height = 4.0 + 18.0 + 16.0 + 3.0 * 14.0 + 6.0 + 6.0;
                        let (rect, resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), height),
                            Sense::click(),
                        );
                        if resp.clicked() {
                            clicked = Some(s.id);
                        }
                        let focused = s.id == self.fixture.focused;
                        if focused || resp.hovered() {
                            ui.painter().rect_filled(
                                rect,
                                6.0,
                                if focused { pal.select } else { pal.hover },
                            );
                        }
                        let state = &s.agent.state;
                        let elapsed = format_elapsed(now_ms - s.agent.since_ms);
                        let pulse = 0.55
                            + 0.45
                                * ((now_ms as f64 / 1000.0 * std::f64::consts::PI).sin().abs()
                                    as f32);
                        let color = match state {
                            AgentState::Idle | AgentState::Exited { .. } => pal.dim,
                            AgentState::Thinking => pal.blue,
                            AgentState::Compacting => pal.magenta,
                            AgentState::ToolRunning { .. } => pal.yellow,
                            AgentState::WaitingPermission { .. } => {
                                pal.orange.gamma_multiply(pulse)
                            }
                            AgentState::WaitingInput => pal.cyan,
                            AgentState::Done => pal.green,
                            AgentState::Error { .. } => pal.red,
                        };
                        let x0 = rect.left() + 6.0;
                        let y1 = rect.top() + 4.0 + 9.0;
                        text(
                            ui,
                            Pos2::new(x0 + 6.0, y1),
                            Align2::CENTER_CENTER,
                            badge(state, tick),
                            prop(14.0),
                            color,
                            20.0,
                        );
                        let kind = text(
                            ui,
                            Pos2::new(x0 + 18.0, y1),
                            Align2::LEFT_CENTER,
                            &kind_label(s),
                            mono(11.0),
                            pal.dim,
                            60.0,
                        );
                        let status = status_text(state, &elapsed);
                        let status_rect = text(
                            ui,
                            Pos2::new(rect.right() - 6.0, y1),
                            Align2::RIGHT_CENTER,
                            &status,
                            prop(11.0),
                            if state.needs_attention() {
                                color
                            } else {
                                pal.dim
                            },
                            110.0,
                        );
                        let title_font = if s.unread { bold(13.0) } else { prop(13.0) };
                        let title_x = kind.right().max(x0 + 58.0) + 6.0;
                        let dot_room = if s.unread { 12.0 } else { 0.0 };
                        let title = text(
                            ui,
                            Pos2::new(title_x, y1),
                            Align2::LEFT_CENTER,
                            s.title(),
                            title_font,
                            if s.unread { pal.fg } else { pal.preview_fg },
                            (status_rect.left() - 8.0 - dot_room - title_x).max(20.0),
                        );
                        if s.unread {
                            ui.painter().circle_filled(
                                Pos2::new(title.right() + 6.0, y1),
                                3.0,
                                pal.blue,
                            );
                        }
                        let y2 = rect.top() + 4.0 + 18.0 + 8.0;
                        text(
                            ui,
                            Pos2::new(x0 + 18.0, y2),
                            Align2::LEFT_CENTER,
                            &tilde(&s.cwd),
                            prop(11.0),
                            pal.faint,
                            rect.width() - 36.0,
                        );

                        // 3-line monospace preview.
                        let top = rect.top() + 4.0 + 18.0 + 16.0 + 2.0;
                        let preview = Rect::from_min_max(
                            Pos2::new(x0 + 16.0, top),
                            Pos2::new(rect.right() - 6.0, top + 3.0 * 14.0 + 6.0),
                        );
                        ui.painter().rect_filled(preview, 4.0, pal.preview_bg);
                        ui.painter().vline(
                            preview.left() + 1.0,
                            preview.top() + 3.0..=preview.bottom() - 3.0,
                            Stroke::new(2.0, color),
                        );
                        if let Some(lines) = self.fixture.previews.get(idx) {
                            for (i, line) in lines.iter().take(3).enumerate() {
                                text(
                                    ui,
                                    Pos2::new(
                                        preview.left() + 8.0,
                                        preview.top() + 3.0 + i as f32 * 14.0,
                                    ),
                                    Align2::LEFT_TOP,
                                    &line.text_trimmed(),
                                    mono(11.0),
                                    pal.preview_fg,
                                    preview.width() - 12.0,
                                );
                            }
                        }
                        ui.add_space(2.0);
                    }
                    ui.add_space(6.0);
                }
            });
        clicked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn home_is_abbreviated() {
        if let Some(home) = std::env::var_os("HOME") {
            let p = Path::new(&home).join("projects/berth");
            assert_eq!(tilde(&p), "~/projects/berth");
        }
        assert_eq!(tilde(Path::new("/tmp/x")), "/tmp/x");
    }
}
