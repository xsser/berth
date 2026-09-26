//! PTY ownership: spawn the child, pump output on a reader thread, write
//! input, resize, inspect the foreground process.
//!
//! This is the only module in the crate allowed to use `unsafe` (libc FFI);
//! every block needs a `// SAFETY:` comment.
//!
//! Process model: portable-pty runs the child through `setsid()` +
//! `TIOCSCTTY`, so the child leads its own session and process group
//! (pgid == pid) with the PTY as controlling terminal. Signals therefore go to
//! that group, plus the PTY's foreground group (a job started by an
//! interactive shell lives in a group of its own).
#![allow(unsafe_code)]

use std::fmt;
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::VtError;

/// Size of one reader-thread `read`, i.e. the largest `PtyOutput::Data`.
pub const READ_CHUNK_SIZE: usize = 64 * 1024;

/// Grace period between SIGHUP and SIGKILL: used when a `PtyHandle` is
/// dropped while its child still runs, and recommended between
/// [`PtyHandle::kill`] and [`PtyHandle::force_kill`].
pub const KILL_GRACE: Duration = Duration::from_secs(1);

/// Variables describing *another* terminal or multiplexer, possibly inherited
/// from whatever launched the daemon. A berth session is none of those, and
/// `TERMINFO` would shadow the `xterm-256color` entry we advertise.
const FOREIGN_TERMINAL_ENV: &[&str] = &[
    "TMUX",
    "TMUX_PANE",
    "STY",
    "TERMINFO",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "COLORFGBG",
    "VTE_VERSION",
    "WINDOWID",
    "COLUMNS",
    "LINES",
    "ITERM_SESSION_ID",
    "ITERM_PROFILE",
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "KITTY_PUBLIC_KEY",
    "KITTY_LISTEN_ON",
    "KITTY_INSTALLATION_DIR",
    "WEZTERM_PANE",
    "WEZTERM_UNIX_SOCKET",
    "WEZTERM_EXECUTABLE",
    "WEZTERM_EXECUTABLE_DIR",
    "WEZTERM_CONFIG_DIR",
    "WEZTERM_CONFIG_FILE",
    "ALACRITTY_WINDOW_ID",
    "ALACRITTY_SOCKET",
    "ALACRITTY_LOG",
    "GHOSTTY_RESOURCES_DIR",
    "GHOSTTY_BIN_DIR",
    "GHOSTTY_SHELL_FEATURES",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtySpawn {
    /// argv; empty means "the user's login shell" (`$SHELL`, `-l`).
    pub command: Vec<String>,
    pub cwd: PathBuf,
    /// Extra environment on top of the daemon's environment. The daemon
    /// always sets `BERTH_SESSION_ID`, `BERTH_SOCKET`, `TERM`, `COLORTERM`.
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

/// Messages from the reader thread.
#[derive(Debug)]
pub enum PtyOutput {
    Data(Vec<u8>),
    /// Reader hit EOF; `try_wait` yields the exit code.
    Eof,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    /// Executable base name (e.g. `claude`, `node`, `zsh`).
    pub name: String,
    pub cwd: Option<PathBuf>,
}

pub struct PtyHandle {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    /// `None` only while `Drop` hands the child to the reaper thread.
    child: Option<Box<dyn Child + Send + Sync>>,
    pid: u32,
    /// Cached once reaped: a reaped pid may be reused, so it is never
    /// signalled or waited on again.
    exit_code: Option<i32>,
}

impl fmt::Debug for PtyHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyHandle")
            .field("pid", &self.pid)
            .field("exit_code", &self.exit_code)
            .finish_non_exhaustive()
    }
}

