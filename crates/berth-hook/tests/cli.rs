//! Black-box tests of the `berth-hook` binary.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use berth_core::{
    decode_payload, AgentSignal, ClaudeHookEvent, ClientMsg, ClientRole, FrameReader, HookEnvelope,
    Request, SessionId, PROTOCOL_VERSION,
};

const BIN: &str = env!("CARGO_BIN_EXE_berth-hook");

fn run(args: &[&str], socket: &Path, sid: Option<SessionId>, stdin: &[u8]) -> (Output, Duration) {
    let mut cmd = Command::new(BIN);
    cmd.args(args)
        .env("BERTH_SOCKET", socket)
        .env_remove("BERTH_SESSION_ID")
        .env_remove("BERTH_HOOK_DEBUG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(sid) = sid {
        cmd.env("BERTH_SESSION_ID", sid.to_string());
    }
    let start = Instant::now();
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    (out, start.elapsed())
}

/// Accept one connection and decode every frame it carries. Waits at most
/// 5 s: a hook that dropped its event must fail the test, not hang it.
fn receive(listener: &UnixListener) -> Vec<ClientMsg> {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut conn = loop {
        match listener.accept() {
            Ok((conn, _)) => break conn,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "the hook delivered nothing within 5 s"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("accept: {e}"),
        }
    };
    // Accepted sockets inherit O_NONBLOCK from the listener on macOS.
    conn.set_nonblocking(false).unwrap();
    // macOS rejects SO_RCVTIMEO (EINVAL) once the peer has already closed,
    // which is the normal case here: the hook writes and exits immediately.
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut bytes = Vec::new();
    conn.read_to_end(&mut bytes).unwrap();
    let mut reader = FrameReader::new();
    reader.push(&bytes);
    let mut msgs = Vec::new();
    while let Some(frame) = reader.next_frame().unwrap() {
        msgs.push(decode_payload::<ClientMsg>(&frame).unwrap());
    }
    msgs
}

fn envelope(msgs: &[ClientMsg]) -> HookEnvelope {
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    match &msgs[0].req {
        Request::Hello { role, protocol, .. } => {
            assert_eq!(*role, ClientRole::Hook);
            assert_eq!(*protocol, PROTOCOL_VERSION);
        }
        other => panic!("first frame must be Hello, got {other:?}"),
    }
    match &msgs[1].req {
        Request::Hook(env) => env.clone(),
        other => panic!("expected Hook, got {other:?}"),
    }
}

const PRE_TOOL: &[u8] =
    br#"{"session_id":"abc123","cwd":"/w","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#;

#[test]
fn no_daemon_exits_zero_within_50ms() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.sock");
    // A stale socket file with no listener behind it (ECONNREFUSED).
    let stale = dir.path().join("stale.sock");
    drop(UnixListener::bind(&stale).unwrap());
    for socket in [&missing, &stale] {
        let mut times = Vec::new();
        for _ in 0..5 {
            let (out, took) = run(&["claude"], socket, None, PRE_TOOL);
            assert_eq!(out.status.code(), Some(0));
            assert!(
                out.stdout.is_empty() && out.stderr.is_empty(),
                "must stay silent"
            );
            times.push(took);
        }
        times.sort();
        assert!(
            times[2] < Duration::from_millis(50),
            "median {:?} ({times:?})",
            times[2]
        );
    }
    // Garbage input and unknown modes are silent successes too.
    let (out, _) = run(&["claude"], &missing, None, b"\x00garbage");
    assert_eq!(out.status.code(), Some(0));
    let (out, _) = run(&["frobnicate"], &missing, None, b"");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn claude_event_reaches_socket() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let sid = SessionId::new();
    let (out, _) = run(&["claude"], &socket, Some(sid), PRE_TOOL);
    assert_eq!(out.status.code(), Some(0));
    let env = envelope(&receive(&listener));
    assert_eq!(env.berth_session, Some(sid));
    assert!(env.pid > 0);
    let AgentSignal::Claude(hook) = env.signal else {
        panic!("{:?}", env.signal)
    };
    assert_eq!(hook.session_id, "abc123");
    assert_eq!(
        hook.event,
        ClaudeHookEvent::PreToolUse {
            tool_name: "Bash".into()
        }
    );
}

