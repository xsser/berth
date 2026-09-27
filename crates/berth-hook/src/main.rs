//! `berth-hook` — must never block or fail the calling agent.
//!
//! Usage (installed by `berth setup-hooks`):
//!   berth-hook claude                                  # stdin: Claude Code hook JSON
//!   berth-hook codex [--chain <notify cmd...>] <json>  # Codex notify (JSON = last argv)
//!   berth-hook statusline -- <statusline cmd...>       # tee stdin JSON, run original
//!
//! Behaviour: read stdin (bounded: 1 MiB, 1 s), map to `HookEnvelope`,
//! connect to `BERTH_SOCKET` (or the default socket) within 50 ms, send one
//! frame pair, exit 0. Any failure is silent (set `BERTH_HOOK_DEBUG=1` for
//! stderr). `codex --chain` then `exec`s the original notify command with the
//! same argv (127 if that fails); `statusline` runs the original command with
//! the same stdin — also when the JSON was not forwarded (too large, or stdin
//! still open after the deadline: then the rest is passed through) — and
//! passes its stdout and exit code through. No tokio, no clap: startup cost
//! matters.
//!
//! Only a hook running inside a berth session forwards: without a valid
//! `BERTH_SESSION_ID` (unset, empty or not a session id) nothing is sent and
//! no socket is touched. The hooks are installed globally, so agents in other
//! terminals run them too, and berthd's cwd fallback would pin their events
//! on a berth session in the same directory. `codex --chain` and
//! `statusline` still run the original command as above.

mod map;
mod send;

use std::io::{self, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Hook payloads larger than this are not forwarded.
const MAX_STDIN: usize = 1024 * 1024;
/// Claude closes stdin right after writing the JSON; anything slower is
/// misuse (e.g. run by hand on a TTY) and must not hang the caller.
const STDIN_DEADLINE: Duration = Duration::from_secs(1);

fn debug() -> bool {
    std::env::var_os("BERTH_HOOK_DEBUG").is_some_and(|v| !v.is_empty() && v != "0")
}

fn log(msg: impl AsRef<str>) {
    if debug() {
        eprintln!("berth-hook: {}", msg.as_ref());
    }
}

fn deliver(envelope: Option<berth_core::HookEnvelope>) {
    let Some(envelope) = envelope else {
        log("payload not recognised; nothing sent");
        return;
    };
    if envelope.berth_session.is_none() {
        log("not in a berth session (no valid BERTH_SESSION_ID); nothing sent");
        return;
    }
    let socket = berth_core::Paths::resolve().socket;
    if let Err(e) = send::send(envelope, &socket, send::BUDGET) {
        log(e);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("claude") => {
            let ctx = map::Context::from_env();
            deliver(read_stdin_with_deadline().and_then(|json| map::claude(&json, &ctx)));
            0
        }
        Some("codex") => codex(&args[1..]),
        Some("statusline") => statusline(&args[1..]),
        other => {
            log(format!("unknown mode {other:?}"));
            0
        }
    };
    std::process::exit(code);
}

/// Chunks of stdin from a helper thread; an empty chunk marks EOF. The
/// thread keeps reading after the deadline (for pass-through) and dies with
/// the process.
type StdinChunks = Receiver<io::Result<Vec<u8>>>;

fn stdin_reader() -> StdinChunks {
    let (tx, rx) = mpsc::sync_channel(8);
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            let mut buf = vec![0u8; 64 * 1024];
            match stdin.read(&mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    if tx.send(Ok(buf)).is_err() || n == 0 {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    });
    rx
}

/// What arrived on stdin within `STDIN_DEADLINE` (at most a chunk beyond
/// `MAX_STDIN`).
struct Head {
    bytes: Vec<u8>,
    /// The stream ended (EOF or read error) within the deadline.
    ended: bool,
}

impl Head {
    /// The whole payload, if it ended in time and fits.
    fn complete(&self) -> Option<&[u8]> {
        (self.ended && self.bytes.len() <= MAX_STDIN).then_some(&self.bytes[..])
    }

    fn why_incomplete(&self) -> &'static str {
        if self.bytes.len() > MAX_STDIN {
            "stdin exceeds 1 MiB"
        } else {
            "stdin deadline exceeded"
        }
    }
}

fn read_head(chunks: &StdinChunks) -> Head {
    let deadline = Instant::now() + STDIN_DEADLINE;
    let mut bytes = Vec::new();
    while bytes.len() <= MAX_STDIN {
        let left = deadline.saturating_duration_since(Instant::now());
        match chunks.recv_timeout(left) {
            Ok(Ok(chunk)) if chunk.is_empty() => return Head { bytes, ended: true },
            Ok(Ok(chunk)) => bytes.extend_from_slice(&chunk),
            Ok(Err(e)) => {
                log(format!("stdin: {e}"));
                return Head { bytes, ended: true };
            }
            Err(RecvTimeoutError::Disconnected) => return Head { bytes, ended: true },
            Err(RecvTimeoutError::Timeout) => break,
        }
    }
    Head {
        bytes,
        ended: false,
    }
}

/// Read stdin (≤ 1 MiB) on a helper thread so a stdin that never closes
/// cannot block the agent.
fn read_stdin_with_deadline() -> Option<Vec<u8>> {
    let head = read_head(&stdin_reader());
    if head.complete().is_none() {
        log(format!("{}; ignored", head.why_incomplete()));
        return None;
    }
    Some(head.bytes)
}

fn codex(args: &[String]) -> i32 {
    let parsed = map::parse_codex_args(args);
    if let Some(json) = &parsed.json {
        deliver(map::codex(json, &map::Context::from_env()));
    }
    let Some((program, rest)) = parsed.chain.split_first() else {
        return 0;
    };
    let mut cmd = Command::new(program);
    cmd.args(rest);
    if let Some(json) = parsed.json {
        cmd.arg(json);
    }
    // Only returns on failure: report it like a shell would.
    let err = cmd.exec();
    log(format!("exec {program}: {err}"));
    127
}

fn statusline(args: &[String]) -> i32 {
    let command = map::parse_statusline_args(args);
    let chunks = stdin_reader();
    let head = read_head(&chunks);
    match head.complete() {
        Some(json) => deliver(map::statusline(json, &map::Context::from_env())),
        None => log(format!(
            "{}; statusline not forwarded",
            head.why_incomplete()
        )),
    }
    let Some((program, rest)) = command.split_first() else {
        return 0;
    };
    let mut child = match Command::new(program)
        .args(rest)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log(format!("spawn {program}: {e}"));
            return 127;
        }
    };
    if let Some(mut pipe) = child.stdin.take() {
        // A child that exits without reading gives EPIPE: not our problem.
        if pipe.write_all(&head.bytes).is_ok() && !head.ended {
            pass_through(&chunks, &mut pipe, &mut child);
        }
    }
    match child.wait() {
        Ok(status) => status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
        Err(e) => {
            log(format!("wait: {e}"));
            1
        }
    }
}

/// Copy the rest of stdin to the child until either side is done; a child
/// that exits without reading ends it even if stdin never closes.
fn pass_through(chunks: &StdinChunks, pipe: &mut impl Write, child: &mut Child) {
    loop {
        match chunks.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(chunk)) if !chunk.is_empty() => {
                if pipe.write_all(&chunk).is_err() {
                    return;
                }
            }
            Ok(_) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {
                if !matches!(child.try_wait(), Ok(None)) {
                    return;
                }
            }
        }
    }
}
