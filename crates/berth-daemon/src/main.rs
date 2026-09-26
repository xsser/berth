//! `berthd` — see docs/DESIGN.md §3 and `berth_daemon::run`.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;

use anyhow::Context;
use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "berthd", version, about = "berth session daemon")]
struct Args {
    /// Stay attached to the terminal and log to stderr (default: log to
    /// `<data_dir>/logs/berthd.log` and detach from the controlling tty).
    #[arg(long)]
    foreground: bool,
}

#[allow(unsafe_code)]
fn private_umask() {
    // SAFETY: umask only changes this process's file-creation mask.
    unsafe {
        libc::umask(0o077);
    }
}

#[allow(unsafe_code)]
fn detach_from_tty() {
    // SAFETY: setsid has no memory-safety preconditions; failure (already a
    // group leader) is harmless and ignored.
    unsafe {
        libc::setsid();
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    private_umask();
    let paths = berth_core::Paths::resolve();
    paths.ensure_dirs().context("creating data directories")?;
    let filter = EnvFilter::try_from_env("BERTH_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    if args.foreground {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(paths.logs_dir.join("berthd.log"))
            .context("opening log file")?;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(log))
            .init();
        detach_from_tty();
    }
    let config = berth_daemon::Config::load(&paths.config_file);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let (tx, rx) = watch::channel(false);
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        let mut hup = signal(SignalKind::hangup())?;
        tokio::spawn(async move {
            tokio::select! {
                _ = term.recv() => tracing::info!("SIGTERM"),
                _ = int.recv() => tracing::info!("SIGINT"),
                _ = hup.recv() => tracing::info!("SIGHUP"),
            }
            tx.send_replace(true);
        });
        berth_daemon::run(paths, config, rx).await
    })
}
