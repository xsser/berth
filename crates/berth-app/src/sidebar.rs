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
//! Context menus (DESIGN §17.2, entries from [`crate::menus`]): session
//! cards and workspace headers open theirs on a secondary click; the
//! terminal area's is opened by the app at the pointer
//! ([`Sidebar::open_terminal_menu`]). 「重命名…」 opens a dialog with a text
//! field.
//!
//! Archived sessions (DESIGN §17.1) are not cards: the 「归档 (N)」 section
//! at the end of the list, collapsed by default, has a row each — title ·
//! workspace · how long ago — with 「恢复」 / 「彻底删除…」 on hover and on
//! its context menu. No preview: berthd refuses to subscribe to them.
//!
//! IME ownership: the terminal owns the window IME. egui-winit toggles
//! `Window::set_ime_allowed` from `PlatformOutput::ime`, so the sidebar clears
//! that field every frame (egui-winit then never touches it). Keyboard and
//! IME events reach egui only while the rename dialog is open
//! ([`Sidebar::wants_keyboard`]); the other dialogs get Enter / Esc from the
//! app. The text field's cursor is kept in [`Sidebar::ime_rect`] for the
//! candidate window.
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
use egui::containers::menu::menu_style;
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{
    Align, Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, Id, LayerId, Margin,
    Order, Popup, PopupAnchor, PopupCloseBehavior, PopupKind, Pos2, Rect, Sense, SetOpenCommand,
    Stroke, Vec2,
};
use winit::window::Window;

use crate::controller::{self, Confirm, Controller, NoticeKind, ResumePreview, PREVIEW_ROWS};
use crate::menus::{self, MenuAction, MenuItem, RenameTarget};
use crate::mismatch::Banner;
use crate::panes::Axis;
use crate::setup_hooks::command_line;
use crate::theme::{contrast_ratio, mix, Rgb, Theme};
use crate::timefmt::local_clock;

const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];

fn c32(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

/// Every color the sidebar and the overlays draw with, derived from the
/// [`Theme`] plus the configured accent — nothing here is hardcoded, so a
/// custom `[theme]` moves the whole surface with it.
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
    /// Panels floating over the terminal area (notices, the mismatch
    /// banner), the fill of an error notice, and the text on that fill.
    raised: Color32,
    danger: Color32,
    danger_fg: Color32,
    /// egui's own popups (menus, dialogs), which float over the sidebar.
    popup: Color32,
    red: Color32,
    green: Color32,
    yellow: Color32,
    blue: Color32,
    magenta: Color32,
    cyan: Color32,
    /// `[theme].accent`: 「等授权」, the unread count, warnings.
    accent: Color32,
    /// [`Theme::is_light`], for egui's own light / dark visuals.
    light: bool,
}

/// How far each surface steps away from the theme's own colors. Named
/// per mode so the two sets read side by side and the checks can state
/// which color a surface is a step from: `sidebar` and `preview` step the
/// terminal background toward black, `select` / `hover` / `border` step
/// [`Palette::tint_base`] (resp. the sidebar fill) toward the foreground,
/// `raised` and `danger` step the terminal background toward the
/// foreground and toward `ansi[1]`, and `dim` / `faint` / `preview_fg`
/// fade the foreground back toward the terminal background.
struct Steps {
    sidebar: f32,
    preview: f32,
    select: f32,
    hover: f32,
    border: f32,
    raised: f32,
    danger: f32,
    dim: f32,
    faint: f32,
    preview_fg: f32,
}

/// A dark theme has room to step far toward black.
const DARK_STEPS: Steps = Steps {
    sidebar: 0.22,
    preview: 0.40,
    select: 0.12,
    hover: 0.05,
    border: 0.10,
    raised: 0.06,
    danger: 0.34,
    dim: 0.45,
    faint: 0.62,
    preview_fg: 0.25,
};

/// White goes dirty long before 0.22, so a light theme steps about a
/// twentieth of that toward black. The amounts that run toward the
/// foreground are not scaled the same way: `border` has to clear `select`
/// (they are a step from the same fill, and at equal amounts a widget's
/// stroke vanishes into its own fill), and `raised` has to lift a panel
/// off a white terminal, where a 0.06 step is 1.12:1.
const LIGHT_STEPS: Steps = Steps {
    sidebar: 0.045,
    preview: 0.075,
    select: 0.14,
    hover: 0.06,
    border: 0.26,
    raised: 0.10,
    danger: 0.16,
    dim: 0.34,
    faint: 0.42,
    preview_fg: 0.20,
};

/// The line the sidebar draws along its own edge. The pane dividers take
/// it from here rather than deriving it a second time
/// (`app::pane_chrome`): they are the same line to the eye, and a light
/// theme's amounts are not a scaled copy of a dark theme's, so a copy
/// would drift the moment either is tuned.
pub fn border(t: &Theme) -> Rgb {
    mix(
        Palette::sidebar_fill(t),
        t.foreground,
        Palette::steps(t).border,
    )
}

impl Palette {
    fn steps(t: &Theme) -> &'static Steps {
        if t.is_light() {
            &LIGHT_STEPS
        } else {
            &DARK_STEPS
        }
    }

    /// The sidebar sits one step below the terminal background, and the
    /// preview block one step further.
    fn sidebar_fill(t: &Theme) -> Rgb {
        mix(t.background, [0, 0, 0], Self::steps(t).sidebar)
    }

    /// What the selection, hover and border tints are a step away from. A
    /// light sidebar is *darker* than its terminal, so tinting the
    /// terminal background toward the foreground (what a dark theme does)
    /// lands back on the sidebar fill and disappears: at the hover amount
    /// it came out 1.01:1 against the fill it was drawn on. A light theme
    /// tints that fill instead.
    fn tint_base(t: &Theme) -> Rgb {
        if t.is_light() {
            Self::sidebar_fill(t)
        } else {
            t.background
        }
    }

    fn from_theme(t: &Theme) -> Self {
        let (fg, bg, black) = (t.foreground, t.background, [0, 0, 0]);
        let light = t.is_light();
        let s = Self::steps(t);
        let sidebar = Self::sidebar_fill(t);
        let base = Self::tint_base(t);
        let tint = |amount: f32| c32(mix(base, fg, amount));
        let fade = |amount: f32| c32(mix(fg, bg, amount));
        let text = mix(fg, bg, 0.08);
        let danger = mix(bg, t.palette[1], s.danger);
        Self {
            bg: c32(sidebar),
            fg: c32(text),
            dim: fade(s.dim),
            faint: fade(s.faint),
            select: tint(s.select),
            hover: tint(s.hover),
            preview_bg: c32(mix(bg, black, s.preview)),
            preview_fg: fade(s.preview_fg),
            border: c32(border(t)),
            raised: c32(mix(bg, fg, s.raised)),
            danger: c32(danger),
            // A custom `ansi[1]` can be any red, so the error notice takes
            // whichever of the theme's two ends reads better on it rather
            // than assuming the foreground does.
            danger_fg: c32(
                if contrast_ratio(text, danger) >= contrast_ratio(bg, danger) {
                    text
                } else {
                    bg
                },
            ),
            // A raised surface is lighter in both modes: for a light theme
            // that is the terminal background itself, above the grayer
            // sidebar.
            popup: c32(if light { bg } else { mix(bg, fg, 0.10) }),
            red: c32(t.palette[1]),
            green: c32(t.palette[2]),
            yellow: c32(t.palette[3]),
            blue: c32(t.palette[4]),
            magenta: c32(t.palette[5]),
            cyan: c32(t.palette[6]),
            accent: c32(t.accent),
            light,
        }
    }
}

