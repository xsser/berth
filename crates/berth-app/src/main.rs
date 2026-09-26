//! `berth` — GUI client and CLI. See docs/DESIGN.md §8 and docs/tasks/app-spike.md.
//!
//! Planned modules: `app` (winit ApplicationHandler), `renderer/{grid, atlas,
//! text, shaders}`, `sidebar` (egui), `input` (key encoding), `ime`, `client`
//! (daemon connection + mirrored state), `config`, `keybinds`, `notify`,
//! `setup_hooks`.

mod config;
mod fixture;
mod ime;
mod input;
mod renderer;
mod terminal;
mod theme;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "berth", version, about = "berth terminal")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
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
    let cli = Cli::parse();
    match cli.cmd {
        None => eprintln!("berth GUI skeleton (not implemented yet)"),
        Some(c) => eprintln!("berth {:?}: not implemented yet", c),
    }
    Ok(())
}
