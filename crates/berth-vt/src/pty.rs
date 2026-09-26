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
            // Reap it too, so no zombie is left behind.
            if let Err(err) = child.wait() {
                tracing::warn!(error = %err, "reaping a child without pid failed");
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

    /// Foreground process of the PTY, used for agent-kind heuristics:
    /// `tcgetpgrp` on the master fd names the foreground process group and
    /// `proc_pidinfo` / `/proc` describe its leader. If the leader already
    /// exited while other members still run (`a | b` after `a` finished),
    /// the lowest-pid member that can still be inspected is reported.
    ///
    /// `None` when there is no foreground group, none of its members can be
    /// inspected, or the process has not exec'd yet: portable-pty's
    /// `pre_exec` closes std's exec-status pipe, so [`PtyHandle::spawn`] can
    /// return while the child still runs the daemon's own image; that
    /// transient state is not reported.
    pub fn foreground_process(&self) -> Option<ProcessInfo> {
        let pgrp = u32::try_from(self.foreground_pgrp()?).ok()?;
        let (info, exe) = procinfo::inspect(pgrp).or_else(|| {
            procinfo::group_members(pgrp)
                .into_iter()
                .filter(|&pid| pid != pgrp)
                .find_map(procinfo::inspect)
        })?;
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
    /// A child that still runs, or whose state cannot be read, is hung up and
    /// reaped on a background thread (SIGKILL after [`KILL_GRACE`]) so the
    /// daemon never leaves orphans or zombies behind.
    fn drop(&mut self) {
        match self.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            // Unknown state: clean up as if it still ran (groups that are
            // already gone are not an error).
            Err(err) => {
                tracing::warn!(pid = self.pid, error = %err, "try_wait failed while dropping PtyHandle; hanging up anyway");
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

/// Give a hung-up child [`KILL_GRACE`] to exit, then SIGKILL its groups and
/// wait for it. A child whose state cannot be polled is treated as still
/// running: same grace period, same escalation; a failing final `wait` is
/// only logged.
fn reap(mut child: Box<dyn Child + Send + Sync>, pid: u32, foreground: Option<libc::pid_t>) {
    let deadline = Instant::now() + KILL_GRACE;
    let mut poll_failed = false;
    loop {
        match poll_child(child.as_mut()) {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(err) => {
                if !poll_failed {
                    tracing::warn!(pid, error = %err, "polling child while reaping failed; escalating after the grace period");
                }
                poll_failed = true;
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Err(err) = signal_session(pid, foreground, libc::SIGKILL) {
        tracing::warn!(pid, error = %err, "SIGKILL while reaping failed");
    }
    if let Err(err) = child.wait() {
        tracing::warn!(pid, error = %err, "waiting for killed child failed");
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

/// `(state, pgrp)` from the contents of `/proc/<pid>/stat`
/// (`pid (comm) state ppid pgrp ...`). `comm` may itself contain spaces and
/// parentheses, so fields are counted after the last `)`.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_stat(stat: &str) -> Option<(char, u32)> {
    let (_, fields) = stat.rsplit_once(')')?;
    let mut fields = fields.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse().ok()?;
    Some((state, pgrp))
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

    /// `PROC_PGRP_ONLY` from `<sys/proc_info.h>` (libc does not export it).
    const PROC_PGRP_ONLY: u32 = 2;

    /// Upper bound on the pids listed per process group; a job's group
    /// rarely has more than a handful of members.
    const MAX_GROUP_MEMBERS: usize = 256;

    /// Pids in process group `pgid`, ascending. The kernel also lists
    /// zombies that were not reaped yet.
    pub(super) fn group_members(pgid: u32) -> Vec<u32> {
        let mut pids: [c_int; MAX_GROUP_MEMBERS] = [0; MAX_GROUP_MEMBERS];
        let Ok(capacity) = c_int::try_from(std::mem::size_of_val(&pids)) else {
            return Vec::new();
        };
        // SAFETY: `pids` is a writable stack array of exactly `capacity`
        // bytes; proc_listpids copies out at most that many bytes of pids and
        // returns the number of bytes written (<= 0 on failure).
        let written = unsafe {
            libc::proc_listpids(PROC_PGRP_ONLY, pgid, pids.as_mut_ptr().cast(), capacity)
        };
        let count = usize::try_from(written).unwrap_or(0) / std::mem::size_of::<c_int>();
        let mut members: Vec<u32> = pids[..count.min(MAX_GROUP_MEMBERS)]
            .iter()
            .filter_map(|&pid| u32::try_from(pid).ok())
            .filter(|&pid| pid > 0)
            .collect();
        members.sort_unstable();
        members.dedup();
        members
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

    /// Live pids in process group `pgid`, ascending, from `/proc/*/stat`.
    pub(super) fn group_members(pgid: u32) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut members: Vec<u32> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .as_deref()
                    .and_then(super::parse_proc_stat)
                    .is_some_and(|(state, pgrp)| pgrp == pgid && !matches!(state, 'Z' | 'X'))
            })
            .collect();
        members.sort_unstable();
        members
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod procinfo {
    use super::ProcessInfo;

    pub(super) fn inspect(_pid: u32) -> Option<(ProcessInfo, Option<std::path::PathBuf>)> {
        None
    }

    pub(super) fn group_members(_pgid: u32) -> Vec<u32> {
        Vec::new()
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

    /// Poll `condition` every 20 ms until it holds or `timeout` passes.
    fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if condition() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Hang up the session and make sure it is gone before the test ends.
    fn shut_down(pty: &mut PtyHandle) {
        pty.kill().expect("SIGHUP");
        if !wait_for(Duration::from_secs(3), || {
            matches!(pty.try_wait(), Ok(Some(_)))
        }) {
            pty.force_kill().expect("SIGKILL");
            assert!(
                wait_for(Duration::from_secs(3), || matches!(
                    pty.try_wait(),
                    Ok(Some(_))
                )),
                "child survived SIGKILL"
            );
        }
    }

    #[test]
    fn foreground_process_reports_a_member_after_the_leader_exits() {
        let dir = tempfile::tempdir().expect("tempdir");
        // An interactive shell saves its history on exit: keep it out of the
        // user's home directory.
        let history = dir.path().join("sh_history");
        let history = history.to_string_lossy();
        let mut spec = spec(&["/bin/sh", "-i"], &[("HISTFILE", history.as_ref())]);
        spec.cwd = dir.path().to_path_buf();
        let (mut pty, _output) = PtyHandle::spawn(&spec).expect("spawn");
        pty.write(b"sleep 0.5 | sleep 5\n").expect("write");
        let runs_sleep = |pty: &PtyHandle| {
            pty.foreground_process()
                .is_some_and(|info| info.name.starts_with("sleep"))
        };
        assert!(
            wait_for(Duration::from_secs(5), || runs_sleep(&pty)),
            "the pipeline never became the foreground job"
        );
        // The job's process group is named after its first process. Once
        // `sleep 0.5` has exited and been reaped, only `sleep 5` keeps the
        // group, and with it the terminal, alive.
        let pgrp = pty
            .foreground_pgrp()
            .and_then(|pgrp| u32::try_from(pgrp).ok())
            .expect("foreground group");
        assert!(
            wait_for(Duration::from_secs(3), || procinfo::inspect(pgrp).is_none()),
            "the group leader never went away"
        );
        let info = pty.foreground_process();
        shut_down(&mut pty);
        let info = info.expect("a surviving member of the foreground group is reported");
        assert!(info.name.starts_with("sleep"), "{info:?}");
        assert_ne!(info.pid, pgrp);
    }

    /// Stands in for a child whose state can no longer be read (for example
    /// because something else reaped it): every poll and wait fails.
    #[derive(Debug)]
    struct UnreadableChild {
        pid: u32,
    }

    impl portable_pty::ChildKiller for UnreadableChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(UnreadableChild { pid: self.pid })
        }
    }

    impl Child for UnreadableChild {
        fn try_wait(&mut self) -> io::Result<Option<portable_pty::ExitStatus>> {
            Err(io::Error::from_raw_os_error(libc::ECHILD))
        }

        fn wait(&mut self) -> io::Result<portable_pty::ExitStatus> {
            Err(io::Error::from_raw_os_error(libc::ECHILD))
        }

        fn process_id(&self) -> Option<u32> {
            Some(self.pid)
        }
    }

    /// Exit status of `child` within `timeout`. On timeout its whole process
    /// group is killed so nothing outlives a failing test.
    fn status_within(
        child: &mut std::process::Child,
        timeout: Duration,
    ) -> Option<std::process::ExitStatus> {
        let mut status = None;
        wait_for(timeout, || {
            status = child.try_wait().expect("try_wait");
            status.is_some()
        });
        if status.is_none() {
            let pgid = libc::pid_t::try_from(child.id()).expect("pid fits pid_t");
            kill_group(pgid, libc::SIGKILL).expect("cleanup SIGKILL");
            child.wait().expect("cleanup wait");
        }
        status
    }

    #[test]
    fn dropping_with_unreadable_child_state_still_hangs_up_and_kills() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};

        let dir = tempfile::tempdir().expect("tempdir");
        let ready = dir.path().join("ready");
        let hung_up = dir.path().join("hung-up");
        // A process group of its own that records SIGHUP but keeps running,
        // so only the SIGKILL escalation ends it.
        let script = format!(
            r#"trap ': > "{hup}"' HUP; : > "{ready}"; while :; do /bin/sleep 0.1; done"#,
            hup = hung_up.display(),
            ready = ready.display(),
        );
        let mut victim = std::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .process_group(0)
            .spawn()
            .expect("spawn victim");
        assert!(
            wait_for(Duration::from_secs(5), || ready.exists()),
            "victim never installed its trap"
        );

        let pid = victim.id();
        let pair = native_pty_system()
            .openpty(pty_size(80, 24))
            .expect("openpty");
        let writer = pair.master.take_writer().expect("pty writer");
        drop(pair.slave);
        let handle = PtyHandle {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Some(Box::new(UnreadableChild { pid })),
            pid,
            exit_code: None,
        };
        let dropped = Instant::now();
        drop(handle);

        let status = status_within(&mut victim, KILL_GRACE + Duration::from_secs(5));
        let elapsed = dropped.elapsed();
        assert!(hung_up.exists(), "SIGHUP was not sent");
        assert_eq!(
            status.and_then(|status| status.signal()),
            Some(libc::SIGKILL),
            "no SIGKILL escalation: {status:?}"
        );
        assert!(
            elapsed >= KILL_GRACE,
            "SIGKILL came before the grace period: {elapsed:?}"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn group_members_lists_the_processes_of_a_group() {
        use std::os::unix::process::CommandExt;

        let mut leader = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn leader");
        let pgid = leader.id();
        let mut member = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(i32::try_from(pgid).expect("pgid fits i32"))
            .spawn()
            .expect("spawn member");
        let members = procinfo::group_members(pgid);
        let unknown = procinfo::group_members(i32::MAX as u32);
        for child in [&mut leader, &mut member] {
            child.kill().expect("kill");
            child.wait().expect("wait");
        }
        let mut expected = vec![pgid, member.id()];
        expected.sort_unstable();
        assert_eq!(members, expected);
        assert!(unknown.is_empty(), "{unknown:?}");
    }

    #[test]
    fn proc_stat_fields_are_counted_after_the_last_paren() {
        assert_eq!(
            parse_proc_stat("4242 (sleep) S 4200 4242 4200 34816 4242 4194304\n"),
            Some(('S', 4242))
        );
        // `comm` may itself contain spaces and parentheses.
        assert_eq!(parse_proc_stat("7 (a) (b c) Z 1 99 1 0"), Some(('Z', 99)));
        assert_eq!(parse_proc_stat("7 (truncated"), None);
        assert_eq!(parse_proc_stat("7 (x) R 1"), None);
    }
}