/// How far the 「等授权」 pulse may dim the accent: at the trough it is
/// drawn at this fraction of full strength over the sidebar fill, which
/// is a plain `mix(bg, accent, PULSE_FLOOR)`. Below 0.80 the badge drops
/// under 3:1 in both presets (0.55 gave 2.14:1 on the light sidebar), and
/// this is the state asking the user to come and confirm something — it
/// has to stay legible through the whole cycle, not only at the peak.
const PULSE_FLOOR: f32 = 0.80;

/// Badge color of an agent state. 「等授权」 breathes between
/// [`PULSE_FLOOR`] and full strength; every other state is a flat palette
/// entry.
fn state_color(pal: Palette, state: &AgentState, now_ms: i64) -> Color32 {
    let wave = (now_ms as f64 / 1000.0 * std::f64::consts::PI).sin().abs() as f32;
    let pulse = PULSE_FLOOR + (1.0 - PULSE_FLOOR) * wave;
    match state {
        AgentState::Idle | AgentState::Exited { .. } => pal.dim,
        AgentState::Thinking => pal.blue,
        AgentState::Compacting => pal.magenta,
        AgentState::ToolRunning { .. } => pal.yellow,
        AgentState::WaitingPermission { .. } => pal.accent.gamma_multiply(pulse),
        AgentState::WaitingInput => pal.cyan,
        AgentState::Done => pal.green,
        AgentState::Error { .. } => pal.red,
    }
}

/// egui's own colors, aligned with [`Palette`]: without this the menus,
/// the rename dialog and the confirm dialogs keep egui's defaults, which
/// clash with a custom theme and, in light mode, with our own grays.
fn visuals(pal: &Palette) -> egui::Visuals {
    let mut v = if pal.light {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    v.override_text_color = Some(pal.fg);
    v.panel_fill = pal.bg;
    v.window_fill = pal.popup;
    v.window_stroke = Stroke::new(1.0, pal.border);
    v.faint_bg_color = pal.hover;
    // Backgrounds of text fields and code blocks.
    v.extreme_bg_color = pal.preview_bg;
    v.code_bg_color = pal.preview_bg;
    v.hyperlink_color = pal.accent;
    v.warn_fg_color = pal.accent;
    v.error_fg_color = pal.red;
    v.selection.bg_fill = pal.select;
    v.selection.stroke = Stroke::new(1.0, pal.fg);
    v.widgets.noninteractive.bg_fill = pal.bg;
    v.widgets.noninteractive.weak_bg_fill = pal.bg;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, pal.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, pal.dim);
    for (w, fill) in [
        (&mut v.widgets.inactive, pal.hover),
        (&mut v.widgets.hovered, pal.select),
        (&mut v.widgets.active, pal.select),
        (&mut v.widgets.open, pal.select),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, pal.border);
        w.fg_stroke = Stroke::new(1.0, pal.fg);
    }
    v
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

/// The label of a dormant card's resume button: the agent it resumes.
fn resume_label(meta: &SessionMeta) -> Option<String> {
    controller::resumable(meta)
        .then(|| meta.agent.resume_kind())
        .flatten()
        .map(|kind| format!("Resume {}", agent_name(kind)))
}

fn agent_name(kind: &AgentKind) -> String {
    match kind {
        AgentKind::Shell => "shell".into(),
        AgentKind::Claude => "claude".into(),
        AgentKind::Codex => "codex".into(),
        AgentKind::Other(name) => name.clone(),
    }
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
        agent => agent_name(agent),
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
    /// The button of the version-mismatch banner (「重启 berthd」).
    RestartDaemon,
    /// Session cards on screen this frame.
    Visible(Vec<SessionId>),
    /// A context menu entry was chosen.
    Menu(MenuAction),
    /// The rename dialog was confirmed with this text.
    Rename(RenameTarget, String),
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
    /// Panes still waiting for their first screen (points): a note each.
    pub pane_notes: Vec<(Rect, &'a str)>,
    /// The pointer is over a split divider (resize cursor).
    pub divider_hover: Option<Axis>,
    /// A berthd of another protocol version (shown instead of
    /// `placeholder`).
    pub mismatch: Option<&'a Banner>,
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
    archive_open: bool,
    jobs: Vec<egui::ClippedPrimitive>,
    screen: egui_wgpu::ScreenDescriptor,
    to_free: Vec<egui::TextureId>,
    repaint_delay: Option<Duration>,
    coverage_done: bool,
    /// `Chrome::demo_hover` of the frame being built.
    demo_hover: Option<SessionId>,
    /// The terminal area's context menu.
    term_menu: Option<TermMenu>,
    /// The rename dialog.
    rename: Option<RenameDialog>,
    /// The focused text field's cursor (points), for the IME candidate
    /// window; `None` when no text field has the keyboard.
    pub ime_rect: Option<Rect>,
}

struct TermMenu {
    pos: Pos2,
    items: Vec<MenuItem>,
    /// Open it on the next frame.
    opening: bool,
}

struct RenameDialog {
    target: RenameTarget,
    title: String,
    hint: &'static str,
    text: String,
    /// Give the field the keyboard on the next frame.
    focus: bool,
}

/// Draw menu entries; a chosen one becomes a [`UiAction::Menu`].
fn menu_entries(ui: &mut egui::Ui, items: &[MenuItem], actions: &mut Vec<UiAction>) {
    for it in items {
        let mut resp = ui.add_enabled(it.enabled, egui::Button::new(it.label));
        if let Some(hint) = it.hint {
            resp = resp.on_disabled_hover_text(hint);
        }
        if resp.clicked() {
            actions.push(UiAction::Menu(it.action));
            ui.close();
        }
    }
}

/// A click inside a context menu (a disabled entry, a gap) keeps it open;
/// a chosen entry closes it itself, a click elsewhere or Esc dismisses it.
const MENU_CLOSE: PopupCloseBehavior = PopupCloseBehavior::CloseOnClickOutside;

/// The context menu of a sidebar row (opened by a secondary click on it).
fn context_menu(resp: &egui::Response, items: &[MenuItem], actions: &mut Vec<UiAction>) {
    Popup::context_menu(resp)
        .close_behavior(MENU_CLOSE)
        .show(|ui| menu_entries(ui, items, actions));
}

/// One frame of the terminal area's context menu; false once it closed
/// (an entry was chosen, or a click outside / Esc dismissed it).
fn show_terminal_menu(
    ctx: &egui::Context,
    menu: &mut TermMenu,
    actions: &mut Vec<UiAction>,
) -> bool {
    let open = std::mem::take(&mut menu.opening).then_some(SetOpenCommand::Bool(true));
    Popup::new(
        Id::new("terminal-menu"),
        ctx.clone(),
        PopupAnchor::Position(menu.pos),
        LayerId::background(),
    )
    .kind(PopupKind::Menu)
    .layout(egui::Layout::top_down_justified(Align::Min))
    .style(menu_style)
    .gap(0.0)
    .close_behavior(MENU_CLOSE)
    .open_memory(open)
    .show(|ui| menu_entries(ui, &menu.items, actions))
    .is_some()
}

/// One frame of 「重命名…」: a text field; Enter / 「确定」 renames (the
/// action is pushed), Esc / 「取消」 or a click outside cancels. `Some`
/// once it closed (`true`: renamed).
fn show_rename(
    ctx: &egui::Context,
    pal: Palette,
    r: &mut RenameDialog,
    actions: &mut Vec<UiAction>,
) -> Option<bool> {
    let mut done: Option<bool> = None;
    let resp = egui::Modal::new(Id::new("rename")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.label(egui::RichText::new(&r.title).size(15.0).strong());
        ui.add_space(8.0);
        let edit = ui.add(
            egui::TextEdit::singleline(&mut r.text)
                .desired_width(f32::INFINITY)
                .char_limit(200),
        );
        if std::mem::take(&mut r.focus) {
            edit.request_focus();
        }
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.add_space(4.0);
        ui.label(egui::RichText::new(r.hint).size(11.0).color(pal.dim));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("确定（Enter）").clicked() || enter {
                done = Some(true);
            }
            if ui.button("取消（Esc）").clicked() {
                done = Some(false);
            }
        });
    });
    if resp.should_close() && done.is_none() {
        done = Some(false);
    }
    if done == Some(true) {
        actions.push(UiAction::Rename(r.target, r.text.clone()));
    }
    done
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

