//! User configuration (DESIGN §8.4): `Paths::config_file`, i.e.
//! `$BERTH_CONFIG`, else `$XDG_CONFIG_HOME/berth/config.toml`, else
//! `~/.config/berth/config.toml` — the file the daemon reads too.
//!
//! Only the sections the GUI needs are parsed; unknown keys are ignored so
//! the daemon's sections and later additions still load.

use serde::Deserialize;
use std::path::Path;

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
    fn out_of_range_size_is_an_error() {
        assert!(Config::from_toml_str("[font]\nsize = 0\n").is_err());
        assert!(Config::from_toml_str("[font]\nsize = \"big\"\n").is_err());
    }
}