impl PtyHandle {
    /// Spawn the child on a new PTY and start the reader thread. Reads are
    /// delivered as `PtyOutput::Data` chunks (up to 64 KiB each), followed by
    /// exactly one `PtyOutput::Eof` once every slave fd is closed.
    ///
    /// The channel is unbounded on purpose: the consumer may block in
    /// [`PtyHandle::write`] on the same thread that drains output, and a
    /// bounded channel could then deadlock against a child that is itself
    /// blocked writing output. `Terminal::process` outpaces PTY throughput,
    /// so the queue stays short as long as the consumer keeps draining.
    ///
    /// Environment: the daemon's environment minus variables that identify
    /// another terminal (tmux, iTerm2, kitty, Ghostty, ...), then
    /// `TERM=xterm-256color`, `COLORTERM=truecolor`, `PWD=<cwd>`, then
    /// `spec.env` (which wins). A missing `cwd` falls back to `$HOME`.
    ///
    /// The child may still be between `fork` and `exec` when this returns
    /// (see [`PtyHandle::foreground_process`]); an `exec` failure therefore
    /// surfaces as an immediate exit rather than as an error here.
    pub fn spawn(spec: &PtySpawn) -> Result<(PtyHandle, Receiver<PtyOutput>), VtError> {
        let pair = native_pty_system()
            .openpty(pty_size(spec.cols, spec.rows))
            .map_err(pty_error("openpty"))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(pty_error("clone pty reader"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(pty_error("take pty writer"))?;
        let mut child = pair
            .slave
            .spawn_command(build_command(spec))
            .map_err(pty_error("spawn"))?;
        // Close our copy of the slave side, otherwise the reader would never
        // see EOF when the child exits.
        drop(pair.slave);

        let Some(pid) = child.process_id().filter(|&pid| pid > 1) else {
            if let Err(err) = child.kill() {
                tracing::warn!(error = %err, "killing a child without pid failed");
            }
            return Err(VtError::Pty("spawned child has no usable pid".into()));
        };

        let (tx, rx) = crossbeam_channel::unbounded();
        let reader_thread = std::thread::Builder::new()
            .name(format!("berth-pty-{pid}"))
            .spawn(move || read_loop(reader, tx));
        let handle = PtyHandle {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Some(child),
            pid,
            exit_code: None,
        };
        if let Err(err) = reader_thread {
            // Dropping the handle hangs up and reaps the child.
            drop(handle);
            return Err(VtError::Io(err));
        }
        Ok((handle, rx))
    }

    /// Write input to the child (blocking until the PTY accepted all bytes).
    pub fn write(&self, bytes: &[u8]) -> Result<(), VtError> {
        let mut writer = self.writer.lock();
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Resize the PTY (`TIOCSWINSZ`; the child receives `SIGWINCH`).
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), VtError> {
        self.master
            .lock()
            .resize(pty_size(cols, rows))
            .map_err(pty_error("resize"))
    }

    pub fn child_pid(&self) -> u32 {
        self.pid
    }

    /// Non-blocking; `Some(code)` once the child exited. A child killed by a
    /// signal reports `128 + signal` (shell convention).
    pub fn try_wait(&mut self) -> Result<Option<i32>, VtError> {
        if let Some(code) = self.exit_code {
            return Ok(Some(code));
        }
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let code = poll_child(child.as_mut())?;
        self.exit_code = code;
        Ok(code)
    }

    /// SIGHUP the child's process group (and the PTY's foreground group, if
    /// different). Does not wait: poll [`PtyHandle::try_wait`] and escalate
    /// with [`PtyHandle::force_kill`] after [`KILL_GRACE`] if needed.
    pub fn kill(&mut self) -> Result<(), VtError> {
        self.signal(libc::SIGHUP)
    }

    /// SIGKILL the same process groups as [`PtyHandle::kill`].
    pub fn force_kill(&mut self) -> Result<(), VtError> {
        self.signal(libc::SIGKILL)
    }

    /// Foreground process group leader of the PTY (`tcgetpgrp` on the master
    /// fd, then `proc_pidinfo` / `/proc`), used for agent-kind heuristics.
    ///
    /// `None` when there is no foreground group, its leader has exited, or it
    /// has not exec'd yet: portable-pty's `pre_exec` closes std's exec-status
    /// pipe, so [`PtyHandle::spawn`] can return while the child still runs
    /// the daemon's own image; that transient state is not reported.
    pub fn foreground_process(&self) -> Option<ProcessInfo> {
        let pgrp = self.foreground_pgrp()?;
        let (info, exe) = procinfo::inspect(u32::try_from(pgrp).ok()?)?;
        if exe.as_deref().is_some_and(is_own_executable) {
            return None;
        }
        Some(info)
    }

    fn foreground_pgrp(&self) -> Option<libc::pid_t> {
        // portable-pty implements this as `tcgetpgrp(master_fd)`.
        self.master.lock().process_group_leader()
    }

    fn signal(&mut self, signal: libc::c_int) -> Result<(), VtError> {
        if self.try_wait()?.is_some() {
            // Already reaped: the pid may belong to someone else by now.
            return Ok(());
        }
        signal_session(self.pid, self.foreground_pgrp(), signal)
    }
}

impl Drop for PtyHandle {
    /// A still-running child is hung up and reaped on a background thread
    /// (SIGKILL after [`KILL_GRACE`]) so the daemon never accumulates zombies.
    fn drop(&mut self) {
        match self.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(pid = self.pid, error = %err, "try_wait failed while dropping PtyHandle");
                return;
            }
        }
        let foreground = self.foreground_pgrp();
        if let Err(err) = signal_session(self.pid, foreground, libc::SIGHUP) {
            tracing::warn!(pid = self.pid, error = %err, "SIGHUP on drop failed");
        }
        let Some(child) = self.child.take() else {
            return;
        };
        let pid = self.pid;
        let reaper = std::thread::Builder::new()
            .name(format!("berth-reap-{pid}"))
            .spawn(move || reap(child, pid, foreground));
        if let Err(err) = reaper {
            tracing::warn!(pid, error = %err, "cannot spawn reaper; child may linger as a zombie");
        }
    }
}

