//! `berth-hook` — must never block or fail the calling agent.
//!
//! Usage (installed by `berth setup-hooks`):
//!   berth-hook claude                       # stdin: Claude Code hook JSON
//!   berth-hook codex [-- <original notify cmd...>]   # argv[1] = JSON (Codex notify)
//!   berth-hook statusline -- <original statusline cmd...>   # tee stdin JSON
//!
//! Behaviour: read stdin (bounded), map to `HookEnvelope`, connect to
//! `BERTH_SOCKET` (or default) with a 50 ms timeout, send one frame, exit 0.
//! On any failure exit 0 silently (set `BERTH_HOOK_DEBUG=1` for stderr).
//! No tokio, no clap: startup cost matters.

fn main() {
    // Skeleton: exit 0 without side effects until implemented.
    if std::env::var_os("BERTH_HOOK_DEBUG").is_some() {
        eprintln!("berth-hook skeleton: args={:?}", std::env::args().skip(1).collect::<Vec<_>>());
    }
}
