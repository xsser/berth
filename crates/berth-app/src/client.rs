//! Connection to `berthd` (DESIGN §3.2, §7; integrate.md §1 and §6).
//!
//! - [`connect_or_spawn`]: connect to `Paths::socket`; when nothing listens,
//!   launch `berthd` (next to this executable, else `$PATH`, else the login
//!   shell's PATH) without `--foreground` — it detaches itself — and retry
//!   the connection for up to [`CONNECT_RETRY`].
//! - [`login_env`]: one `$SHELL -lc` run (at most [`LOGIN_PROBE_TIMEOUT`])
//!   that reports the login PATH and where `claude` / `codex` resolve.
//!   `berthd` execs `claude --resume <id>` by argv with its own PATH, and a
//!   GUI started from Finder or launchd has a very short one, so the daemon
//!   is launched with the login shell's PATH.
//! - [`Client`]: a handshaken connection. The first frame is always
//!   `Hello { role }`; afterwards a reader thread turns frames into
//!   [`ClientEvent`]s for a sink (the GUI's `EventLoopProxy`) and a writer
//!   thread drains a bounded channel of encoded frames. Request ids are
//!   allocated here; the controller matches `reply_to` against what it asked.
//! - [`SyncClient`]: blocking request / reply for the CLI subcommands.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use berth_core::{
    decode_payload, encode_frame, ClientMsg, ClientRole, DaemonMsg, Event, FrameReader, Paths,
    Request, PROTOCOL_VERSION,
};
use crossbeam_channel::{RecvTimeoutError, Sender, TrySendError};
use parking_lot::Mutex;

/// How long to keep retrying the socket after launching `berthd`.
pub const CONNECT_RETRY: Duration = Duration::from_secs(2);
/// Longest wait for the daemon's answer to `Hello`.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait for the login shell probe.
pub const LOGIN_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Encoded requests waiting for the writer thread. Beyond this the daemon
/// is not reading and `send` fails instead of buffering without bound.
const WRITE_QUEUE: usize = 4096;
const READ_BUF: usize = 64 * 1024;
/// Unsolicited messages a `SyncClient` keeps while waiting for a reply.
const SYNC_BACKLOG: usize = 1024;

/// What the reader thread reports.
#[derive(Debug)]
pub enum ClientEvent {
    Msg(Box<DaemonMsg>),
    /// The connection ended (EOF, I/O error, broken framing). Always last.
    Closed(String),
}

/// A handshaken GUI / CLI connection with reader and writer threads.
pub struct Client {
    stream: UnixStream,
    tx: Sender<Vec<u8>>,
    next_id: u32,
    daemon_version: String,
}

impl Client {
    /// Send `Hello { role }` as the first frame, check the daemon's answer,
    /// then start the reader (→ `sink`) and writer threads.
    pub fn start(
        mut stream: UnixStream,
        role: ClientRole,
        mut sink: impl FnMut(ClientEvent) + Send + 'static,
    ) -> Result<Client> {
        let (daemon_version, frames) = handshake(&mut stream, role)?;
        let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(WRITE_QUEUE);
        let write_error = Arc::new(Mutex::new(None::<String>));
        let mut writer = stream
            .try_clone()
            .context("cloning the socket for the writer")?;
        let reader = stream
            .try_clone()
            .context("cloning the socket for the reader")?;
        let werr = write_error.clone();
        std::thread::Builder::new()
            .name("berth-client-writer".into())
            .spawn(move || {
                for frame in rx {
                    if let Err(e) = writer.write_all(&frame) {
                        *werr.lock() = Some(format!("writing to berthd failed: {e}"));
                        // Wakes the reader, which reports the error.
                        let _ = writer.shutdown(Shutdown::Both);
                        return;
                    }
                }
            })
            .context("starting the writer thread")?;
        std::thread::Builder::new()
            .name("berth-client-reader".into())
            .spawn(move || {
                let reason = read_loop(reader, frames, &mut sink);
                let reason = write_error.lock().take().unwrap_or(reason);
                sink(ClientEvent::Closed(reason));
            })
            .context("starting the reader thread")?;
        Ok(Client {
            stream,
            tx,
            next_id: 1,
            daemon_version,
        })
    }

