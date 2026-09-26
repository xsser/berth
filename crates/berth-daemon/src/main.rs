//! `berthd` — see docs/DESIGN.md §3 and docs/tasks/daemon.md.
//!
//! Planned modules: `lock` (single instance), `server` (socket accept + per
//! connection framing), `manager` (registry, workspaces, restore on start),
//! `session` (actor: PTY loop → Terminal → deltas → subscribers, snapshot
//! scheduling), `agent_state` (state machine), `hooks` (HookEnvelope →
//! transitions).

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "berthd", version, about = "berth session daemon")]
struct Args {
    /// Stay in the foreground and log to stderr.
    #[arg(long)]
    foreground: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let paths = berth_core::Paths::resolve();
    eprintln!("berthd skeleton: data_dir={} socket={} foreground={}", paths.data_dir.display(), paths.socket.display(), args.foreground);
    Ok(())
}