fn read_loop(mut reader: Box<dyn Read + Send>, tx: Sender<PtyOutput>) {
    let mut buf = vec![0u8; READ_CHUNK_SIZE];
    loop {
        match reader.read(&mut buf) {
            // portable-pty maps EIO (slave closed) to Ok(0).
            Ok(0) => break,
            Ok(n) => {
                if tx.send(PtyOutput::Data(buf[..n].to_vec())).is_err() {
                    // Receiver dropped: nobody is interested any more.
                    return;
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => {
                tracing::warn!(error = %err, "pty read failed; treating as EOF");
                break;
            }
        }
    }
    if tx.send(PtyOutput::Eof).is_err() {
        tracing::debug!("pty EOF not delivered: receiver dropped");
    }
}

fn reap(mut child: Box<dyn Child + Send + Sync>, pid: u32, foreground: Option<libc::pid_t>) {
    let deadline = Instant::now() + KILL_GRACE;
    loop {
        match poll_child(child.as_mut()) {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                if let Err(err) = signal_session(pid, foreground, libc::SIGKILL) {
                    tracing::warn!(pid, error = %err, "SIGKILL while reaping failed");
                }
                if let Err(err) = child.wait() {
                    tracing::warn!(pid, error = %err, "waiting for killed child failed");
                }
                return;
            }
            Err(err) => {
                tracing::warn!(pid, error = %err, "polling child while reaping failed");
                return;
            }
        }
    }
}

/// Exit code of `child` if it has exited (reaping it).
fn poll_child(child: &mut (dyn Child + Send + Sync)) -> io::Result<Option<i32>> {
    let child: &mut dyn Child = child;
    // portable-pty's unix children are `std::process::Child`; going through
    // std keeps the signal number (portable-pty only keeps its name).
    if let Some(process) = child.downcast_mut::<std::process::Child>() {
        return Ok(std::process::Child::try_wait(process)?.map(crate::exit_status_code));
    }
    Ok(child
        .try_wait()?
        .map(|status| i32::try_from(status.exit_code()).unwrap_or(i32::MAX)))
}

/// Signal the child's process group and, if different, the PTY's foreground
/// process group. Groups that no longer exist are not an error.
fn signal_session(
    pid: u32,
    foreground: Option<libc::pid_t>,
    signal: libc::c_int,
) -> Result<(), VtError> {
    let pgid = libc::pid_t::try_from(pid)
        .map_err(|_| VtError::Pty(format!("pid {pid} does not fit pid_t")))?;
    let result = kill_group(pgid, signal);
    if let Some(foreground) = foreground.filter(|&fg| fg != pgid) {
        if let Err(err) = kill_group(foreground, signal) {
            tracing::warn!(pgid = foreground, signal, error = %err, "signalling the foreground group failed");
        }
    }
    result.map_err(VtError::Io)
}

/// `killpg(2)`, treating `ESRCH` (group already gone) as success.
fn kill_group(pgid: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    if pgid <= 1 {
        // 0 would mean "our own group"; 1 is launchd/init.
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("refusing to signal process group {pgid}"),
        ));
    }
    // SAFETY: killpg takes plain integers and has no memory-safety
    // preconditions; it only delivers a signal. `pgid > 1` excludes the
    // daemon's own group (0) and init (1), and callers only pass the child's
    // own group or the PTY's foreground group, both inside the child's session.
    let rc = unsafe { libc::killpg(pgid, signal) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(err)
    }
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn pty_error<E: fmt::Display>(context: &'static str) -> impl FnOnce(E) -> VtError {
    move |err| VtError::Pty(format!("{context}: {err:#}"))
}