    pub fn daemon_version(&self) -> &str {
        &self.daemon_version
    }

    /// Queue a request; returns its id (the `reply_to` of its answer).
    pub fn send(&mut self, req: Request) -> Result<u32> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let frame = encode_frame(&ClientMsg { id, req }).context("encoding a request")?;
        match self.tx.try_send(frame) {
            Ok(()) => Ok(id),
            Err(TrySendError::Full(_)) => {
                bail!("berthd is not reading requests ({WRITE_QUEUE} queued)")
            }
            Err(TrySendError::Disconnected(_)) => bail!("the connection to berthd is closed"),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Ends both threads: the reader sees EOF, the writer's channel closes.
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

fn read_loop(
    mut stream: UnixStream,
    mut frames: FrameReader,
    sink: &mut impl FnMut(ClientEvent),
) -> String {
    let mut buf = vec![0u8; READ_BUF];
    loop {
        loop {
            match frames.next_frame() {
                Ok(Some(payload)) => match decode_payload::<DaemonMsg>(&payload) {
                    Ok(msg) => sink(ClientEvent::Msg(Box::new(msg))),
                    Err(e) => {
                        tracing::warn!("undecodable message from berthd: {e}");
                        sink(ClientEvent::Msg(Box::new(DaemonMsg {
                            reply_to: None,
                            event: Event::Error {
                                message: format!(
                                    "berth could not decode a message from berthd ({e}); \
                                     are the GUI and the daemon the same version?"
                                ),
                            },
                        })));
                    }
                },
                Ok(None) => break,
                Err(e) => return format!("bad frame from berthd: {e}"),
            }
        }
        match stream.read(&mut buf) {
            Ok(0) => return "berthd closed the connection".into(),
            Ok(n) => frames.push(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return format!("reading from berthd failed: {e}"),
        }
    }
}

/// `Hello` first (integrate.md §6); returns the daemon version and any bytes
/// that arrived after its answer.
fn handshake(stream: &mut UnixStream, role: ClientRole) -> Result<(String, FrameReader)> {
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .context("setting the handshake timeout")?;
    let hello = ClientMsg {
        id: 0,
        req: Request::Hello {
            role,
            protocol: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    };
    stream
        .write_all(&encode_frame(&hello)?)
        .context("sending Hello to berthd")?;
    let mut frames = FrameReader::new();
    let mut buf = vec![0u8; 4096];
    let msg: DaemonMsg = loop {
        if let Some(payload) = frames.next_frame()? {
            break decode_payload(&payload).context("decoding the answer to Hello")?;
        }
        match stream.read(&mut buf) {
            Ok(0) => bail!("berthd closed the connection during the handshake"),
            Ok(n) => frames.push(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                bail!(
                    "berthd did not answer Hello within {} s",
                    HANDSHAKE_TIMEOUT.as_secs()
                )
            }
            Err(e) => return Err(e).context("reading the answer to Hello"),
        }
    };
    stream
        .set_read_timeout(None)
        .context("clearing the handshake timeout")?;
    match msg.event {
        Event::Hello {
            daemon_version,
            protocol,
        } if protocol == PROTOCOL_VERSION => Ok((daemon_version, frames)),
        Event::Hello { protocol, .. }
        | Event::Incompatible {
            daemon_protocol: protocol,
        } => bail!(
            "berthd speaks protocol {protocol}, this berth speaks {PROTOCOL_VERSION}; \
             restart the daemon with the matching version"
        ),
        Event::Error { message } => bail!("berthd refused the connection: {message}"),
        other => bail!("unexpected answer to Hello: {other:?}"),
    }
}

/// Blocking request / reply client (CLI subcommands, tests).
pub struct SyncClient {
    stream: UnixStream,
    frames: FrameReader,
    next_id: u32,
    backlog: VecDeque<DaemonMsg>,
    buf: Vec<u8>,
    daemon_version: String,
}

impl SyncClient {
    /// Connect to a running daemon (never launches one).
    pub fn connect(paths: &Paths, role: ClientRole) -> Result<SyncClient> {
        let stream = UnixStream::connect(&paths.socket)
            .with_context(|| format!("connecting to {}", paths.socket.display()))?;
        SyncClient::start(stream, role)
    }

    pub fn start(mut stream: UnixStream, role: ClientRole) -> Result<SyncClient> {
        let (daemon_version, frames) = handshake(&mut stream, role)?;
        Ok(SyncClient {
            stream,
            frames,
            next_id: 1,
            backlog: VecDeque::new(),
            buf: vec![0u8; READ_BUF],
            daemon_version,
        })
    }

    pub fn daemon_version(&self) -> &str {
        &self.daemon_version
    }

    pub fn send(&mut self, req: Request) -> Result<u32> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let frame = encode_frame(&ClientMsg { id, req }).context("encoding a request")?;
        self.stream.write_all(&frame).context("writing to berthd")?;
        Ok(id)
    }

    /// Send `req` and wait for its answer.
    pub fn request(&mut self, req: Request, timeout: Duration) -> Result<Event> {
        let id = self.send(req)?;
        let msg = self
            .wait_for(Instant::now() + timeout, |m| m.reply_to == Some(id))?
            .ok_or_else(|| anyhow!("berthd did not answer within {} s", timeout.as_secs()))?;
        Ok(msg.event)
    }

    /// First message (queued ones first) matching `pred` before `deadline`;
    /// `None` on timeout. Other messages are kept for later calls.
    pub fn wait_for(
        &mut self,
        deadline: Instant,
        mut pred: impl FnMut(&DaemonMsg) -> bool,
    ) -> Result<Option<DaemonMsg>> {
        if let Some(pos) = self.backlog.iter().position(&mut pred) {
            return Ok(self.backlog.remove(pos));
        }
        loop {
            match self.read_msg(deadline)? {
                Some(msg) if pred(&msg) => return Ok(Some(msg)),
                Some(msg) => {
                    if self.backlog.len() >= SYNC_BACKLOG {
                        self.backlog.pop_front();
                    }
                    self.backlog.push_back(msg);
                }
                None => return Ok(None),
            }
        }
    }

    fn read_msg(&mut self, deadline: Instant) -> Result<Option<DaemonMsg>> {
        loop {
            if let Some(payload) = self.frames.next_frame()? {
                return Ok(Some(
                    decode_payload(&payload).context("decoding a message from berthd")?,
                ));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.stream.set_read_timeout(Some(left))?;
            match self.stream.read(&mut self.buf) {
                Ok(0) => bail!("berthd closed the connection"),
                Ok(n) => self.frames.push(&self.buf[..n]),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(None)
                }
                Err(e) => return Err(e).context("reading from berthd"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Finding / launching the daemon
// ---------------------------------------------------------------------------

/// Connect to the daemon, calling `launch` first when nothing listens on
/// the socket. Returns the stream and whether `launch` ran.
pub fn connect_or_spawn(
    paths: &Paths,
    launch: &mut dyn FnMut() -> Result<()>,
) -> Result<(UnixStream, bool)> {
    match UnixStream::connect(&paths.socket) {
        Ok(stream) => return Ok((stream, false)),
        Err(e) if daemon_absent(&e) => {
            tracing::info!(socket = %paths.socket.display(), "berthd is not running ({e}); launching it");
        }
        Err(e) => {
            return Err(e).with_context(|| format!("connecting to {}", paths.socket.display()))
        }
    }
    launch()?;
    let deadline = Instant::now() + CONNECT_RETRY;
    loop {
        match UnixStream::connect(&paths.socket) {
            Ok(stream) => return Ok((stream, true)),
            Err(e) if Instant::now() >= deadline => {
                return Err(e).with_context(|| {
                    format!(
                        "berthd was launched but {} accepted no connection within {} s (log: {})",
                        paths.socket.display(),
                        CONNECT_RETRY.as_secs(),
                        paths.logs_dir.join("berthd.log").display()
                    )
                })
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// No daemon behind the socket path (as opposed to e.g. a permission error).
pub fn daemon_absent(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused)
}

/// Launch `berthd` in the background with the login shell's PATH. It
/// detaches itself (`setsid`, log file); a thread reaps it whenever it exits.
pub fn launch_berthd() -> Result<()> {
    let login = login_env();
    let exe = std::env::current_exe().ok();
    let berthd = find_berthd(
        exe.as_deref().and_then(Path::parent),
        std::env::var_os("PATH").as_deref(),
        login.path.as_deref().map(OsStr::new),
    )
    .ok_or_else(|| {
        anyhow!(
            "berthd not found next to {} or on PATH",
            exe.as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "berth".into())
        )
    })?;
    let mut child = daemon_command(&berthd, login.path.as_deref())
        .spawn()
        .with_context(|| format!("starting {}", berthd.display()))?;
    tracing::info!(
        berthd = %berthd.display(),
        pid = child.id(),
        path = if login.path.is_some() { "login shell" } else { "inherited" },
        "launched berthd"
    );
    std::thread::Builder::new()
        .name("berthd-reaper".into())
        .spawn(move || {
            if let Err(e) = child.wait() {
                tracing::warn!("waiting for berthd failed: {e}");
            }
        })
        .context("starting the berthd reaper thread")?;
    Ok(())
}

/// `berthd` next to the running executable, else on `path`, else on the
/// login shell's `login_path`.
pub fn find_berthd(
    exe_dir: Option<&Path>,
    path: Option<&OsStr>,
    login_path: Option<&OsStr>,
) -> Option<PathBuf> {
    fn dirs(p: Option<&OsStr>) -> impl Iterator<Item = PathBuf> + '_ {
        p.map(std::env::split_paths)
            .into_iter()
            .flatten()
            .filter(|d| d.is_absolute())
    }
    exe_dir
        .map(Path::to_path_buf)
        .into_iter()
        .chain(dirs(path))
        .chain(dirs(login_path))
        .map(|d| d.join("berthd"))
        .find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The command that starts the daemon: no `--foreground` (it detaches and
/// logs to `<data_dir>/logs/berthd.log`), no stdio, PATH from the login
/// shell when known. Everything else, including `BERTH_*`, is inherited.
pub fn daemon_command(berthd: &Path, path_env: Option<&str>) -> Command {
    let mut cmd = Command::new(berthd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(path) = path_env {
        cmd.env("PATH", path);
    }
    cmd
}

/// What a login shell reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginEnv {
    pub shell: PathBuf,
    /// The login shell's PATH; `None` when the probe failed.
    pub path: Option<String>,
    /// `command -v claude` / `command -v codex` (empty output → `None`).
    pub claude: Option<String>,
    pub codex: Option<String>,
    /// Why the probe failed (then callers keep their own PATH).
    pub error: Option<String>,
}

impl LoginEnv {
    /// Whether `found` (a `command -v` result) is an executable path that
    /// an argv exec can use (shell functions and aliases are not).
    pub fn is_exec_path(found: &Option<String>) -> bool {
        found
            .as_deref()
            .is_some_and(|p| Path::new(p).is_absolute() && is_executable(Path::new(p)))
    }
}

/// Probe the login shell once per process (`$SHELL`, else `/bin/zsh`).
pub fn login_env() -> &'static LoginEnv {
    static CACHE: OnceLock<LoginEnv> = OnceLock::new();
    CACHE.get_or_init(|| probe_login_env(&user_shell(), LOGIN_PROBE_TIMEOUT))
}

pub fn user_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"))
}

/// Printed between ASCII record separators so profile noise (greetings,
/// `motd`, …) cannot be mistaken for the answer.
const PROBE_SCRIPT: &str = concat!(
    r#"printf '\036berth-path=%s\036\n' "$PATH"; "#,
    r#"printf '\036berth-claude=%s\036\n' "$(command -v claude 2>/dev/null)"; "#,
    r#"printf '\036berth-codex=%s\036\n' "$(command -v codex 2>/dev/null)""#
);
const RS: char = '\u{1e}';

pub fn probe_login_env(shell: &Path, timeout: Duration) -> LoginEnv {
    let mut env = LoginEnv {
        shell: shell.to_path_buf(),
        ..LoginEnv::default()
    };
    match run_probe(shell, timeout) {
        Ok(out) => {
            let parsed = parse_login_probe(&out);
            match parsed.path.filter(|p| !p.is_empty()) {
                Some(path) => {
                    env.path = Some(path);
                    env.claude = parsed.claude;
                    env.codex = parsed.codex;
                }
                None => env.error = Some(format!("{} -lc printed no PATH", shell.display())),
            }
        }
        Err(e) => env.error = Some(format!("{e:#}")),
    }
    if let Some(err) = &env.error {
        tracing::warn!("login shell probe failed ({err}); berthd inherits this process's PATH");
    }
    env
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProbeOutput {
    pub path: Option<String>,
    pub claude: Option<String>,
    pub codex: Option<String>,
}

pub fn parse_login_probe(out: &str) -> ProbeOutput {
    let mut parsed = ProbeOutput::default();
    for seg in out.split(RS) {
        let value = |v: &str| Some(v.to_string()).filter(|v| !v.is_empty());
        if let Some(v) = seg.strip_prefix("berth-path=") {
            parsed.path = value(v);
        } else if let Some(v) = seg.strip_prefix("berth-claude=") {
            parsed.claude = value(v);
        } else if let Some(v) = seg.strip_prefix("berth-codex=") {
            parsed.codex = value(v);
        }
    }
    parsed
}

fn probe_complete(out: &[u8]) -> bool {
    let s = String::from_utf8_lossy(out);
    s.split("\u{1e}berth-codex=")
        .nth(1)
        .is_some_and(|rest| rest.contains(RS))
}

fn run_probe(shell: &Path, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let mut child = Command::new(shell)
        .args(["-l", "-c", PROBE_SCRIPT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {} -lc", shell.display()))?;
    let mut stdout = child.stdout.take().context("login shell stdout")?;
    // Read on a thread: a profile that leaves a background job holding the
    // pipe open must not block us past the deadline.
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    std::thread::Builder::new()
        .name("berth-login-probe".into())
        .spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .context("starting the login probe reader")?;
    let mut out = Vec::new();
    let finished = loop {
        if probe_complete(&out) {
            break true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break false;
        }
        match rx.recv_timeout(left) {
            Ok(chunk) => out.extend_from_slice(&chunk),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break probe_complete(&out),
        }
    };
    // Reap the shell, but never wait past the deadline (a slow `.zlogout`).
    let reap_until = deadline.max(Instant::now() + Duration::from_millis(200));
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < reap_until => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    if !finished {
        bail!(
            "{} -lc did not finish within {} s",
            shell.display(),
            timeout.as_secs_f32()
        );
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{write_script, TestDaemon};
    use berth_core::{Dims, SessionStatus};
    use std::os::unix::net::UnixListener;

    #[test]
    fn berthd_is_found_beside_the_executable_first_then_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let (exe_dir, on_path, on_login) = (
            dir.path().join("bin"),
            dir.path().join("path"),
            dir.path().join("login"),
        );
        for d in [&exe_dir, &on_path, &on_login] {
            std::fs::create_dir_all(d).unwrap();
        }
        let path_var = std::env::join_paths([Path::new("relative"), &on_path]).unwrap();
        let login_var = on_login.clone().into_os_string();
        let find = || find_berthd(Some(&exe_dir), Some(&path_var), Some(&login_var));
        assert_eq!(find(), None);
        write_script(&on_login.join("berthd"), "exit 0");
        assert_eq!(find(), Some(on_login.join("berthd")));
        write_script(&on_path.join("berthd"), "exit 0");
        assert_eq!(find(), Some(on_path.join("berthd")));
        // Not executable: skipped.
        std::fs::write(exe_dir.join("berthd"), "x").unwrap();
        assert_eq!(find(), Some(on_path.join("berthd")));
        write_script(&exe_dir.join("berthd"), "exit 0");
        assert_eq!(find(), Some(exe_dir.join("berthd")));
    }

    #[test]
    fn probe_output_is_parsed_between_separators_despite_noise() {
        let out = "Welcome!\nberth-path=/fake\n\u{1e}berth-path=/a/bin:/usr/bin\u{1e}\n\
                   motd\n\u{1e}berth-claude=/a/bin/claude\u{1e}\n\u{1e}berth-codex=\u{1e}\n";
        assert_eq!(
            parse_login_probe(out),
            ProbeOutput {
                path: Some("/a/bin:/usr/bin".into()),
                claude: Some("/a/bin/claude".into()),
                codex: None,
            }
        );
        assert!(probe_complete(out.as_bytes()));
        assert!(!probe_complete(b"\x1eberth-codex=/x"));
    }

    #[test]
    fn probe_script_runs_in_a_real_posix_shell() {
        let dir = tempfile::tempdir().unwrap();
        // Stand-in for `$SHELL`: drop `-l` (no user profile in tests) and run
        // the real probe script with /bin/sh.
        let shell = dir.path().join("fake-login-shell");
        write_script(
            &shell,
            "echo 'profile noise'\nshift\nPATH=/usr/bin:/bin exec /bin/sh \"$@\"",
        );
        let env = probe_login_env(&shell, Duration::from_secs(5));
        assert_eq!(env.error, None);
        assert_eq!(env.path.as_deref(), Some("/usr/bin:/bin"));
        // No claude / codex in /usr/bin:/bin.
        assert_eq!((env.claude, env.codex), (None, None));
    }

    #[test]
    fn a_hanging_login_shell_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let shell = dir.path().join("slow-shell");
        write_script(&shell, "exec sleep 30");
        let t0 = Instant::now();
        let env = probe_login_env(&shell, Duration::from_millis(300));
        assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
        assert!(env.path.is_none());
        assert!(
            env.error.as_deref().unwrap().contains("did not finish"),
            "{env:?}"
        );
    }

    #[test]
    fn daemon_command_passes_path_and_detaches_stdio() {
        let cmd = daemon_command(Path::new("/x/berthd"), Some("/login/bin:/usr/bin"));
        assert_eq!(cmd.get_program(), "/x/berthd");
        assert_eq!(cmd.get_args().count(), 0, "no --foreground");
        let path: Vec<_> = cmd
            .get_envs()
            .filter(|(k, _)| *k == "PATH")
            .map(|(_, v)| v.map(|v| v.to_owned()))
            .collect();
        assert_eq!(path, vec![Some("/login/bin:/usr/bin".into())]);
        let inherited = daemon_command(Path::new("/x/berthd"), None);
        assert_eq!(inherited.get_envs().count(), 0);
    }

    /// A fake daemon that checks the first frame and answers with `reply`.
    fn fake_daemon(reply: Event) -> (tempfile::TempDir, Paths, std::thread::JoinHandle<ClientMsg>) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(dir.path());
        let listener = UnixListener::bind(&paths.socket).unwrap();
        let join = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut frames = FrameReader::new();
            let mut buf = [0u8; 4096];
            let first: ClientMsg = loop {
                if let Some(p) = frames.next_frame().unwrap() {
                    break decode_payload(&p).unwrap();
                }
                let n = s.read(&mut buf).unwrap();
                assert!(n > 0, "client closed before Hello");
                frames.push(&buf[..n]);
            };
            let answer = DaemonMsg {
                reply_to: Some(first.id),
                event: reply,
            };
            s.write_all(&encode_frame(&answer).unwrap()).unwrap();
            // Then one unsolicited event and echo of the next request id.
            let bell = DaemonMsg {
                reply_to: None,
                event: Event::Bell {
                    session: berth_core::SessionId::nil(),
                },
            };
            let _ = s.write_all(&encode_frame(&bell).unwrap());
            let next: Option<ClientMsg> = loop {
                if let Some(p) = frames.next_frame().unwrap() {
                    break Some(decode_payload(&p).unwrap());
                }
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => frames.push(&buf[..n]),
                }
            };
            if let Some(next) = next {
                let ok = DaemonMsg {
                    reply_to: Some(next.id),
                    event: Event::Ok,
                };
                let _ = s.write_all(&encode_frame(&ok).unwrap());
            }
            first
        });
        (dir, paths, join)
    }

    #[test]
    fn hello_is_the_first_frame_and_replies_flow_to_the_sink() {
        let (_dir, paths, join) = fake_daemon(Event::Hello {
            daemon_version: "9.9.9".into(),
            protocol: PROTOCOL_VERSION,
        });
        let stream = UnixStream::connect(&paths.socket).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut client = Client::start(stream, ClientRole::Gui, move |ev| {
            let _ = tx.send(ev);
        })
        .unwrap();
        assert_eq!(client.daemon_version(), "9.9.9");
        let id = client.send(Request::ListSessions).unwrap();
        let first = join.join().unwrap();
        assert!(matches!(
            first.req,
            Request::Hello {
                role: ClientRole::Gui,
                protocol: PROTOCOL_VERSION,
                ..
            }
        ));
        let mut got = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                ClientEvent::Msg(m) => got.push(*m),
                ClientEvent::Closed(reason) => {
                    assert!(reason.contains("closed"), "{reason}");
                    break;
                }
            }
        }
        assert!(matches!(got[0].event, Event::Bell { .. }));
        assert_eq!(got[1].reply_to, Some(id));
        assert_eq!(got[1].event, Event::Ok);
    }

    #[test]
    fn an_incompatible_daemon_is_reported() {
        let (_dir, paths, _join) = fake_daemon(Event::Incompatible {
            daemon_protocol: PROTOCOL_VERSION + 1,
        });
        let stream = UnixStream::connect(&paths.socket).unwrap();
        let err = SyncClient::start(stream, ClientRole::Cli)
            .err()
            .expect("handshake must fail");
        assert!(err.to_string().contains("protocol"), "{err:#}");
    }

    #[test]
    fn launch_runs_only_when_nothing_listens_and_the_retry_connects() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(dir.path());
        let mut daemon: Option<TestDaemon> = None;
        let mut launches = 0;
        let (stream, spawned) = connect_or_spawn(&paths, &mut || {
            launches += 1;
            daemon = Some(TestDaemon::start_at(paths.clone()));
            Ok(())
        })
        .unwrap();
        assert!(spawned);
        assert_eq!(launches, 1);
        let mut c = SyncClient::start(stream, ClientRole::Cli).unwrap();
        assert!(matches!(
            c.request(Request::DaemonStatus, Duration::from_secs(5))
                .unwrap(),
            Event::Status(_)
        ));
        // Already running: no second launch.
        let (_s, spawned) =
            connect_or_spawn(&paths, &mut || panic!("must not launch a second daemon")).unwrap();
        assert!(!spawned);
        drop(daemon);
    }

    #[test]
    fn a_launcher_that_never_starts_a_daemon_fails_after_the_retry_window() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(dir.path());
        let t0 = Instant::now();
        let err = connect_or_spawn(&paths, &mut || Ok(())).unwrap_err();
        assert!(t0.elapsed() >= CONNECT_RETRY);
        assert!(
            format!("{err:#}").contains("accepted no connection"),
            "{err:#}"
        );
    }

    #[test]
    fn real_daemon_roundtrip_with_the_threaded_client() {
        let daemon = TestDaemon::start();
        let stream = UnixStream::connect(&daemon.paths.socket).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut client = Client::start(stream, ClientRole::Gui, move |ev| {
            let _ = tx.send(ev);
        })
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let ws = client
            .send(Request::CreateWorkspace {
                name: "t".into(),
                root: root.path().to_path_buf(),
            })
            .unwrap();
        let reply = |rx: &crossbeam_channel::Receiver<ClientEvent>, id: u32| loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                ClientEvent::Msg(m) if m.reply_to == Some(id) => return m.event,
                ClientEvent::Msg(_) => {}
                ClientEvent::Closed(r) => panic!("closed: {r}"),
            }
        };
        let Event::WorkspaceUpdated(ws) = reply(&rx, ws) else {
            panic!("CreateWorkspace must answer WorkspaceUpdated")
        };
        let create = client
            .send(Request::CreateSession {
                workspace: ws.id,
                cwd: None,
                command: Some(vec!["/bin/cat".into()]),
                title: None,
                dims: Dims { cols: 80, rows: 24 },
            })
            .unwrap();
        let Event::SessionUpdated(meta) = reply(&rx, create) else {
            panic!("CreateSession must answer SessionUpdated")
        };
        assert_eq!(meta.status, SessionStatus::Live);
        // Input is fire-and-forget; errors come back as Error.
        let bogus = client
            .send(Request::Input {
                session: berth_core::SessionId::new(),
                data: b"x".to_vec(),
            })
            .unwrap();
        assert!(matches!(reply(&rx, bogus), Event::Error { .. }));
        drop(client);
        // The reader reports the end of the connection.
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                ClientEvent::Closed(_) => break,
                ClientEvent::Msg(_) => {}
            }
        }
        drop(daemon);
    }
}
