//! Test helpers: the real daemon (`berth_daemon::run`) in-process on a
//! temporary data directory, fake daemons of another protocol version, and
//! small executable scripts.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use berth_core::{
    decode_payload, encode_frame, ClientMsg, DaemonMsg, Event, FrameReader, Paths, Request,
};

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

/// The next client message on a fake daemon's connection.
pub fn read_client_msg(s: &mut UnixStream, frames: &mut FrameReader) -> ClientMsg {
    let mut buf = [0u8; 4096];
    loop {
        if let Some(p) = frames.next_frame().unwrap() {
            return decode_payload(&p).unwrap();
        }
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "client closed early");
        frames.push(&buf[..n]);
    }
}

pub fn write_daemon_msg(s: &mut UnixStream, reply_to: Option<u32>, event: Event) {
    s.write_all(&encode_frame(&DaemonMsg { reply_to, event }).unwrap())
        .unwrap();
}

/// What a daemon of protocol `old` does with the next `n` connections:
/// answer `Hello` with `Incompatible` and close. Returns their requests.
pub fn refuse_hellos(listener: &UnixListener, old: u32, n: usize) -> Vec<Request> {
    (0..n)
        .map(|_| {
            let (mut s, _) = listener.accept().unwrap();
            let hello = read_client_msg(&mut s, &mut FrameReader::new());
            let incompatible = Event::Incompatible {
                daemon_protocol: old,
            };
            write_daemon_msg(&mut s, Some(hello.id), incompatible);
            hello.req
        })
        .collect()
}

/// A daemon of protocol `old`: our `Hello` gets `Incompatible`; on a
/// second connection a `Hello` in its version is accepted, a message
/// this build cannot decode is pushed, `Shutdown` is answered `Ok`, and
/// the socket goes. Returns the requests it received.
pub fn fake_old_daemon(paths: &Paths, old: u32) -> JoinHandle<Vec<Request>> {
    let listener = UnixListener::bind(&paths.socket).unwrap();
    let socket = paths.socket.clone();
    std::thread::spawn(move || {
        let mut seen = refuse_hellos(&listener, old, 1);
        let (mut s, _) = listener.accept().unwrap();
        let mut frames = FrameReader::new();
        let hello = read_client_msg(&mut s, &mut frames);
        seen.push(hello.req);
        let answer = Event::Hello {
            daemon_version: "0.0.1".into(),
            protocol: old,
        };
        write_daemon_msg(&mut s, Some(hello.id), answer);
        let mut push = encode_frame(&DaemonMsg {
            reply_to: None,
            event: Event::Ok,
        })
        .unwrap();
        *push.last_mut().unwrap() = 250; // no such Event variant here
        s.write_all(&push).unwrap();
        let shutdown = read_client_msg(&mut s, &mut frames);
        seen.push(shutdown.req);
        write_daemon_msg(&mut s, Some(shutdown.id), Event::Ok);
        drop(s);
        std::fs::remove_file(&socket).unwrap();
        seen
    })
}
