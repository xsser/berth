//! `berthd` library: `run` starts the daemon in-process (used by `main.rs`
//! and by integration tests with a temporary `Paths::in_dir`).
//!
//! Modules: `lock` (single instance), `config`, `server` (socket + framing),
//! `outbox` (per-connection coalescing queue), `manager` (registry,
//! persistence, restore, hook routing, archiving), `session` (per-session
//! actor thread: PTY → Terminal → deltas / previews / snapshots), `view`
//! (virtual line space helpers), `agent_state` (DESIGN §9 state machine),
//! `hooks` (`HookEnvelope` → session), `archive` (DESIGN §17.1: the
//! automatic archive scan).
#![deny(unsafe_code)]

pub mod agent_state;
pub mod archive;
pub mod config;
pub mod hooks;
pub mod lock;
pub mod manager;
pub mod outbox;
pub mod server;
mod session;
pub mod shell_integration;
pub mod view;

use std::os::unix::fs::PermissionsExt;

use anyhow::Context;
use berth_core::Paths;
use berth_store::Store;
use tokio::net::UnixListener;
use tokio::sync::watch;

pub use config::Config;
pub use session::{BATCH, MAX_FETCH_LINES};

/// Run the daemon until `shutdown` turns true or a client sends `Shutdown`.
/// Returns `Ok(())` without doing anything if another berthd holds the lock
/// (the owner's pid is printed).
pub async fn run(
    paths: Paths,
    config: Config,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    paths.ensure_dirs().context("creating data directories")?;
    let _lock = match lock::acquire(&paths).context("acquiring berthd.lock")? {
        lock::LockOutcome::Acquired(lock) => lock,
        lock::LockOutcome::Busy { pid } => {
            let msg = match pid {
                Some(pid) => format!("berthd already running (pid {pid})"),
                None => "berthd already running".to_string(),
            };
            println!("{msg}");
            tracing::info!("{msg}");
            return Ok(());
        }
    };
    // We own the lock, so any socket file is a leftover of a dead daemon.
    match std::fs::remove_file(&paths.socket) {
        Ok(()) => tracing::info!(socket = %paths.socket.display(), "removed stale socket"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("removing stale socket"),
    }
    let store = Store::open(&paths).context("opening store")?;
    let (stop_tx, stop_rx) = watch::channel(false);
    let mgr = manager::Manager::start(paths.clone(), config, store, stop_tx.clone())?;
    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("binding {}", paths.socket.display()))?;
    std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))
        .context("restricting socket permissions")?;
    tracing::info!(socket = %paths.socket.display(), pid = std::process::id(), "berthd listening");

    let forward = tokio::spawn(async move {
        let mut shutdown = shutdown;
        wait_true(&mut shutdown).await;
        stop_tx.send_replace(true);
    });
    let scanner = tokio::spawn(archive::run_scanner(
        mgr.clone(),
        stop_rx.clone(),
        archive::FIRST_SCAN,
        archive::SCAN_EVERY,
        berth_core::now_ms,
    ));
    server::serve(listener, mgr, stop_rx).await;
    scanner.abort();
    forward.abort();
    if let Err(e) = std::fs::remove_file(&paths.socket) {
        tracing::debug!(error = %e, "socket already gone");
    }
    tracing::info!("berthd stopped");
    Ok(())
}

/// Resolve once the watch value is true. A dropped sender never fires.
pub(crate) async fn wait_true(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}