fn build_command(spec: &PtySpawn) -> CommandBuilder {
    let mut cmd = match spec.command.split_first() {
        Some((program, args)) => {
            let mut cmd = CommandBuilder::new(program);
            cmd.args(args);
            cmd
        }
        None => {
            let mut cmd = CommandBuilder::new(login_shell(&spec.env));
            cmd.arg("-l");
            cmd
        }
    };
    cmd.cwd(&spec.cwd);
    configure_env(&mut cmd, spec);
    cmd
}

fn configure_env(cmd: &mut CommandBuilder, spec: &PtySpawn) {
    for key in FOREIGN_TERMINAL_ENV {
        cmd.env_remove(key);
    }
    cmd.env_remove("OLDPWD");
    if spec.cwd.is_absolute() && spec.cwd.is_dir() {
        cmd.env("PWD", &spec.cwd);
    } else {
        // portable-pty falls back to $HOME; let the shell derive PWD.
        cmd.env_remove("PWD");
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
}

/// `SHELL` from the spawn env, else the daemon's `$SHELL`, else `/bin/zsh`,
/// else `/bin/sh`: the first that is an absolute path to an executable.
fn login_shell(extra_env: &[(String, String)]) -> PathBuf {
    let from_spec = extra_env
        .iter()
        .rev()
        .find(|(key, _)| key == "SHELL")
        .map(|(_, value)| PathBuf::from(value));
    let from_daemon = std::env::var_os("SHELL").map(PathBuf::from);
    from_spec
        .into_iter()
        .chain(from_daemon)
        .chain(["/bin/zsh", "/bin/sh"].map(PathBuf::from))
        .find(|path| path.is_absolute() && is_executable(path))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Whether `exe` is this process's own executable (a forked child that has
/// not exec'd yet still shows it).
fn is_own_executable(exe: &Path) -> bool {
    static OWN: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    let Some(own) = OWN.get_or_init(|| std::env::current_exe().ok()?.canonicalize().ok()) else {
        return false;
    };
    exe.file_name() == own.file_name() && exe.canonicalize().is_ok_and(|exe| exe == *own)
}

#[cfg(target_os = "macos")]
mod procinfo {
    use std::ffi::{OsStr, OsString};
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    use libc::c_int;

    use super::ProcessInfo;

    /// Process info plus the full executable path when known.
    pub(super) fn inspect(pid: u32) -> Option<(ProcessInfo, Option<PathBuf>)> {
        let raw = c_int::try_from(pid).ok().filter(|&pid| pid > 0)?;
        let exe = executable_path(raw);
        // The executable's base name is not truncated, unlike `p_comm`.
        let name = exe
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .or_else(|| short_name(raw))?;
        Some((
            ProcessInfo {
                pid,
                name,
                cwd: cwd(raw),
            },
            exe,
        ))
    }

    fn executable_path(pid: c_int) -> Option<PathBuf> {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let capacity = u32::try_from(buf.len()).ok()?;
        // SAFETY: `buf` is a live, writable allocation of `capacity` bytes;
        // proc_pidpath writes at most that many bytes and returns the number
        // written (<= 0 on failure).
        let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), capacity) };
        let len = usize::try_from(len)
            .ok()
            .filter(|&len| len > 0 && len <= buf.len())?;
        Some(PathBuf::from(OsStr::from_bytes(&buf[..len])))
    }

    /// The kernel's process name (`p_name`/`p_comm`, at most 32 bytes).
    fn short_name(pid: c_int) -> Option<String> {
        let mut buf = [0u8; 64];
        let capacity = u32::try_from(buf.len()).ok()?;
        // SAFETY: `buf` is a writable stack array of `capacity` bytes;
        // proc_name writes at most that many bytes and returns the length.
        let len = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), capacity) };
        let len = usize::try_from(len)
            .ok()
            .filter(|&len| len > 0 && len <= buf.len())?;
        Some(String::from_utf8_lossy(&buf[..len]).into_owned())
    }

    fn cwd(pid: c_int) -> Option<PathBuf> {
        let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let size = c_int::try_from(std::mem::size_of::<libc::proc_vnodepathinfo>()).ok()?;
        // SAFETY: `info` is writable memory of exactly `size` bytes;
        // proc_pidinfo(PROC_PIDVNODEPATHINFO) fills at most `size` bytes and
        // returns the number of bytes written.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written != size {
            return None;
        }
        // SAFETY: proc_vnodepathinfo consists solely of integers and C char
        // arrays, for which the all-zero bit pattern (from `zeroed`) and any
        // bytes the kernel wrote are valid values.
        let info = unsafe { info.assume_init() };
        let bytes: Vec<u8> = info
            .pvi_cdir
            .vip_path
            .iter()
            .flatten()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        (!bytes.is_empty()).then(|| PathBuf::from(OsString::from_vec(bytes)))
    }
}

