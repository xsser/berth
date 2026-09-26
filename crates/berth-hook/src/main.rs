//! `berth-hook` — must never block or fail the calling agent.
//!
//! Usage (installed by `berth setup-hooks`):
//!   berth-hook claude                                  # stdin: Claude Code hook JSON
//!   berth-hook codex [--chain <notify cmd...>] <json>  # Codex notify (JSON = last argv)
//!   berth-hook statusline -- <statusline cmd...>       # tee stdin JSON, run original
//!
//! Behaviour: read stdin (bounded), map to `HookEnvelope`, connect to
//! `BERTH_SOCKET` (or the default socket) within 50 ms, send one frame pair,
//! exit 0. Any failure is silent (set `BERTH_HOOK_DEBUG=1` for stderr).
//! `codex --chain` then `exec`s the original notify command with the same
//! argv; `statusline` runs the original command with the same stdin and
//! passes its stdout and exit code through. No tokio, no clap: startup cost
//! matters.

mod map;
mod send;

use std::io::{self, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

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

/// Read stdin (≤ 1 MiB) on a helper thread so a stdin that never closes
/// cannot block the agent.
fn read_stdin_with_deadline() -> Option<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let res = io::stdin()
            .lock()
            .take(MAX_STDIN as u64 + 1)
            .read_to_end(&mut buf);
        let _ = tx.send(res.map(|_| buf));
    });
    match rx.recv_timeout(STDIN_DEADLINE) {
        Ok(Ok(buf)) if buf.len() <= MAX_STDIN => Some(buf),
        Ok(Ok(_)) => {
            log("stdin exceeds 1 MiB; ignored");
            None
        }
        Ok(Err(e)) => {
            log(format!("stdin: {e}"));
            None
        }
        Err(_) => {
            log("stdin deadline exceeded");
            None
        }
    }
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
    // Only returns on failure.
    let err = cmd.exec();
    log(format!("exec {program}: {err}"));
    0
}

fn statusline(args: &[String]) -> i32 {
    let command = map::parse_statusline_args(args);
    let mut stdin = io::stdin().lock();
    let mut head = Vec::new();
    if let Err(e) = (&mut stdin)
        .take(MAX_STDIN as u64 + 1)
        .read_to_end(&mut head)
    {
        log(format!("stdin: {e}"));
    }
    let complete = head.len() <= MAX_STDIN;
    if complete {
        deliver(map::statusline(&head, &map::Context::from_env()));
    } else {
        log("statusline JSON exceeds 1 MiB; not forwarded");
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
        let _ = pipe.write_all(&head);
        if !complete {
            let _ = io::copy(&mut stdin, &mut pipe);
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
