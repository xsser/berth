//! `berth` — GUI client and CLI. See docs/DESIGN.md §8 and
//! docs/tasks/integrate.md.
//!
//! Without a subcommand the GUI starts: it connects to `berthd` (launching
//! it when nothing listens) and shows the daemon's sessions.
//! Modules: `app` (winit ApplicationHandler, run modes), `client` (daemon
//! connection), `controller` (protocol state), `session_view` (screen and
//! history mirror), `renderer/{grid, atlas, text, metrics, sprites,
//! shaders.wgsl}`, `sidebar` (egui), `input` (key encoding), `ime`, `mouse`,
//! `paste`, `selection`, `notify`, `dock` (Dock badge), `cli` (list / doctor
//! / debug), `setup_hooks`, `fixture` (`--bench` data), `theme`, `timefmt`,
//! `config`, `stats`.

mod app;
mod cli;
mod client;
mod config;
mod controller;
mod dock;
mod fixture;
mod ime;
mod input;
mod mouse;
mod notify;
mod paste;
mod renderer;
mod selection;
mod session_view;
mod setup_hooks;
mod sidebar;
mod stats;
#[cfg(test)]
mod testutil;
mod theme;
mod timefmt;

use std::path::PathBuf;
use std::time::Duration;

use berth_core::{CursorShape, Paths};
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
    /// Save the window as a PNG once the session list and the focused
    /// screen are shown (works while the screen is locked), then exit.
    #[arg(long, value_name = "PNG", conflicts_with = "bench")]
    screenshot: Option<PathBuf>,
    /// Extra wait before the screenshot (previews, late output).
    #[arg(
        long,
        value_name = "MS",
        default_value_t = 800,
        requires = "screenshot"
    )]
    screenshot_delay_ms: u64,
    /// Focus this session (id or unique prefix) at start.
    #[arg(long, value_name = "ID")]
    session: Option<String>,
    /// Scroll the focused session N lines into its history once shown.
    #[arg(long, value_name = "N", hide = true)]
    scroll: Option<u32>,
    /// Time every frame (GPU-synchronised) and print statistics to stderr
    /// every 5 s and at exit.
    #[arg(long, conflicts_with = "bench")]
    stats: bool,
    /// Quit after SECS seconds.
    #[arg(long, value_name = "SECS", value_parser = parse_secs)]
    exit_after: Option<f64>,
    /// Initial window size in cells (default 120x40).
    #[arg(long, value_name = "COLSxROWS", value_parser = parse_cells)]
    cells: Option<(u16, u16)>,
    /// Renderer benchmark on static fixture data (no daemon): render
    /// continuously for SECS per phase with GPU-synchronised frame timing,
    /// print statistics to stderr and exit.
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
    /// Show this live session's hover details without a pointer
    /// (screenshot check only; id or unique prefix).
    #[arg(long, hide = true, value_name = "ID")]
    demo_hover: Option<String>,
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

/// `COLSxROWS`, e.g. `120x40`.
fn parse_cells(s: &str) -> std::result::Result<(u16, u16), String> {
    let (c, r) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| "expected COLSxROWS, e.g. 120x40".to_string())?;
    let parse = |v: &str| -> std::result::Result<u16, String> {
        let n: u16 = v.trim().parse().map_err(|e| format!("{v:?}: {e}"))?;
        if (2..=1000).contains(&n) {
            Ok(n)
        } else {
            Err(format!("{n} is outside 2..=1000"))
        }
    };
    Ok((parse(c)?, parse(r)?))
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List workspaces and sessions known to the daemon.
    List,
    /// Show / install / undo berth's hook entries for Claude Code
    /// (~/.claude/settings.json) or Codex (~/.codex/config.toml). Only
    /// `--yes` writes, after a backup.
    SetupHooks(setup_hooks::Args),
    /// Read-only checks: daemon, socket permissions, hooks, login PATH.
    Doctor,
    /// Scripted checks against a running daemon.
    #[command(hide = true, subcommand)]
    Debug(cli::DebugCmd),
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
        None => app::run(gui_options(cli.gui)),
        Some(Cmd::List) => cli::list(&Paths::resolve()),
        Some(Cmd::Doctor) => cli::doctor(&Paths::resolve()),
        Some(Cmd::Debug(cmd)) => cli::debug(&Paths::resolve(), cmd),
        Some(Cmd::SetupHooks(args)) => {
            let report = setup_hooks::run(&args, &setup_hooks::Env::from_process()?)?;
            print!("{report}");
            Ok(())
        }
    }
}

fn gui_options(gui: GuiArgs) -> app::GuiOptions {
    app::GuiOptions {
        screenshot_delay: if gui.screenshot.is_some() {
            Duration::from_millis(gui.screenshot_delay_ms)
        } else {
            Duration::ZERO
        },
        screenshot: gui.screenshot,
        session: gui.session,
        scroll: gui.scroll,
        stats: gui.stats,
        exit_after: gui.exit_after.map(Duration::from_secs_f64),
        bench_secs: gui.bench,
        cells: gui.cells,
        font_family: gui.font_family,
        font_size: gui.font_size,
        no_vsync: gui.no_vsync,
        cursor_style: gui.cursor_style.map(CursorShape::from),
        demo_preedit: gui.demo_preedit,
        demo_hover: gui.demo_hover,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_parse() {
        assert_eq!(parse_cells("120x40"), Ok((120, 40)));
        assert_eq!(parse_cells("80X24"), Ok((80, 24)));
        assert!(parse_cells("80").is_err());
        assert!(parse_cells("1x24").is_err());
    }

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
        let cli = Cli::try_parse_from([
            "berth",
            "--screenshot",
            "/tmp/s.png",
            "--screenshot-delay-ms",
            "1500",
            "--session",
            "ab12",
            "--cells",
            "90x30",
            "--stats",
        ])
        .unwrap();
        let opts = gui_options(cli.gui);
        assert_eq!(opts.screenshot_delay, Duration::from_millis(1500));
        assert_eq!(opts.session.as_deref(), Some("ab12"));
        assert_eq!(opts.cells, Some((90, 30)));
        assert!(opts.stats);
        assert!(Cli::try_parse_from(["berth", "--screenshot-delay-ms", "5"]).is_err());
        assert!(Cli::try_parse_from(["berth", "--stats", "--bench", "5"]).is_err());
        let cli = Cli::try_parse_from(["berth", "debug", "send", "ab12", "ls\\r"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Some(Cmd::Debug(cli::DebugCmd::Send { ref session, .. })) if session == "ab12"
        ));
        let cli = Cli::try_parse_from([
            "berth",
            "debug",
            "new-session",
            "--dir",
            "/tmp",
            "--",
            "/bin/sh",
            "-c",
            "true",
        ])
        .unwrap();
        match cli.cmd {
            Some(Cmd::Debug(cli::DebugCmd::NewSession { command, size, .. })) => {
                assert_eq!(command, ["/bin/sh", "-c", "true"]);
                assert_eq!(size, (120, 40));
            }
            other => panic!("{other:?}"),
        }
    }
}