#[cfg(target_os = "linux")]
mod procinfo {
    use std::path::PathBuf;

    use super::ProcessInfo;

    /// Process info plus the full executable path when known.
    pub(super) fn inspect(pid: u32) -> Option<(ProcessInfo, Option<PathBuf>)> {
        let dir = PathBuf::from(format!("/proc/{pid}"));
        let exe = std::fs::read_link(dir.join("exe")).ok();
        // `comm` is truncated to 15 bytes, so prefer the executable's name.
        let name = exe
            .as_deref()
            .and_then(|exe| exe.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .or_else(|| {
                let comm = std::fs::read_to_string(dir.join("comm")).ok()?;
                Some(comm.trim_end_matches('\n').to_owned())
            })?;
        let cwd = std::fs::read_link(dir.join("cwd")).ok();
        Some((ProcessInfo { pid, name, cwd }, exe))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod procinfo {
    use super::ProcessInfo;

    pub(super) fn inspect(_pid: u32) -> Option<(ProcessInfo, Option<std::path::PathBuf>)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &[&str], env: &[(&str, &str)]) -> PtySpawn {
        PtySpawn {
            command: command.iter().map(|s| s.to_string()).collect(),
            cwd: PathBuf::from("/"),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            cols: 80,
            rows: 24,
        }
    }

    fn argv(cmd: &CommandBuilder) -> Vec<String> {
        cmd.get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn empty_command_runs_login_shell() {
        let cmd = build_command(&spec(&[], &[("SHELL", "/bin/sh")]));
        assert_eq!(argv(&cmd), ["/bin/sh", "-l"]);
    }

    #[test]
    fn explicit_command_keeps_argv() {
        let cmd = build_command(&spec(&["/bin/echo", "a b", "c"], &[]));
        assert_eq!(argv(&cmd), ["/bin/echo", "a b", "c"]);
    }

    #[test]
    fn login_shell_skips_unusable_candidates() {
        let shell = login_shell(&[("SHELL".into(), "/definitely/not/a/shell".into())]);
        assert!(shell.is_absolute() && is_executable(&shell), "{shell:?}");
    }

    #[test]
    fn environment_is_prepared() {
        let mut cmd = CommandBuilder::new("/bin/true");
        // Pretend the daemon inherited these from another terminal.
        cmd.env("TMUX", "/tmp/tmux-501/default,1,0");
        cmd.env("TERM_PROGRAM", "ghostty");
        cmd.env(
            "TERMINFO",
            "/Applications/Ghostty.app/Contents/Resources/terminfo",
        );
        cmd.env("OLDPWD", "/somewhere");
        cmd.env("HOME", "/Users/test");
        let spec = spec(
            &["/bin/true"],
            &[("BERTH_SESSION_ID", "abc"), ("COLORTERM", "24bit")],
        );
        configure_env(&mut cmd, &spec);
        let get = |key: &str| cmd.get_env(key).map(|v| v.to_string_lossy().into_owned());
        assert_eq!(get("TMUX"), None);
        assert_eq!(get("TERM_PROGRAM"), None);
        assert_eq!(get("TERMINFO"), None);
        assert_eq!(get("OLDPWD"), None);
        assert_eq!(get("HOME").as_deref(), Some("/Users/test"));
        assert_eq!(get("PWD").as_deref(), Some("/"));
        assert_eq!(get("TERM").as_deref(), Some("xterm-256color"));
        // spec.env is applied last and wins.
        assert_eq!(get("COLORTERM").as_deref(), Some("24bit"));
        assert_eq!(get("BERTH_SESSION_ID").as_deref(), Some("abc"));
    }

    #[test]
    fn refuses_to_signal_special_groups() {
        assert!(kill_group(0, libc::SIGHUP).is_err());
        assert!(kill_group(1, libc::SIGHUP).is_err());
        assert!(signal_session(1, None, libc::SIGHUP).is_err());
    }

    #[test]
    fn pty_handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PtyHandle>();
    }
}