/// [`egui::Ui::put`] for a widget inside an already allocated card. `put`
/// also moves the list's cursor to just below the widget (egui assigns
/// `cursor.min.y` rather than taking the max), so the next card would start
/// there and cover whatever the card draws below it, like the resume line.
fn put_inside(ui: &mut egui::Ui, r: Rect, widget: impl egui::Widget) -> egui::Response {
    ui.new_child(
        egui::UiBuilder::new()
            .max_rect(r)
            .layout(egui::Layout::centered_and_justified(
                egui::Direction::TopDown,
            )),
    )
    .add(widget)
}

/// A collapsible group header; returns true when clicked.
fn group_header(
    ui: &mut egui::Ui,
    pal: Palette,
    painted: &mut Option<&mut Painted>,
    title: &str,
    open: Option<bool>,
) -> bool {
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

/// The 「归档 (N)」 section (only with archived sessions): a header that
/// opens and closes it, then a row per archived session while `open`.
fn archive_section(
    ui: &mut egui::Ui,
    pal: Palette,
    ctl: &Controller,
    open: &mut bool,
    now_ms: i64,
    painted: &mut Option<&mut Painted>,
    actions: &mut Vec<UiAction>,
) {
    let archived = ctl.archived();
    if archived.is_empty() {
        return;
    }
    ui.add_space(4.0);
    let title = format!("归档 ({})", archived.len());
    if group_header(ui, pal, painted, &title, Some(*open)) {
        *open = !*open;
    }
    if *open {
        for m in archived {
            archived_row(ui, pal, ctl, m, now_ms, painted, actions);
        }
    }
}

/// A row of the 「归档」 section: title · workspace · archived how long
/// ago. Under the pointer, 「恢复」 and 「彻底删除…」 buttons take the
/// age's place (the same entries as its context menu).
fn archived_row(
    ui: &mut egui::Ui,
    pal: Palette,
    ctl: &Controller,
    m: &SessionMeta,
    now_ms: i64,
    painted: &mut Option<&mut Painted>,
    actions: &mut Vec<UiAction>,
) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::hover());
    let resp = ui.interact(rect, Id::new(("archived", m.id)), Sense::click());
    context_menu(&resp, &menus::archived_row(m.id), actions);
    // Not `hovered()`: the pointer over a button is still over the row.
    let hot = ui.rect_contains_pointer(rect);
    if hot {
        ui.painter().rect_filled(rect, 6.0, pal.hover);
    }
    let y = rect.center().y;
    let mut right = rect.right() - 6.0;
    if hot {
        for (label, action, tip) in [
            (
                "彻底删除…",
                MenuAction::DeleteForever(m.id),
                "删除它的全部历史（先确认）",
            ),
            (
                "恢复",
                MenuAction::Unarchive(m.id),
                "回到原 workspace（休眠，可 Revive）",
            ),
        ] {
            let w = ui
                .painter()
                .layout_no_wrap(label.to_string(), prop(11.0), pal.fg)
                .size()
                .x
                + 14.0;
            let r = Rect::from_min_size(Pos2::new(right - w, y - 10.0), Vec2::new(w, 20.0));
            right -= w + 4.0;
            let b = egui::Button::new(egui::RichText::new(label).size(11.0));
            if put_inside(ui, r, b).on_hover_text(tip).clicked() {
                actions.push(UiAction::Menu(action));
            }
        }
    } else if let Some(at) = m.archived_at_ms {
        let age = format!("{}前", format_elapsed(now_ms.saturating_sub(at)));
        let age = paint_text(
            ui,
            painted,
            Pos2::new(right, y),
            Align2::RIGHT_CENTER,
            &age,
            prop(11.0),
            pal.faint,
            80.0,
        );
        right = age.left();
    }
    let x = rect.left() + 14.0;
    let avail = (right - 8.0 - x).max(20.0);
    let ws = ctl
        .workspace(m.workspace)
        .map_or("（workspace 已删除）", |w| w.name.as_str());
    let ws = format!(" · {ws}");
    let ws_w = ui
        .painter()
        .layout_no_wrap(ws.clone(), prop(11.0), pal.faint)
        .size()
        .x;
    let title = paint_text(
        ui,
        painted,
        Pos2::new(x, y),
        Align2::LEFT_CENTER,
        m.title(),
        prop(12.5),
        pal.preview_fg,
        avail - ws_w.min(avail * 0.45),
    );
    paint_text(
        ui,
        painted,
        Pos2::new(title.right(), y),
        Align2::LEFT_CENTER,
        &ws,
        prop(11.0),
        pal.faint,
        (x + avail - title.right()).max(1.0),
    );
    ui.add_space(2.0);
}

