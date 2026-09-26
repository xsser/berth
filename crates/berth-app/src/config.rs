//! User configuration (`~/.config/berth/config.toml`, DESIGN §8.4).
//!
//! Only the sections the M0 spike needs are parsed; unknown keys are ignored
//! so a config written for later milestones still loads.

use serde::Deserialize;
use std::path::{Path, PathBuf};

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font: FontConfig::default(),
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    font: FileFont,
    #[serde(default)]
    sidebar: FileSidebar,
}

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
        Ok(cfg)
    }

    /// `~/.config/berth/config.toml` (same path on macOS and Linux, per DESIGN §8.4).
    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|home| Path::new(&home).join(".config/berth/config.toml"))
    }

    /// Load the user config if present. A missing file yields defaults; a
    /// malformed file is reported and ignored so the terminal still starts.
    pub fn load() -> Self {
        let Some(path) = Self::default_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(src) => match Self::from_toml_str(&src) {
                Ok(cfg) => cfg,
                Err(err) => {
                    tracing::warn!(path = %path.display(), "ignoring invalid config: {err:#}");
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                tracing::warn!(path = %path.display(), "cannot read config: {err}");
                Self::default()
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
    fn out_of_range_size_is_an_error() {
        assert!(Config::from_toml_str("[font]\nsize = 0\n").is_err());
        assert!(Config::from_toml_str("[font]\nsize = \"big\"\n").is_err());
    }
}
