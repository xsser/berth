//! User configuration (DESIGN §8.4): `Paths::config_file`, i.e.
//! `$BERTH_CONFIG`, else `$XDG_CONFIG_HOME/berth/config.toml`, else
//! `~/.config/berth/config.toml` — the file the daemon reads too.
//!
//! Only the sections the GUI needs are parsed; unknown keys are ignored so
//! the daemon's sections and later additions still load.
//!
//! `[theme]` picks a preset (`light`, the default, or `dark`) and overrides
//! any of its colors. A value that is not a color costs that one key: it is
//! skipped with a warning and the rest of the file still applies, because a
//! typo in one of seventeen colors should not send the whole theme back to
//! the preset.

use serde::Deserialize;
use std::path::Path;

use crate::theme::{
    contrast_ratio, xterm_palette, Rgb, Theme, ACCENT_CONTRAST, BODY_TEXT_CONTRAST,
};

pub const DEFAULT_FONT_FAMILY: &str = "SF Mono";
pub const DEFAULT_FONT_SIZE: f32 = 13.0;
pub const DEFAULT_SIDEBAR_WIDTH: f32 = 280.0;

#[derive(Clone, Debug, PartialEq)]
pub struct FontConfig {
    pub family: String,
    /// Size in points; multiplied by the window scale factor for pixels.
    pub size: f32,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: DEFAULT_FONT_FAMILY.to_string(),
            size: DEFAULT_FONT_SIZE,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub font: FontConfig,
    /// Terminal colors and the accent, the source every other color in the
    /// window is derived from (`[theme]`).
    pub theme: Theme,
    /// Sidebar width in logical points.
    pub sidebar_width: f32,
    /// Agent states (`AgentState::name`) that raise a desktop notification.
    pub notify_on: Vec<String>,
    /// Bundle id desktop notifications appear under (`[notify].identity`).
    pub notify_identity: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font: FontConfig::default(),
            theme: Theme::default(),
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            notify_on: crate::notify::DEFAULT_ON
                .iter()
                .map(|s| s.to_string())
                .collect(),
            notify_identity: crate::notify::DEFAULT_IDENTITY.to_string(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    font: FileFont,
    #[serde(default)]
    sidebar: FileSidebar,
    #[serde(default)]
    notify: FileNotify,
    #[serde(default)]
    theme: FileTheme,
}

#[derive(Debug, Default, Deserialize)]
struct FileTheme {
    preset: Option<String>,
    background: Option<String>,
    foreground: Option<String>,
    cursor: Option<String>,
    cursor_text: Option<String>,
    accent: Option<String>,
    ansi: Option<Vec<String>>,
}

/// `#rgb` or `#rrggbb`, case-insensitive, the `#` optional.
fn parse_color(s: &str) -> Option<Rgb> {
    let h = s.trim();
    let h = h.strip_prefix('#').unwrap_or(h).as_bytes();
    if !h.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let d = |b: u8| (b as char).to_digit(16).expect("checked above") as u8;
    match h.len() {
        3 => Some([d(h[0]) * 0x11, d(h[1]) * 0x11, d(h[2]) * 0x11]),
        6 => Some([
            d(h[0]) * 16 + d(h[1]),
            d(h[2]) * 16 + d(h[3]),
            d(h[4]) * 16 + d(h[5]),
        ]),
        _ => None,
    }
}

/// One `[theme]` color key: `None` when unset, and `None` plus a warning
/// naming the key and the value when it does not parse.
///
/// Two strategies for a bad value live in this parser, on purpose for
/// now. `[theme]` degrades per key: the key is skipped with a warning
/// and everything else in the document still applies, because a typo in
/// one of seventeen colors should not send the whole theme back to the
/// preset. The older keys reject the document instead: `font.size`,
/// `sidebar.width`, `notify.on` and `notify.identity` each return an
/// `Err` from `from_toml_str`, so one bad bundle id takes `[theme]` and
/// every other setting down with it.
///
/// That asymmetry is a real gap, and the wrong half is the old one: a
/// config file is read once at startup, so rejecting it wholesale costs
/// the user every other setting to punish one. Unifying on per-key
/// degradation is tracked for v1.1; it is left alone here because it
/// changes behaviour outside this change's subject.
fn color_key(key: &str, value: Option<String>) -> Option<Rgb> {
    let raw = value?;
    match parse_color(&raw) {
        Some(c) => Some(c),
        None => {
            tracing::warn!("theme.{key} = {raw:?} 不是颜色（如 \"#ffffff\"），已跳过该键");
            None
        }
    }
}

/// `[theme]` applied on top of a preset.
fn theme_from_file(f: FileTheme) -> Theme {
    let mut theme = match f.preset.as_deref().map(str::trim) {
        None | Some("light") => Theme::light(),
        Some("dark") => Theme::dark(),
        Some(other) => {
            tracing::warn!("theme.preset = {other:?} 不是 \"light\" 或 \"dark\"，已用 light");
            Theme::light()
        }
    };
    if let Some(list) = f.ansi {
        if list.len() == 16 {
            let mut named: [Rgb; 16] = theme.palette[..16].try_into().expect("16 entries");
            for (i, raw) in list.iter().enumerate() {
                match parse_color(raw) {
                    Some(c) => named[i] = c,
                    None => tracing::warn!("theme.ansi[{i}] = {raw:?} 不是颜色，保留预设色"),
                }
            }
            theme.palette = xterm_palette(&named);
        } else {
            tracing::warn!(
                "theme.ansi 有 {} 个颜色，需要正好 16 个，已整段忽略",
                list.len()
            );
        }
    }
    if let Some(c) = color_key("background", f.background) {
        theme.background = c;
    }
    if let Some(c) = color_key("foreground", f.foreground) {
        theme.foreground = c;
    }
    // Ghostty semantics: an unset cursor color means the foreground and an
    // unset cursor text means the background, so both follow an override of
    // those rather than keeping the preset's.
    theme.cursor = color_key("cursor", f.cursor).unwrap_or(theme.foreground);
    theme.cursor_text = color_key("cursor_text", f.cursor_text).unwrap_or(theme.background);
    // The accent follows the background that ended up in effect, so a dark
    // `background` override gets the dark accent without naming a preset.
    theme.accent = color_key("accent", f.accent).unwrap_or_else(|| theme.default_accent());
    // Custom colors are the user's call, so these only warn: a pair below
    // the floor is legal but is almost always a mistake, and it is far
    // easier to explain here than from a screenshot. The two floors are
    // the ones the presets are held to — body text against the background,
    // and the accent as a marker on it.
    let ratio = contrast_ratio(theme.foreground, theme.background);
    if ratio < BODY_TEXT_CONTRAST {
        tracing::warn!(
            "theme.foreground {:02x?} 在 theme.background {:02x?} 上对比度只有 {ratio:.2}:1（正文需要 {BODY_TEXT_CONTRAST}:1）",
            theme.foreground,
            theme.background
        );
    }
    let ratio = contrast_ratio(theme.accent, theme.background);
    if ratio < ACCENT_CONTRAST {
        tracing::warn!(
            "theme.accent {:02x?} 在 theme.background {:02x?} 上对比度只有 {ratio:.2}:1（强调色需要 {ACCENT_CONTRAST}:1）",
            theme.accent,
            theme.background
        );
    }
    theme
}

#[derive(Debug, Default, Deserialize)]
struct FileNotify {
    on: Option<Vec<String>>,
    identity: Option<String>,
}

/// A bundle id as Apple defines it: letters, digits, `-` and `.`, with at
/// least one `.` (reverse DNS), e.g. `com.apple.Terminal`.
fn is_bundle_id(s: &str) -> bool {
    s.contains('.')
        && !s.starts_with('.')
        && !s.ends_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// States `[notify].on` may list.
const NOTIFY_STATES: [&str; 4] = ["waiting_permission", "waiting_input", "done", "error"];

#[derive(Debug, Default, Deserialize)]
struct FileFont {
    family: Option<String>,
    size: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
struct FileSidebar {
    width: Option<f32>,
}

impl Config {
    /// Parse a config document; missing keys keep their defaults.
    pub fn from_toml_str(src: &str) -> anyhow::Result<Self> {
        let file: FileConfig = toml::from_str(src)?;
        let mut cfg = Config::default();
        if let Some(family) = file.font.family.filter(|f| !f.trim().is_empty()) {
            cfg.font.family = family;
        }
        if let Some(size) = file.font.size {
            anyhow::ensure!(
                size.is_finite() && (4.0..=96.0).contains(&size),
                "font.size {size} out of range 4..=96"
            );
            cfg.font.size = size;
        }
        if let Some(width) = file.sidebar.width {
            anyhow::ensure!(
                width.is_finite() && (120.0..=800.0).contains(&width),
                "sidebar.width {width} out of range"
            );
            cfg.sidebar_width = width;
        }
        if let Some(on) = file.notify.on {
            if let Some(bad) = on.iter().find(|s| !NOTIFY_STATES.contains(&s.as_str())) {
                anyhow::bail!(
                    "notify.on: unknown state {bad:?} (expected some of {NOTIFY_STATES:?})"
                );
            }
            cfg.notify_on = on;
        }
        if let Some(identity) = file.notify.identity {
            anyhow::ensure!(
                is_bundle_id(&identity),
                "notify.identity {identity:?} is not a bundle id (e.g. \"com.apple.Terminal\")"
            );
            cfg.notify_identity = identity;
        }
        cfg.theme = theme_from_file(file.theme);
        Ok(cfg)
    }

    /// Load `path` if present. A missing file yields defaults; an unreadable
    /// or malformed one yields defaults plus a message for the user.
    pub fn load_from(path: &Path) -> (Self, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(src) => match Self::from_toml_str(&src) {
                Ok(cfg) => (cfg, None),
                Err(err) => {
                    let msg = format!("配置文件 {} 无效，已使用默认值：{err:#}", path.display());
                    tracing::warn!("{msg}");
                    (Self::default(), Some(msg))
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(err) => {
                let msg = format!("无法读取配置文件 {}：{err}", path.display());
                tracing::warn!("{msg}");
                (Self::default(), Some(msg))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_gives_defaults() {
        let cfg = Config::from_toml_str("").unwrap();
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.font.family, "SF Mono");
        assert_eq!(cfg.font.size, 13.0);
    }

    #[test]
    fn font_section_overrides_and_unknown_keys_are_ignored() {
        let src = "[font]\nfamily = \"Menlo\"\nsize = 14.5\n[terminal]\nscrollback = 20000\n[sidebar]\nwidth = 300\n";
        let cfg = Config::from_toml_str(src).unwrap();
        assert_eq!(cfg.font.family, "Menlo");
        assert_eq!(cfg.font.size, 14.5);
        assert_eq!(cfg.sidebar_width, 300.0);
    }

    #[test]
    fn notify_identity_is_configurable_and_checked() {
        assert_eq!(Config::default().notify_identity, "com.apple.Terminal");
        let cfg = Config::from_toml_str("[notify]\nidentity = \"dev.berth.app\"\n").unwrap();
        assert_eq!(cfg.notify_identity, "dev.berth.app");
        assert_eq!(
            cfg.notify_on,
            Config::default().notify_on,
            "on keeps its default"
        );
        for bad in [
            "",
            "Terminal",
            "com.apple.Terminal app",
            ".com.x",
            "com.x.",
            "com/x.y",
        ] {
            let doc = format!("[notify]\nidentity = {bad:?}\n");
            assert!(Config::from_toml_str(&doc).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn notify_states_are_configurable_and_checked() {
        assert_eq!(Config::default().notify_on.len(), 4);
        let cfg = Config::from_toml_str("[notify]\non = [\"done\"]\n").unwrap();
        assert_eq!(cfg.notify_on, ["done"]);
        let off = Config::from_toml_str("[notify]\non = []\n").unwrap();
        assert!(off.notify_on.is_empty());
        assert!(Config::from_toml_str("[notify]\non = [\"thinking\"]\n").is_err());
    }

    #[test]
    fn missing_and_invalid_files_fall_back_with_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, msg) = Config::load_from(&dir.path().join("none.toml"));
        assert_eq!((cfg, msg), (Config::default(), None));
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[font]\nsize = 0\n").unwrap();
        let (cfg, msg) = Config::load_from(&bad);
        assert_eq!(cfg, Config::default());
        assert!(msg.unwrap().contains("bad.toml"));
    }

    #[test]
    fn colors_accept_three_and_six_digits_with_or_without_hash() {
        assert_eq!(parse_color("#ffffff"), Some([255, 255, 255]));
        assert_eq!(parse_color("FFFFFF"), Some([255, 255, 255]));
        assert_eq!(parse_color("#DE935f"), Some([0xde, 0x93, 0x5f]));
        assert_eq!(parse_color(" #f0a "), Some([0xff, 0x00, 0xaa]));
        assert_eq!(parse_color("0a7d55"), Some([0x0a, 0x7d, 0x55]));
        for bad in [
            "",
            "#",
            "#ff",
            "#fffff",
            "#ggg",
            "#1234567",
            "rebeccapurple",
        ] {
            assert_eq!(parse_color(bad), None, "{bad:?} accepted");
        }
    }

    #[test]
    fn theme_defaults_to_light() {
        let cfg = Config::from_toml_str("").unwrap();
        assert_eq!(cfg.theme, Theme::light());
        assert_eq!(cfg.theme.background, [0xff, 0xff, 0xff]);
        assert!(cfg.theme.is_light());
        assert_eq!(cfg.theme.accent, crate::theme::LIGHT_ACCENT);
        // An empty table is the same as no table at all.
        assert_eq!(Config::from_toml_str("[theme]\n").unwrap(), cfg);
    }

    #[test]
    fn theme_preset_selects_dark() {
        let cfg = Config::from_toml_str("[theme]\npreset = \"dark\"\n").unwrap();
        assert_eq!(cfg.theme, Theme::dark());
        assert!(!cfg.theme.is_light());
        assert_eq!(cfg.theme.accent, crate::theme::DARK_ACCENT);
        // An unknown preset warns and falls back to light.
        let cfg = Config::from_toml_str("[theme]\npreset = \"solarized\"\n").unwrap();
        assert_eq!(cfg.theme, Theme::light());
    }

    #[test]
    fn theme_keys_override_the_preset_one_by_one() {
        let src = "[theme]\npreset = \"dark\"\nbackground = \"#fffdf6\"\naccent = \"#0a7d55\"\n";
        let cfg = Config::from_toml_str(src).unwrap();
        assert_eq!(cfg.theme.background, [0xff, 0xfd, 0xf6]);
        assert_eq!(cfg.theme.foreground, Theme::dark().foreground, "kept");
        assert_eq!(cfg.theme.accent, [0x0a, 0x7d, 0x55]);
        // cursor / cursor_text follow the overridden background by default
        // and take an explicit value when given.
        assert_eq!(cfg.theme.cursor_text, [0xff, 0xfd, 0xf6]);
        assert_eq!(cfg.theme.cursor, cfg.theme.foreground);
        let src = "[theme]\ncursor = \"#f00\"\ncursor_text = \"#00f\"\nforeground = \"#111\"\n";
        let cfg = Config::from_toml_str(src).unwrap();
        assert_eq!(cfg.theme.cursor, [0xff, 0, 0]);
        assert_eq!(cfg.theme.cursor_text, [0, 0, 0xff]);
        assert_eq!(cfg.theme.foreground, [0x11, 0x11, 0x11]);
        // No accent given, and the background is still light.
        assert_eq!(cfg.theme.accent, crate::theme::LIGHT_ACCENT);
        // A dark background override alone moves the accent with it.
        let cfg = Config::from_toml_str("[theme]\nbackground = \"#101418\"\n").unwrap();
        assert_eq!(cfg.theme.accent, crate::theme::DARK_ACCENT);
    }

    #[test]
    fn theme_ansi_replaces_the_named_colors_and_rebuilds_the_cube() {
        let mut list: Vec<String> = (0..16).map(|i| format!("#{i:02x}0000")).collect();
        let src = format!("[theme]\nansi = {list:?}\n");
        let cfg = Config::from_toml_str(&src).unwrap();
        assert_eq!(cfg.theme.palette[1], [0x01, 0, 0]);
        assert_eq!(cfg.theme.palette[15], [0x0f, 0, 0]);
        assert_eq!(cfg.theme.palette[16..], Theme::light().palette[16..]);
        // Not exactly sixteen: the whole key is ignored.
        list.pop();
        let src = format!("[theme]\nansi = {list:?}\n");
        let cfg = Config::from_toml_str(&src).unwrap();
        assert_eq!(cfg.theme.palette, Theme::light().palette);
        // One bad entry keeps the preset color at that index only.
        let mut list: Vec<String> = (0..16).map(|i| format!("#{i:02x}0000")).collect();
        list[2] = "chartreuse".into();
        let src = format!("[theme]\nansi = {list:?}\n");
        let cfg = Config::from_toml_str(&src).unwrap();
        assert_eq!(cfg.theme.palette[2], Theme::light().palette[2]);
        assert_eq!(cfg.theme.palette[3], [0x03, 0, 0]);
    }

    #[test]
    fn a_bad_theme_color_is_skipped_and_the_other_keys_still_apply() {
        let src = "[theme]\npreset = \"dark\"\nbackground = \"not a color\"\nforeground = \"#abcdef\"\naccent = \"#12\"\ncursor = \"\"\n";
        let cfg = Config::from_toml_str(src).expect("a bad color is not a parse error");
        assert_eq!(cfg.theme.background, Theme::dark().background, "skipped");
        assert_eq!(cfg.theme.foreground, [0xab, 0xcd, 0xef], "applied");
        assert_eq!(
            cfg.theme.accent,
            crate::theme::DARK_ACCENT,
            "back to the preset"
        );
        assert_eq!(cfg.theme.cursor, [0xab, 0xcd, 0xef], "= foreground");
    }

    #[test]
    fn an_unreadable_custom_pair_warns_but_still_loads() {
        let src =
            "[theme]\nbackground = \"#ffffff\"\nforeground = \"#eeeeee\"\naccent = \"#f5f5f5\"\n";
        let cfg = Config::from_toml_str(src).expect("the user's colors are the user's call");
        assert_eq!(cfg.theme.foreground, [0xee, 0xee, 0xee]);
        assert_eq!(cfg.theme.accent, [0xf5, 0xf5, 0xf5]);
        let bg = cfg.theme.background;
        assert!(contrast_ratio(cfg.theme.foreground, bg) < BODY_TEXT_CONTRAST);
        assert!(contrast_ratio(cfg.theme.accent, bg) < ACCENT_CONTRAST);
        // Both presets are well clear of the lines the warnings draw.
        for t in [Theme::light(), Theme::dark()] {
            assert!(contrast_ratio(t.foreground, t.background) >= BODY_TEXT_CONTRAST);
            assert!(contrast_ratio(t.accent, t.background) >= ACCENT_CONTRAST);
        }
    }

    #[test]
    fn theme_ignores_unknown_keys_and_leaves_other_sections_alone() {
        let src = "[theme]\npreset = \"dark\"\nselection = \"#123456\"\n[font]\nsize = 15\n";
        let cfg = Config::from_toml_str(src).unwrap();
        assert_eq!(cfg.theme, Theme::dark());
        assert_eq!(cfg.font.size, 15.0);
    }

    #[test]
    fn out_of_range_size_is_an_error() {
        assert!(Config::from_toml_str("[font]\nsize = 0\n").is_err());
        assert!(Config::from_toml_str("[font]\nsize = \"big\"\n").is_err());
    }
}
