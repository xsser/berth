//! Test helpers: the real daemon (`berth_daemon::run`) in-process on a
//! temporary data directory, and small executable scripts.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use berth_core::Paths;

/// berthd on its own runtime thread; stopped (sessions saved) on drop.
pub struct TestDaemon {
    pub paths: Paths,
    stop: tokio::sync::watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
    _dir: Option<tempfile::TempDir>,
}

impl TestDaemon {
    /// A daemon on a fresh temporary directory.
    pub fn start() -> TestDaemon {
        TestDaemon::start_with(berth_daemon::Config::default())
    }

    /// Like [`TestDaemon::start`], with this daemon configuration (e.g. a
    /// small scrollback).
    pub fn start_with(config: berth_daemon::Config) -> TestDaemon {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut daemon = TestDaemon::spawn(Paths::in_dir(dir.path()), config);
        daemon._dir = Some(dir);
        daemon
    }

    /// A daemon on `paths`; returns once its socket accepts connections.
    pub fn start_at(paths: Paths) -> TestDaemon {
        TestDaemon::spawn(paths, berth_daemon::Config::default())
    }

    fn spawn(paths: Paths, config: berth_daemon::Config) -> TestDaemon {
        let (stop, rx) = tokio::sync::watch::channel(false);
        let p = paths.clone();
        let thread = std::thread::Builder::new()
            .name("test-berthd".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                if let Err(e) = rt.block_on(berth_daemon::run(p, config, rx)) {
                    panic!("test daemon failed: {e:#}");
                }
            })
            .expect("daemon thread");
        let deadline = Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(&paths.socket).is_err() {
            assert!(Instant::now() < deadline, "test daemon did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        TestDaemon {
            paths,
            stop,
            thread: Some(thread),
            _dir: None,
        }
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Write an executable `#!/bin/sh` script.
pub fn write_script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}