/// The version-mismatch banner, centered in the terminal area. Returns its
/// button, when it has one.
fn mismatch_panel(
    ctx: &egui::Context,
    pal: Palette,
    grid: Rect,
    banner: &Banner,
    painted: &mut Option<&mut Painted>,
    actions: &mut Vec<UiAction>,
) -> Option<egui::Response> {
    if let Some(p) = painted.as_deref_mut() {
        p.push((
            FontFamily::Proportional,
            format!("{}{}", banner.title, banner.body),
        ));
    }
    let width = (grid.width() - 40.0).clamp(200.0, 460.0);
    egui::Area::new(Id::new("mismatch"))
        .order(Order::Middle)
        .pivot(Align2::CENTER_CENTER)
        .fixed_pos(grid.center())
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(pal.raised)
                .stroke(Stroke::new(1.0, pal.border))
                .corner_radius(8.0)
                .inner_margin(Margin::same(16))
                .show(ui, |ui| {
                    ui.set_width(width);
                    let title = egui::RichText::new(&banner.title).size(15.0).strong();
                    ui.label(title.color(pal.fg));
                    ui.add_space(6.0);
                    let body = egui::RichText::new(&banner.body).size(13.0);
                    ui.add(egui::Label::new(body.color(pal.dim)).wrap());
                    let label = banner.button?;
                    ui.add_space(10.0);
                    let button = ui.button(label);
                    if button.clicked() {
                        actions.push(UiAction::RestartDaemon);
                    }
                    Some(button)
                })
                .inner
        })
        .inner
}

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
        let palette = Palette::from_theme(theme);
        let ctx = egui::Context::default();
        ctx.set_visuals(visuals(&palette));
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
            palette,
            theme: theme.clone(),
            width_pt,
            dormant_open: true,
            archive_open: false,
            jobs: Vec::new(),
            screen: egui_wgpu::ScreenDescriptor {
                size_in_pixels: [1, 1],
                pixels_per_point: 1.0,
            },
            to_free: Vec::new(),
            repaint_delay: None,
            coverage_done: false,
            demo_hover: None,
            term_menu: None,
            rename: None,
            ime_rect: None,
        }
    }

    /// Expand the 「归档」 section (`--demo-archive-open`).
    pub fn open_archive(&mut self) {
        self.archive_open = true;
    }

    /// Open the terminal area's context menu at `pos` (points).
    pub fn open_terminal_menu(&mut self, pos: Pos2, items: Vec<MenuItem>) {
        Popup::close_all(&self.ctx);
        self.term_menu = Some(TermMenu {
            pos,
            items,
            opening: true,
        });
    }

    /// A context menu is open: a click elsewhere only closes it.
    pub fn menu_open(&self) -> bool {
        self.term_menu.is_some() || Popup::is_any_open(&self.ctx)
    }

    /// Esc while a menu is open.
    pub fn close_menus(&mut self) {
        self.term_menu = None;
        Popup::close_all(&self.ctx);
    }

    /// 「重命名…」: the dialog, prefilled with the current name.
    pub fn begin_rename(&mut self, target: RenameTarget, current: &str) {
        let (title, hint) = match target {
            RenameTarget::Session(_) => (
                format!("重命名「{current}」"),
                "留空则恢复自动标题（程序设置的标题或命令名）",
            ),
            RenameTarget::Workspace(_) => (
                format!("重命名 workspace「{current}」"),
                "只改显示名，目录不变",
            ),
        };
        self.close_menus();
        self.rename = Some(RenameDialog {
            target,
            title,
            hint,
            text: current.to_string(),
            focus: true,
        });
    }

    /// The rename dialog is open: keyboard and IME events go to egui.
    pub fn wants_keyboard(&self) -> bool {
        self.rename.is_some()
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
        self.ime_rect = platform.ime.as_ref().map(|i| i.cursor_rect);
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
        state_color(self.palette, state, now_ms)
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
        self.terminal_menu(&ctx, actions);
        self.rename_dialog(&ctx, actions);
        if let Some(axis) = chrome.divider_hover {
            ctx.set_cursor_icon(match axis {
                Axis::Horizontal => egui::CursorIcon::ResizeColumn,
                Axis::Vertical => egui::CursorIcon::ResizeRow,
            });
        }
    }

    /// The terminal area's context menu, while open.
    fn terminal_menu(&mut self, ctx: &egui::Context, actions: &mut Vec<UiAction>) {
        if let Some(menu) = self.term_menu.as_mut() {
            if !show_terminal_menu(ctx, menu, actions) {
                self.term_menu = None;
            }
        }
    }

    /// 「重命名…」, while open.
    fn rename_dialog(&mut self, ctx: &egui::Context, actions: &mut Vec<UiAction>) {
        let pal = self.palette;
        if let Some(r) = self.rename.as_mut() {
            if show_rename(ctx, pal, r, actions).is_some() {
                self.rename = None;
            }
        }
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
            if attention > 0 { pal.accent } else { pal.dim },
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
            let head = ui.interact(rect, Id::new(("ws-head", ws.id)), Sense::click());
            let entries = menus::workspace_header(ws.id, ctl.workspace_has_sessions(ws.id));
            context_menu(&head, &entries, actions);
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
                .on_hover_text("在这个 workspace 新建 session（⌘N / ⌘T）");
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
            group_header(ui, pal, painted, "（workspace 已删除）", None);
            for m in orphans {
                self.card(ui, ctl, m, number(m.id), now_ms, painted, actions, visible);
            }
        }
        let dormant = ctl.dormant();
        if !dormant.is_empty() {
            let title = format!("休眠 ({})  只读历史 · 可 Revive", dormant.len());
            if group_header(ui, pal, painted, &title, Some(self.dormant_open)) {
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
                "没有 session：⌘N / ⌘T 新建",
                prop(12.0),
                pal.dim,
                rect.width(),
            );
        }
        archive_section(
            ui,
            pal,
            ctl,
            &mut self.archive_open,
            now_ms,
            painted,
            actions,
        );
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
        let buttons_h = match (live, resume) {
            (true, _) => 0.0,
            (false, false) => 24.0,
            (false, true) => 24.0 + 16.0,
        };
        let preview_h = PREVIEW_ROWS as f32 * LINE_H + 6.0;
        let height = 4.0 + 18.0 + 16.0 + preview_h + buttons_h + 8.0;
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
        // A stable id: the card's context menu survives the list changing.
        let resp = ui.interact(rect, Id::new(("card", m.id)), Sense::click());
        if ui.is_rect_visible(rect) {
            visible.push(m.id);
        }
        if resp.clicked() {
            actions.push(UiAction::Focus(m.id));
        }
        let row = menus::SessionRow {
            sid: m.id,
            shown: ctl.is_shown(m.id),
            unread: m.unread,
            live: m.is_live(),
        };
        context_menu(&resp, &menus::session_row(row), actions);
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
                pal.accent
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

        // Preview: a session shown in a pane from its full view, others
        // from their Preview subscription.
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
        let pane_tail;
        let (lines, styles): (&[LineSnapshot], Option<&StyleTable>) = match ctl.pane_view(m.id) {
            Some(v) if v.has_screen() => {
                pane_tail = v.tail(PREVIEW_ROWS);
                (&pane_tail, Some(v.styles()))
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
                if put_inside(ui, r, b).on_hover_text(tip).clicked() {
                    actions.push(UiAction::Revive(m.id, mode));
                }
            };
            button(
                "Revive",
                ReviveMode::Shell,
                "在原目录启动新的 shell，历史保留在上方",
            );
            if let Some(label) = resume_label(m) {
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
            if resume {
                let (text, color) = match ctl.resume_preview(m.id) {
                    Some(ResumePreview {
                        command: Ok(argv), ..
                    }) => (format!("$ {}", clean(&command_line(argv))), pal.dim),
                    Some(ResumePreview {
                        command: Err(e), ..
                    }) => (format!("不能恢复：{}", clean(e)), pal.accent),
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
                ui.label(small(format!("读取事件失败：{}", clean(e)), pal.accent));
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
            ("+ session", UiAction::NewSession, "⌘N / ⌘T"),
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
        if let Some(banner) = chrome.mismatch {
            mismatch_panel(ctx, pal, grid, banner, painted, actions);
        } else if let Some(text) = chrome.placeholder {
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
        } else {
            let painter =
                ctx.layer_painter(egui::LayerId::new(Order::Middle, Id::new("pane-notes")));
            for (rect, text) in &chrome.pane_notes {
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    *text,
                    prop(14.0),
                    pal.dim,
                );
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
                            NoticeKind::Error => (pal.danger, pal.danger_fg),
                            NoticeKind::Info => (pal.raised, pal.fg),
                        };
                        egui::Frame::new()
                            .fill(fill)
                            .stroke(Stroke::new(1.0, pal.border))
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
                Confirm::Archive { title, what, .. } => (
                    format!("归档「{title}」？"),
                    format!("{what}。归档会先结束其中的进程；历史保留，可在侧栏「归档」里恢复。"),
                    "归档",
                ),
                Confirm::Delete { title, .. } => (
                    format!("彻底删除「{title}」？"),
                    "这会删除这个 session 的全部历史（快照与记录），不可撤销。".to_string(),
                    "彻底删除",
                ),
                Confirm::DeleteWorkspace { name, .. } => (
                    format!("删除 workspace「{name}」？"),
                    "它已经没有 session；只删除侧栏里的这个分组，目录与文件不受影响。".to_string(),
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
                    ("⌘N / ⌘T", "当前 workspace 新建 session"),
                    ("⌘⇧N", "新建 workspace（选择目录）"),
                    (
                        "⌘W",
                        "关闭 pane 并归档其 session（agent 或命令运行时先确认）",
                    ),
                    ("⌘1…⌘9", "跳到第 n 个 session"),
                    ("⌘D / ⌘⇧D", "向右 / 向下分屏（新 session，同目录）"),
                    ("⌥⌘←→↑↓", "在分屏之间移动焦点"),
                    ("右键", "菜单（程序读鼠标时 ⇧+右键交给程序）"),
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
    use crate::theme::ACCENT_CONTRAST;

    /// A painted color back as the theme's own `[u8; 3]`.
    fn rgb(c: Color32) -> Rgb {
        let [r, g, b, _] = c.to_srgba_unmultiplied();
        [r, g, b]
    }

    #[test]
    fn reverse_video_previews_keep_a_readable_background() {
        use berth_core::{CellFlags, Color, Style};
        let theme = Theme::dark();
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

    /// Two surfaces that land on the same color are invisible against
    /// each other, and a check that only compares each one to the
    /// background cannot see it: the light theme's `select` and `border`
    /// were both `#d6d7d7`, so every widget's stroke disappeared into its
    /// own fill.
    #[test]
    fn no_two_sidebar_surfaces_share_a_color() {
        for theme in [Theme::light(), Theme::dark()] {
            let name = if theme.is_light() { "light" } else { "dark" };
            let p = Palette::from_theme(&theme);
            let named = [
                ("bg", p.bg),
                ("hover", p.hover),
                ("select", p.select),
                ("border", p.border),
                ("preview_bg", p.preview_bg),
                ("raised", p.raised),
                ("popup", p.popup),
            ];
            for (i, (a_name, a)) in named.iter().enumerate() {
                for (b_name, b) in &named[i + 1..] {
                    assert_ne!(a, b, "{name}: {a_name} and {b_name} are one color");
                }
            }
        }
    }

    /// The light theme's tints come off its own sidebar fill, the dark
    /// theme's off the terminal background. Deriving both from the
    /// terminal background is the bug this guards: a light sidebar is
    /// darker than its terminal, so those tints land back on the sidebar
    /// fill and vanish.
    #[test]
    fn a_light_theme_tints_its_own_fill_not_the_terminal_background() {
        let light = Theme::light();
        let dark = Theme::dark();
        let sidebar = Palette::sidebar_fill(&light);
        assert_eq!(Palette::tint_base(&light), sidebar);
        assert_eq!(Palette::tint_base(&dark), dark.background);

        let fg = light.foreground;
        let p = Palette::from_theme(&light);
        assert_eq!(rgb(p.select), mix(sidebar, fg, LIGHT_STEPS.select));
        assert_eq!(rgb(p.hover), mix(sidebar, fg, LIGHT_STEPS.hover));
        assert_ne!(rgb(p.select), mix(light.background, fg, LIGHT_STEPS.select));
        assert_ne!(rgb(p.hover), mix(light.background, fg, LIGHT_STEPS.hover));

        // Why it matters: at the hover amount the terminal-derived tint
        // is indistinguishable from the fill it is drawn on, and at both
        // amounts it is a weaker step away from that fill than ours.
        let vanished = mix(light.background, fg, LIGHT_STEPS.hover);
        assert!(
            contrast_ratio(vanished, sidebar) < 1.05,
            "{vanished:02x?} on {sidebar:02x?} would have been visible after all"
        );
        for amount in [LIGHT_STEPS.hover, LIGHT_STEPS.select] {
            let theirs = contrast_ratio(mix(light.background, fg, amount), sidebar);
            let ours = contrast_ratio(mix(sidebar, fg, amount), sidebar);
            assert!(
                ours > theirs,
                "at {amount}: {ours:.2} is no better than {theirs:.2}"
            );
        }

        // The dark theme is unchanged: its tints stay on the terminal.
        let d = Palette::from_theme(&dark);
        let dfg = dark.foreground;
        assert_eq!(rgb(d.select), mix(dark.background, dfg, DARK_STEPS.select));
        assert_eq!(rgb(d.hover), mix(dark.background, dfg, DARK_STEPS.hover));
    }

    /// 「等授权」 breathes, and the trough is the part nobody looks at
    /// when picking the colors: at the old floor of 0.55 the badge sank to
    /// 2.14:1 on the light sidebar. This is the one state that is asking
    /// the user to come and act on it.
    #[test]
    fn the_waiting_permission_pulse_stays_readable_at_its_trough() {
        let waiting = AgentState::WaitingPermission { tool: None };
        for theme in [Theme::light(), Theme::dark()] {
            let name = if theme.is_light() { "light" } else { "dark" };
            let pal = Palette::from_theme(&theme);
            // now_ms 0 is sin(0), the trough; 500 is sin(pi/2), the peak.
            let trough = state_color(pal, &waiting, 0);
            assert_eq!(trough, pal.accent.gamma_multiply(PULSE_FLOOR));
            let peak = state_color(pal, &waiting, 500);
            assert_eq!(peak, pal.accent, "{name}: the peak is the accent");

            let on_sidebar = pal.bg.blend(trough);
            let low = contrast_ratio(rgb(on_sidebar), rgb(pal.bg));
            let high = contrast_ratio(rgb(peak), rgb(pal.bg));
            println!(
                "{name}: pulse {low:.2}:1 .. {high:.2}:1 on {:02x?} (floor {PULSE_FLOOR})",
                rgb(pal.bg)
            );
            assert!(
                low >= ACCENT_CONTRAST,
                "{name}: the pulse bottoms out at {low:.2}:1 on the sidebar"
            );
            assert!(high > low, "{name}: the badge no longer breathes");
        }
    }

    /// The sidebar's surfaces are one step away from the terminal
    /// background in both directions, and every color it puts on them
    /// stays legible. The light preset is the case that used to break: its
    /// sidebar is *darker* than the terminal, so a hover tint mixed from
    /// the terminal background would land on the sidebar color and vanish.
    #[test]
    fn both_presets_keep_the_sidebar_readable_and_its_surfaces_apart() {
        for theme in [Theme::light(), Theme::dark()] {
            let name = if theme.is_light() { "light" } else { "dark" };
            let p = Palette::from_theme(&theme);
            let bg = rgb(p.bg);
            assert_eq!(p.light, theme.is_light());
            assert_ne!(bg, theme.background, "{name}: sidebar = terminal");
            for (label, c) in [
                ("hover", p.hover),
                ("select", p.select),
                ("border", p.border),
                ("preview_bg", p.preview_bg),
            ] {
                assert_ne!(rgb(c), bg, "{name}: {label} disappears into the sidebar");
            }
            // Ordered: hover is the lightest touch, select the stronger one.
            let d = |c: Color32| contrast_ratio(rgb(c), bg);
            assert!(d(p.hover) < d(p.select), "{name}: hover >= select");
            for (label, c, floor) in [
                ("fg", p.fg, 4.5),
                ("preview_fg", p.preview_fg, 4.5),
                ("dim", p.dim, 3.0),
                ("faint", p.faint, 2.5),
                ("accent", p.accent, 3.0),
                ("red", p.red, 3.0),
                ("green", p.green, 3.0),
                ("yellow", p.yellow, 3.0),
                ("blue", p.blue, 3.0),
                ("magenta", p.magenta, 3.0),
                ("cyan", p.cyan, 3.0),
            ] {
                let r = contrast_ratio(rgb(c), bg);
                assert!(r >= floor, "{name}: {label} is {r:.2}:1 on the sidebar");
            }
            // Notices and the mismatch banner float over the terminal.
            for (label, fill, on) in [
                ("raised", p.raised, p.fg),
                ("danger", p.danger, p.danger_fg),
            ] {
                assert_ne!(rgb(fill), theme.background, "{name}: {label} invisible");
                let r = contrast_ratio(rgb(on), rgb(fill));
                assert!(r >= 4.5, "{name}: text on {label} is {r:.2}:1");
            }
        }
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
    fn buttons_inside_a_card_leave_the_next_card_where_it_was() {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let (card, _) = ui.allocate_exact_size(Vec2::new(200.0, 100.0), Sense::hover());
            let next = ui.cursor().top();
            assert!(next >= card.bottom());
            let r = Rect::from_min_size(card.min + Vec2::new(10.0, 40.0), Vec2::new(60.0, 20.0));
            let revive = put_inside(ui, r, egui::Button::new("Revive"));
            let resume = put_inside(
                ui,
                r.translate(Vec2::new(70.0, 0.0)),
                egui::Button::new("Resume"),
            );
            assert_ne!(revive.id, resume.id);
            assert_eq!(ui.cursor().top(), next, "the next card would overlap");
        });
        // No renderer here: the font atlas upload is dropped on purpose.
        out.textures_delta.clear();
    }

    #[test]
    fn the_mismatch_banner_button_asks_for_the_restart() {
        use crate::client::Incompatible;
        use crate::mismatch::Mismatch;
        let ctx = egui::Context::default();
        let pal = Palette::from_theme(&Theme::dark());
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 600.0));
        let grid = Rect::from_min_max(Pos2::new(240.0, 0.0), screen.max);
        let frame = |events: Vec<egui::Event>, banner: &Banner| {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            };
            let (mut button, mut actions) = (None, Vec::new());
            let mut out = ctx.run_ui(input, |ui| {
                let shown = mismatch_panel(ui.ctx(), pal, grid, banner, &mut None, &mut actions);
                button = shown.map(|r| r.rect);
            });
            // No renderer here: the font atlas upload is dropped on purpose.
            out.textures_delta.clear();
            (button, actions)
        };
        let mut m = Mismatch::None;
        m.connect_failed(Some(Incompatible {
            daemon: berth_core::PROTOCOL_VERSION - 1,
        }));
        let banner = m.banner().unwrap();
        // A new area is measured on its first frame and placed on the next.
        frame(Vec::new(), &banner);
        let (button, actions) = frame(Vec::new(), &banner);
        let at = button.expect("a restart button").center();
        assert!(grid.contains(at), "{at:?}");
        assert!(actions.is_empty());
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![egui::Event::PointerMoved(at)], &banner);
        frame(vec![press(true)], &banner);
        let (_, actions) = frame(vec![press(false)], &banner);
        assert_eq!(actions, vec![UiAction::RestartDaemon]);

        assert!(m.restart());
        let (button, _) = frame(Vec::new(), &m.banner().unwrap());
        assert_eq!(button, None, "nothing to press while it stops");
    }

    #[test]
    fn a_session_whose_agent_left_offers_to_resume_that_agent() {
        let mut m = SessionMeta {
            status: SessionStatus::Restored,
            command: vec!["/bin/zsh".into()],
            ..SessionMeta::default()
        };
        m.agent.external_id = Some("abc".into());
        assert_eq!(resume_label(&m), None, "a plain shell");
        m.agent.last_agent = Some(AgentKind::Claude);
        assert_eq!(resume_label(&m).as_deref(), Some("Resume claude"));
        assert_eq!(kind_label(&m), "zsh", "the card still shows what runs");
        m.agent.kind = AgentKind::Codex;
        assert_eq!(resume_label(&m).as_deref(), Some("Resume codex"));
        m.status = SessionStatus::Live;
        assert_eq!(resume_label(&m), None, "live: nothing to resume");
    }

    #[test]
    fn home_is_abbreviated() {
        if let Some(home) = std::env::var_os("HOME") {
            let p = Path::new(&home).join("projects/berth");
            assert_eq!(tilde(&p), "~/projects/berth");
        }
        assert_eq!(tilde(Path::new("/tmp/x")), "/tmp/x");
    }

    /// Where `text` was painted (its first occurrence).
    fn painted_at(out: &egui::FullOutput, text: &str) -> Option<Rect> {
        fn walk(shape: &egui::Shape, text: &str) -> Option<Rect> {
            match shape {
                egui::Shape::Text(t) if t.galley.text() == text => {
                    Some(t.galley.rect.translate(t.pos.to_vec2()))
                }
                egui::Shape::Vec(v) => v.iter().find_map(|s| walk(s, text)),
                _ => None,
            }
        }
        out.shapes.iter().find_map(|c| walk(&c.shape, text))
    }

    /// Pointer over `at`, press, release: one frame each.
    fn click_at(at: Pos2) -> [Vec<egui::Event>; 3] {
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        [
            vec![egui::Event::PointerMoved(at)],
            vec![press(true)],
            vec![press(false)],
        ]
    }

    fn headless_frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        mut show: impl FnMut(&egui::Context),
    ) -> egui::FullOutput {
        headless_ui(ctx, events, |ui| show(ui.ctx()))
    }

    /// One frame of `show` in the whole (1000×600) screen.
    fn headless_ui(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        mut show: impl FnMut(&mut egui::Ui),
    ) -> egui::FullOutput {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 600.0))),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| show(ui));
        // No renderer here: the font atlas upload is dropped on purpose.
        out.textures_delta.clear();
        out
    }

    /// A controller that listed `sessions` (requests go nowhere).
    fn listed(wss: Vec<berth_core::Workspace>, sessions: Vec<SessionMeta>) -> Controller {
        struct Nowhere(u32);
        impl controller::Outbound for Nowhere {
            fn send(&mut self, _: berth_core::Request) -> anyhow::Result<u32> {
                self.0 += 1;
                Ok(self.0)
            }
        }
        let mut c = Controller::new(vec![]);
        c.auto_session = false;
        let mut out = Nowhere(0);
        c.on_connected(&mut out); // ListWorkspaces = 1, ListSessions = 2
        let now = std::time::Instant::now();
        let answer = |id, event| berth_core::DaemonMsg {
            reply_to: Some(id),
            event,
        };
        c.handle(&mut out, answer(1, berth_core::Event::Workspaces(wss)), now);
        c.handle(
            &mut out,
            answer(2, berth_core::Event::Sessions(sessions)),
            now,
        );
        assert!(c.is_loaded());
        c
    }

    #[test]
    fn the_archive_section_opens_and_its_rows_restore_or_delete() {
        let ws = berth_core::Workspace {
            id: WorkspaceId::new(),
            name: "proj".into(),
            root: "/tmp/proj".into(),
            color: None,
            order: 0,
            created_at_ms: 0,
        };
        let now_ms = 10 * 86_400_000;
        let live = SessionMeta {
            id: SessionId::new(),
            workspace: ws.id,
            title_auto: "live".into(),
            status: SessionStatus::Live,
            ..Default::default()
        };
        let old = SessionMeta {
            id: SessionId::new(),
            workspace: ws.id,
            title_auto: "old-build".into(),
            status: SessionStatus::Restored,
            archived_at_ms: Some(now_ms - 3 * 86_400_000),
            ..Default::default()
        };
        let ctl = listed(vec![ws], vec![live, old.clone()]);
        let pal = Palette::from_theme(&Theme::dark());
        let ctx = egui::Context::default();
        // egui's own fonts, with the "bold" family headers use.
        let mut fonts = FontDefinitions::default();
        let regular = fonts.families[&FontFamily::Proportional].clone();
        fonts
            .families
            .insert(FontFamily::Name("bold".into()), regular);
        ctx.set_fonts(fonts);
        let mut open = false;
        let mut frame = |events| {
            let mut actions = Vec::new();
            let out = headless_ui(&ctx, events, |ui| {
                archive_section(ui, pal, &ctl, &mut open, now_ms, &mut None, &mut actions);
            });
            (actions, out)
        };
        // Collapsed: the header only.
        let (_, out) = frame(Vec::new());
        let header = painted_at(&out, "归档 (1)").expect("归档 (1)");
        assert!(painted_at(&out, "old-build").is_none());
        for events in click_at(header.center()) {
            frame(events);
        }
        // Open: title · workspace · how long ago; no buttons until hovered.
        let away = vec![egui::Event::PointerMoved(Pos2::new(500.0, 500.0))];
        let (_, out) = frame(away.clone());
        let title = painted_at(&out, "old-build").expect("the row");
        assert!(painted_at(&out, " · proj").is_some());
        assert!(painted_at(&out, "3d前").is_some());
        assert!(painted_at(&out, "恢复").is_none());
        // Hovered: 「恢复」 and 「彻底删除…」 instead of the age.
        let (_, out) = frame(vec![egui::Event::PointerMoved(title.center())]);
        assert!(painted_at(&out, "3d前").is_none());
        assert!(painted_at(&out, "彻底删除…").is_some());
        let restore = painted_at(&out, "恢复").expect("恢复");
        let mut got = Vec::new();
        for events in click_at(restore.center()) {
            got.extend(frame(events).0);
        }
        assert_eq!(got, [UiAction::Menu(MenuAction::Unarchive(old.id))]);
        // Its context menu has the same entries.
        let secondary = |pressed| egui::Event::PointerButton {
            pos: title.center(),
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![secondary(true)]);
        frame(vec![secondary(false)]);
        frame(away.clone());
        let (_, out) = frame(away);
        let delete = painted_at(&out, "彻底删除…").expect("the menu");
        let mut got = Vec::new();
        for events in click_at(delete.center()) {
            got.extend(frame(events).0);
        }
        assert_eq!(got, [UiAction::Menu(MenuAction::DeleteForever(old.id))]);
    }

    #[test]
    fn the_terminal_menu_turns_a_click_into_its_action() {
        use crate::panes::SplitDir;
        let ctx = egui::Context::default();
        let sid = SessionId::new();
        let mut menu = TermMenu {
            pos: Pos2::new(500.0, 200.0),
            items: menus::terminal(menus::TerminalArea {
                sid,
                has_selection: false,
                panes: 1,
            }),
            opening: true,
        };
        let frame = |menu: &mut TermMenu, events| {
            let (mut open, mut actions) = (false, Vec::new());
            let out = headless_frame(&ctx, events, |ctx| {
                open = show_terminal_menu(ctx, menu, &mut actions);
            });
            (open, actions, out)
        };
        // A new area is measured on its first frame and placed on the next.
        assert!(frame(&mut menu, Vec::new()).0);
        let (open, actions, out) = frame(&mut menu, Vec::new());
        assert!(open && actions.is_empty());
        let right = painted_at(&out, "向右分屏").expect("向右分屏");
        let remove = painted_at(&out, "从分屏移除").expect("从分屏移除");
        assert!(painted_at(&out, "粘贴").is_some());
        assert!(painted_at(&out, "复制").is_none(), "nothing selected");
        assert!(right.min.x >= 500.0 && right.min.y >= 200.0, "{right:?}");
        // The only pane cannot leave the split: nothing happens, the menu
        // stays.
        for events in click_at(remove.center()) {
            let (open, actions, _) = frame(&mut menu, events);
            assert!(open && actions.is_empty(), "{actions:?}");
        }
        let mut got = Vec::new();
        for events in click_at(right.center()) {
            got.extend(frame(&mut menu, events).1);
        }
        assert_eq!(got, [UiAction::Menu(MenuAction::Split(SplitDir::Right))]);
        assert!(!frame(&mut menu, Vec::new()).0, "closed by the choice");

        // A click elsewhere only dismisses it.
        let mut menu = TermMenu {
            opening: true,
            ..menu
        };
        frame(&mut menu, Vec::new());
        frame(&mut menu, Vec::new());
        let mut got = Vec::new();
        for events in click_at(Pos2::new(100.0, 500.0)) {
            got.extend(frame(&mut menu, events).1);
        }
        assert!(got.is_empty(), "{got:?}");
        assert!(!frame(&mut menu, Vec::new()).0, "dismissed");
    }

    /// The dialogs and the context menus are egui's own widgets, so they
    /// paint from `Visuals`, not from our [`Palette`]. Without
    /// [`visuals`] they would keep egui's grays, which in light mode are a
    /// different set from the sidebar's and in any custom theme are simply
    /// wrong. This runs a real frame of the rename dialog and of the
    /// terminal menu and checks the theme's colors reach the shapes.
    #[test]
    fn egui_dialogs_and_menus_paint_with_the_theme() {
        fn fills(shape: &egui::Shape, out: &mut Vec<Color32>) {
            match shape {
                egui::Shape::Rect(r) => {
                    out.push(r.fill);
                    out.push(r.stroke.color);
                }
                egui::Shape::Vec(v) => v.iter().for_each(|s| fills(s, out)),
                _ => {}
            }
        }
        let painted = |out: &egui::FullOutput| {
            let mut v = Vec::new();
            for c in &out.shapes {
                fills(&c.shape, &mut v);
            }
            v
        };
        const FADE_IN_FRAMES: usize = 16;
        for theme in [Theme::light(), Theme::dark()] {
            let name = if theme.is_light() { "light" } else { "dark" };
            let pal = Palette::from_theme(&theme);
            let ctx = egui::Context::default();
            ctx.set_visuals(visuals(&pal));
            let egui_own = if theme.is_light() {
                egui::Visuals::light()
            } else {
                egui::Visuals::dark()
            }
            .window_fill;

            let mut dialog = RenameDialog {
                target: RenameTarget::Session(SessionId::new()),
                title: "重命名「build」".into(),
                hint: "留空则恢复自动标题",
                text: "build".into(),
                focus: true,
            };
            // Two frames: the first lays the modal out, the second paints
            // it at its measured size.
            // egui fades a modal in, so the first frames paint it
            // translucent; FADE_IN_FRAMES is well past the end of that.
            let mut shapes = Vec::new();
            for _ in 0..FADE_IN_FRAMES {
                let out = headless_frame(&ctx, Vec::new(), |ctx| {
                    show_rename(ctx, pal, &mut dialog, &mut Vec::new());
                });
                shapes = painted(&out);
            }
            assert!(
                shapes.contains(&pal.popup),
                "{name}: the rename dialog is not on the theme's popup fill"
            );
            assert!(
                shapes.contains(&pal.border),
                "{name}: the rename dialog has no theme border"
            );
            assert!(
                egui_own == pal.popup || !shapes.contains(&egui_own),
                "{name}: egui's own window fill is still painted"
            );

            let mut menu = TermMenu {
                pos: Pos2::new(400.0, 300.0),
                items: menus::terminal(menus::TerminalArea {
                    sid: SessionId::new(),
                    has_selection: true,
                    panes: 2,
                }),
                opening: true,
            };
            let mut shapes = Vec::new();
            for _ in 0..FADE_IN_FRAMES {
                let out = headless_frame(&ctx, Vec::new(), |ctx| {
                    show_terminal_menu(ctx, &mut menu, &mut Vec::new());
                });
                shapes = painted(&out);
            }
            assert!(
                shapes.contains(&pal.popup),
                "{name}: the terminal menu is not on the theme's popup fill"
            );
        }
    }

    #[test]
    fn the_rename_dialog_confirms_with_enter_and_cancels_with_escape() {
        let ctx = egui::Context::default();
        let pal = Palette::from_theme(&Theme::dark());
        let sid = SessionId::new();
        let dialog = || RenameDialog {
            target: RenameTarget::Session(sid),
            title: "重命名「build」".into(),
            hint: "留空则恢复自动标题（程序设置的标题或命令名）",
            text: "build".into(),
            focus: true,
        };
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let frame = |r: &mut RenameDialog, events| {
            let (mut done, mut actions) = (None, Vec::new());
            headless_frame(&ctx, events, |ctx| {
                done = show_rename(ctx, pal, r, &mut actions);
            });
            (done, actions)
        };
        let mut r = dialog();
        assert_eq!(frame(&mut r, Vec::new()), (None, vec![]));
        let typed = vec![egui::Event::Text(" 2".into())];
        assert_eq!(frame(&mut r, typed), (None, vec![]));
        let (done, actions) = frame(&mut r, vec![key(egui::Key::Enter)]);
        assert_eq!(done, Some(true));
        assert_eq!(
            actions,
            [UiAction::Rename(
                RenameTarget::Session(sid),
                "build 2".into()
            )]
        );

        let mut r = dialog();
        frame(&mut r, Vec::new());
        assert_eq!(
            frame(&mut r, vec![key(egui::Key::Escape)]),
            (Some(false), vec![])
        );
    }
}
