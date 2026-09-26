//! `berth` — GUI client and CLI. See docs/DESIGN.md §8 and docs/tasks/app-spike.md.
//!
//! Without a subcommand the GUI starts (M0: fixture data, no daemon).
//! Modules: `app` (winit ApplicationHandler, run modes), `renderer/{grid,
//! atlas, text, metrics, sprites, shaders.wgsl}`, `sidebar` (egui), `input`
//! (key encoding), `ime`, `terminal` (client-side view state), `fixture`,
//! `theme`, `config`, `stats`. Planned: `client` (daemon connection),
//! `keybinds`, `notify`, `setup_hooks`.

mod app;
mod config;
mod fixture;
mod ime;
mod input;
mod renderer;
mod sidebar;
mod stats;
mod terminal;
mod theme;

use std::path::PathBuf;

use berth_core::CursorShape;
use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "berth", version, about = "berth terminal")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    gui: GuiArgs,
}

/// GUI options (used when no subcommand is given).
#[derive(Args, Debug, Default)]
struct GuiArgs {
    /// Render 3 frames, save the window contents as a PNG and exit.
    #[arg(long, value_name = "PNG", conflicts_with = "bench")]
    screenshot: Option<PathBuf>,
    /// Render continuously for SECS per phase with GPU-synchronised frame
    /// timing, print statistics to stderr and exit.
    #[arg(long, value_name = "SECS", value_parser = parse_secs)]
    bench: Option<f64>,
    /// Terminal font family (overrides the config file; default "SF Mono").
    #[arg(long, value_name = "NAME")]
    font_family: Option<String>,
    /// Terminal font size in points (default 13).
    #[arg(long, value_name = "PT", value_parser = parse_font_size)]
    font_size: Option<f32>,
    /// Present without vsync to measure the uncapped frame rate.
    #[arg(long)]
    no_vsync: bool,
    /// Cursor shape (Ghostty `cursor-style`).
    #[arg(long, value_enum, value_name = "STYLE")]
    cursor_style: Option<CursorStyle>,
    /// Inject a synthetic IME preedit at startup (screenshot check only).
    #[arg(long, hide = true, value_name = "TEXT")]
    demo_preedit: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CursorStyle {
    Block,
    Beam,
    Underline,
    Hollow,
}

impl From<CursorStyle> for CursorShape {
    fn from(s: CursorStyle) -> Self {
        match s {
            CursorStyle::Block => CursorShape::Block,
            CursorStyle::Beam => CursorShape::Beam,
            CursorStyle::Underline => CursorShape::Underline,
            CursorStyle::Hollow => CursorShape::HollowBlock,
        }
    }
}

fn parse_secs(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if v.is_finite() && v > 0.0 && v <= 600.0 {
        Ok(v)
    } else {
        Err("expected 0 < SECS <= 600".into())
    }
}

fn parse_font_size(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|e| format!("{e}"))?;
    if (4.0..=96.0).contains(&v) {
        Ok(v)
    } else {
        Err("expected 4 <= PT <= 96".into())
    }
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List sessions known to the daemon.
    List,
    /// Install / preview / undo Claude Code and Codex hook entries (explicit, never silent).
    SetupHooks,
    /// Check daemon, socket, hooks and shell integration.
    Doctor,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,berth=info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        None => app::run(app::GuiOptions {
            screenshot: cli.gui.screenshot,
            bench_secs: cli.gui.bench,
            font_family: cli.gui.font_family,
            font_size: cli.gui.font_size,
            no_vsync: cli.gui.no_vsync,
            cursor_style: cli.gui.cursor_style.map(CursorShape::from),
            demo_preedit: cli.gui.demo_preedit,
        }),
        Some(c) => {
            eprintln!("berth {c:?}: not implemented yet (M0 spike)");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_gui_flags_and_subcommands() {
        let cli = Cli::try_parse_from(["berth", "--screenshot", "/tmp/x.png", "--font-size", "14"])
            .unwrap();
        assert!(cli.cmd.is_none());
        assert_eq!(
            cli.gui.screenshot.as_deref(),
            Some(std::path::Path::new("/tmp/x.png"))
        );
        assert_eq!(cli.gui.font_size, Some(14.0));
        assert!(Cli::try_parse_from(["berth", "--screenshot", "a.png", "--bench", "5"]).is_err());
        assert!(Cli::try_parse_from(["berth", "--bench", "0"]).is_err());
        let cli = Cli::try_parse_from(["berth", "--cursor-style", "beam"]).unwrap();
        assert_eq!(
            cli.gui.cursor_style.map(CursorShape::from),
            Some(CursorShape::Beam)
        );
        let cli = Cli::try_parse_from(["berth", "doctor"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::Doctor)));
    }
}