#[test]
fn stdin_that_never_closes_does_not_hang() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(BIN)
        .arg("claude")
        .env("BERTH_SOCKET", dir.path().join("none.sock"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _keep_open = child.stdin.take();
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "berth-hook hung on stdin"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0));
}

#[test]
fn statusline_tees_and_passes_through() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let json = br#"{"session_id":"s1","model":{"id":"claude-opus-5-5"},"context_window":{"used_percentage":42.5},"cost":{"total_cost_usd":1.5},"workspace":{"project_dir":"/p"},"cwd":"/p/sub"}"#;
    let (out, _) = run(
        &[
            "statusline",
            "--",
            "/bin/sh",
            "-c",
            "cat; printf ' <- original'; exit 3",
        ],
        &socket,
        None,
        json,
    );
    assert_eq!(out.status.code(), Some(3));
    let mut expected = json.to_vec();
    expected.extend_from_slice(b" <- original");
    assert_eq!(out.stdout, expected);
    let env = envelope(&receive(&listener));
    let AgentSignal::Statusline(s) = env.signal else {
        panic!()
    };
    assert_eq!(s.session_id, "s1");
    assert_eq!(s.model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(s.context_pct, Some(42.5));
    assert_eq!(s.cost_usd, Some(1.5));

    // Without a daemon the original statusline still works unchanged.
    let (out, _) = run(
        &["statusline", "--", "/bin/cat"],
        &dir.path().join("x.sock"),
        None,
        json,
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, json.to_vec());
}

#[test]
fn codex_chain_execs_original_with_same_argv() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let json = r#"{"type":"agent-turn-complete","thread-id":"t-1","cwd":"/w","input-messages":["hi"],"last-assistant-message":"done"}"#;
    let (out, _) = run(
        &["codex", "--chain", "/bin/echo", "orig-arg", json],
        &socket,
        None,
        b"",
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("orig-arg {json}\n")
    );
    let env = envelope(&receive(&listener));
    let AgentSignal::Codex(n) = env.signal else {
        panic!()
    };
    assert_eq!(n.event_type, "agent-turn-complete");
    assert_eq!(n.thread_id.as_deref(), Some("t-1"));
}

/// Review medium #5: when the chained notify command cannot be executed the
/// hook reports it like a shell (127) — still silently, still after sending.
#[test]
fn codex_chain_exec_failure_exits_127() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let json = r#"{"type":"agent-turn-complete","thread-id":"t-1"}"#;
    let (out, _) = run(
        &["codex", "--chain", "/nonexistent/notifier", json],
        &socket,
        None,
        b"",
    );
    assert_eq!(out.status.code(), Some(127));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    let env = envelope(&receive(&listener));
    assert!(matches!(env.signal, AgentSignal::Codex(_)));
    // Without --chain there is nothing to fail.
    let (out, _) = run(&["codex", json], &dir.path().join("x.sock"), None, b"");
    assert_eq!(out.status.code(), Some(0));
}

/// Review medium #4: a statusline stdin that stays open is not forwarded
/// after the 1 s deadline, but the original command still gets what was
/// read and the rest is passed through.
#[test]
fn statusline_stdin_that_stays_open_still_runs_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut child = Command::new(BIN)
        .args(["statusline", "--", "/bin/cat"])
        .env("BERTH_SOCKET", &socket)
        .env_remove("BERTH_SESSION_ID")
        .env_remove("BERTH_HOOK_DEBUG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        while let Ok(n) = stdout.read(&mut buf) {
            if tx.send(buf[..n].to_vec()).is_err() || n == 0 {
                break;
            }
        }
    });
    let read_until = |want: &[u8]| {
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !got.ends_with(want) {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(chunk) if !chunk.is_empty() => got.extend_from_slice(&chunk),
                other => panic!("original command output so far {got:?}; then {other:?}"),
            }
        }
        got
    };

    let partial = br#"{"session_id":"s1","model":{"id":"m"}"#;
    stdin.write_all(partial).unwrap();
    // stdin stays open: the original runs after the deadline with the head.
    assert_eq!(read_until(partial), partial.to_vec());
    stdin.write_all(b"}\n").unwrap();
    assert_eq!(read_until(b"}\n"), b"}\n".to_vec());
    drop(stdin);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
    // Nothing was sent to the daemon.
    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("statusline forwarded after the deadline: {other:?}"),
    }
}
