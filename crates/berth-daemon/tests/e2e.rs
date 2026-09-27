//! End-to-end: in-process `berth_daemon::run` on a temp dir, a real PTY
//! (`/bin/sh`), and a protocol client speaking the framed socket protocol.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use berth_core::{
    decode_payload, encode_frame, now_ms, AgentKind, AgentSignal, AgentState, ClaudeHook,
    ClaudeHookEvent, ClientMsg, ClientRole, DaemonMsg, Dims, Event, FrameReader, HookEnvelope,
    LineSnapshot, Paths, Request, ReviveMode, ScreenUpdate, SessionId, SessionMeta, SessionStatus,
    StateSource, SubscribeMode, Workspace, WorkspaceId, PROTOCOL_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(15);
const DIMS: Dims = Dims { cols: 80, rows: 24 };

struct Daemon {
    stop: watch::Sender<bool>,
    task: JoinHandle<anyhow::Result<()>>,
}

fn start_daemon(paths: &Paths) -> Daemon {
    start_daemon_with(paths, berth_daemon::Config::default())
}

fn start_daemon_with(paths: &Paths, config: berth_daemon::Config) -> Daemon {
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(berth_daemon::run(paths.clone(), config, rx));
    Daemon { stop, task }
}

impl Daemon {
    async fn join(self) {
        let res = tokio::time::timeout(WAIT, self.task)
            .await
            .expect("daemon did not stop");
        res.expect("daemon task panicked")
            .expect("daemon returned an error");
    }
}

struct Client {
    stream: UnixStream,
    frames: FrameReader,
    backlog: VecDeque<DaemonMsg>,
    next_id: u32,
    /// Heap buffer: a stack array held across `.await` would be copied into
    /// every enclosing future and overflow the test thread's stack.
    buf: Vec<u8>,
}

impl Client {
    async fn connect(socket: &Path) -> Client {
        Client::connect_as(socket, ClientRole::Gui).await
    }

    async fn connect_as(socket: &Path, role: ClientRole) -> Client {
        let deadline = Instant::now() + WAIT;
        let stream = loop {
            match UnixStream::connect(socket).await {
                Ok(s) => break s,
                Err(e) if Instant::now() < deadline => {
                    let _ = e;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("cannot connect to {}: {e}", socket.display()),
            }
        };
        let mut c = Client {
            stream,
            frames: FrameReader::new(),
            backlog: VecDeque::new(),
            next_id: 1,
            buf: vec![0u8; 64 * 1024],
        };
        match c
            .request(Request::Hello {
                role,
                protocol: PROTOCOL_VERSION,
                client_version: "e2e".into(),
            })
            .await
        {
            Event::Hello { protocol, .. } => assert_eq!(protocol, PROTOCOL_VERSION),
            other => panic!("expected Hello, got {other:?}"),
        }
        c
    }

    async fn send(&mut self, req: Request) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let frame = encode_frame(&ClientMsg { id, req }).unwrap();
        self.stream.write_all(&frame).await.unwrap();
        id
    }

    /// Next message from the socket (not the backlog).
    async fn read_one(&mut self, deadline: Instant) -> Option<DaemonMsg> {
        loop {
            if let Some(frame) = self.frames.next_frame().unwrap() {
                return Some(decode_payload(&frame).unwrap());
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            match tokio::time::timeout(left, self.stream.read(&mut self.buf)).await {
                Ok(Ok(0)) => return None,
                Ok(Ok(n)) => self.frames.push(&self.buf[..n]),
                Ok(Err(e)) => panic!("read: {e}"),
                Err(_) => return None,
            }
        }
    }

    /// First message (backlog first) matching `pred`; others are kept.
    async fn wait_for(
        &mut self,
        what: &str,
        mut pred: impl FnMut(&DaemonMsg) -> bool,
    ) -> DaemonMsg {
        if let Some(pos) = self.backlog.iter().position(&mut pred) {
            return self.backlog.remove(pos).unwrap();
        }
        let deadline = Instant::now() + WAIT;
        loop {
            match self.read_one(deadline).await {
                Some(msg) if pred(&msg) => return msg,
                Some(msg) => self.backlog.push_back(msg),
                None => panic!(
                    "timed out waiting for {what}; backlog: {:?}",
                    summarize(&self.backlog)
                ),
            }
        }
    }

    async fn request(&mut self, req: Request) -> Event {
        let id = self.send(req).await;
        self.wait_for("reply", |m| m.reply_to == Some(id))
            .await
            .event
    }

    /// Wait until the mirrored screen satisfies `pred`, applying every
    /// `Screen` update for `sid` (backlog first).
    async fn screen_until(
        &mut self,
        sid: SessionId,
        model: &mut ScreenModel,
        what: &str,
        pred: impl Fn(&ScreenModel) -> bool,
    ) {
        let pending: Vec<DaemonMsg> = self.backlog.drain(..).collect();
        for msg in pending {
            match &msg.event {
                Event::Screen(u) if u.session == sid => model.apply(u),
                _ => self.backlog.push_back(msg),
            }
        }
        let deadline = Instant::now() + WAIT;
        while !pred(model) {
            match self.read_one(deadline).await {
                Some(DaemonMsg {
                    event: Event::Screen(u),
                    ..
                }) if u.session == sid => model.apply(&u),
                Some(msg) => self.backlog.push_back(msg),
                None => panic!(
                    "timed out waiting for screen: {what}; screen: {:#?}",
                    model.rows
                ),
            }
        }
    }
}

fn summarize(q: &VecDeque<DaemonMsg>) -> Vec<String> {
    q.iter()
        .map(|m| {
            let s = format!("{:?}", m.event);
            s.chars().take(120).collect()
        })
        .collect()
}

#[derive(Default, Debug)]
struct ScreenModel {
    rows: BTreeMap<u16, String>,
    history_len: u64,
    updates: usize,
}

impl ScreenModel {
    fn apply(&mut self, u: &ScreenUpdate) {
        if u.full {
            self.rows.clear();
        }
        for (row, line) in &u.lines {
            self.rows.insert(*row, line.text());
        }
        self.history_len = u.history_len;
        self.updates += 1;
    }

    fn has(&self, marker: &str) -> bool {
        self.rows.values().any(|t| has_marker(t, marker))
    }
}

/// `marker` immediately followed by a digit: `hi-123` from `echo hi-$$`
/// (the echoed command line itself shows `hi-$$`).
fn has_marker(text: &str, marker: &str) -> bool {
    text.match_indices(marker).any(|(i, _)| {
        text[i + marker.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    })
}

fn position(lines: &[LineSnapshot], marker: &str) -> Option<usize> {
    lines.iter().position(|l| has_marker(&l.text(), marker))
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

async fn workspace_and_shell(c: &mut Client, root: &Path) -> (WorkspaceId, SessionMeta) {
    workspace_and_command(c, root, &["/bin/sh"]).await
}

async fn workspace_and_command(
    c: &mut Client,
    root: &Path,
    argv: &[&str],
) -> (WorkspaceId, SessionMeta) {
    let ws = match c
        .request(Request::CreateWorkspace {
            name: "w".into(),
            root: root.to_path_buf(),
        })
        .await
    {
        Event::WorkspaceUpdated(ws) => ws,
        other => panic!("{other:?}"),
    };
    let req = Request::CreateSession {
        workspace: ws.id,
        cwd: None,
        command: Some(argv.iter().map(|a| a.to_string()).collect()),
        title: None,
        dims: DIMS,
    };
    match c.request(req).await {
        Event::SessionUpdated(meta) => {
            assert_eq!(meta.status, SessionStatus::Live);
            assert_eq!(meta.cwd, root);
            (ws.id, meta)
        }
        other => panic!("{other:?}"),
    }
}

/// Attach and mirror the full screen of the reply.
async fn attach(c: &mut Client, sid: SessionId) -> ScreenModel {
    let id = c
        .send(Request::Attach {
            session: sid,
            dims: DIMS,
        })
        .await;
    let Event::Screen(first) = c
        .wait_for("attach reply", |m| m.reply_to == Some(id))
        .await
        .event
    else {
        panic!("attach must be answered by a Screen");
    };
    let mut screen = ScreenModel::default();
    screen.apply(&first);
    screen
}

async fn exited(c: &mut Client, sid: SessionId) -> Option<i32> {
    match c
        .wait_for(
            "Exited",
            |m| matches!(m.event, Event::Exited { session, .. } if session == sid),
        )
        .await
        .event
    {
        Event::Exited { code, .. } => code,
        _ => unreachable!(),
    }
}

fn claude(sid: SessionId, event: ClaudeHookEvent) -> Request {
    claude_as(sid, "claude-e2e", event)
}

/// A Claude hook event carrying `session_id` (the agent's own session id).
fn claude_as(sid: SessionId, session_id: &str, event: ClaudeHookEvent) -> Request {
    Request::Hook(HookEnvelope {
        berth_session: Some(sid),
        pid: std::process::id(),
        sent_at_ms: now_ms(),
        signal: AgentSignal::Claude(ClaudeHook {
            session_id: session_id.into(),
            cwd: None,
            transcript_path: None,
            permission_mode: Some("default".into()),
            event,
        }),
    })
}

async fn agent_state(c: &mut Client, sid: SessionId, want: AgentState) {
    c.wait_for(&format!("agent state {want:?}"), |m| {
        matches!(&m.event, Event::AgentChanged { session, agent } if *session == sid && agent.state == want)
    })
    .await;
}

/// Task D.1–D.3 in one scenario (the restart needs the same session).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_hooks_kill_restart_revive() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let daemon = start_daemon(&paths);
    let mut c = Client::connect(&paths.socket).await;

    // Single instance: a second daemon on the same dir returns immediately.
    let (_keep, rx2) = watch::channel(false);
    let second = Box::pin(berth_daemon::run(
        paths.clone(),
        berth_daemon::Config::default(),
        rx2,
    ));
    tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("second daemon must not block")
        .expect("second daemon returns Ok");

    // D.1: create → attach → input → screen → detach → re-attach.
    let (_ws, meta) = workspace_and_shell(&mut c, root.path()).await;
    let sid = meta.id;
    let attach = c
        .send(Request::Attach {
            session: sid,
            dims: DIMS,
        })
        .await;
    let Event::Screen(first) = c
        .wait_for("attach reply", |m| m.reply_to == Some(attach))
        .await
        .event
    else {
        panic!("attach must be answered by a Screen");
    };
    assert!(first.full);
    assert_eq!(first.lines.len(), usize::from(DIMS.rows));
    let mut screen = ScreenModel::default();
    screen.apply(&first);
    c.send(Request::Input {
        session: sid,
        data: b"echo hi-$$\n".to_vec(),
    })
    .await;
    c.screen_until(sid, &mut screen, "hi-<pid>", |s| s.has("hi-"))
        .await;
    assert_eq!(c.request(Request::Detach { session: sid }).await, Event::Ok);
    let again = c
        .send(Request::Attach {
            session: sid,
            dims: DIMS,
        })
        .await;
    let Event::Screen(full) = c
        .wait_for("re-attach reply", |m| m.reply_to == Some(again))
        .await
        .event
    else {
        panic!("re-attach must be answered by a Screen");
    };
    assert!(full.full && !full.styles.is_empty());
    let mut fresh = ScreenModel::default();
    fresh.apply(&full);
    assert!(
        fresh.has("hi-"),
        "re-attached screen lost the line: {:#?}",
        fresh.rows
    );

    // D.2: hook events (on a Hook connection) drive the agent state.
    let mut hook = Client::connect_as(&paths.socket, ClientRole::Hook).await;
    assert_eq!(
        hook.request(claude(
            sid,
            ClaudeHookEvent::PreToolUse {
                tool_name: "Bash".into()
            }
        ))
        .await,
        Event::Ok
    );
    agent_state(
        &mut c,
        sid,
        AgentState::ToolRunning {
            tool: "Bash".into(),
        },
    )
    .await;
    let permission = ClaudeHookEvent::Notification {
        notification_type: Some("permission_prompt".into()),
        message: "Claude needs your permission to use Bash".into(),
    };
    assert_eq!(hook.request(claude(sid, permission)).await, Event::Ok);
    agent_state(
        &mut c,
        sid,
        AgentState::WaitingPermission {
            tool: Some("Bash".into()),
        },
    )
    .await;
    assert_eq!(
        hook.request(claude(
            sid,
            ClaudeHookEvent::Stop {
                stop_hook_active: false
            }
        ))
        .await,
        Event::Ok
    );
    agent_state(&mut c, sid, AgentState::Done).await;

    // D.3: kill → Exited → snapshot on disk; shutdown.
    assert_eq!(c.request(Request::Kill { session: sid }).await, Event::Ok);
    c.wait_for(
        "Exited",
        |m| matches!(m.event, Event::Exited { session, .. } if session == sid),
    )
    .await;
    let snap = paths.snapshot_file(&sid);
    assert!(snap.exists(), "snapshot written on exit");
    assert_eq!(mode(dir.path()), 0o700);
    assert_eq!(mode(&paths.snapshots_dir), 0o700);
    assert_eq!(mode(&paths.db), 0o600);
    assert_eq!(mode(&paths.socket), 0o600);
    assert_eq!(mode(&paths.lock), 0o600);
    assert_eq!(mode(&snap), 0o600);
    match c.request(Request::ListSessions).await {
        Event::Sessions(list) => assert!(matches!(list[0].status, SessionStatus::Dormant { .. })),
        other => panic!("{other:?}"),
    }
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
    assert!(!paths.socket.exists(), "socket removed on shutdown");
    {
        let store = berth_store::Store::open(&paths).unwrap();
        let events = store.list_events(sid, 100).unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        for want in [
            "hook:PreToolUse",
            "hook:Notification",
            "hook:Stop",
            "pty:exit",
        ] {
            assert!(kinds.contains(&want), "missing {want} in {kinds:?}");
        }
        assert!(events.iter().all(|e| !e
            .detail
            .as_deref()
            .unwrap_or("")
            .contains("permission to use")));
    }

    // Restart on the same directory: Restored + history + revive below it.
    let daemon = start_daemon(&paths);
    let mut c = Client::connect(&paths.socket).await;
    let list = match c.request(Request::ListSessions).await {
        Event::Sessions(list) => list,
        other => panic!("{other:?}"),
    };
    let restored = list
        .iter()
        .find(|m| m.id == sid)
        .expect("session survives restart");
    assert_eq!(restored.status, SessionStatus::Restored);
    assert_eq!(restored.agent.external_id.as_deref(), Some("claude-e2e"));
    let Event::Lines {
        start,
        lines: history,
        ..
    } = c
        .request(Request::FetchLines {
            session: sid,
            start: 0,
            count: 1000,
        })
        .await
    else {
        panic!("FetchLines must answer with Lines");
    };
    assert_eq!(start, 0);
    let hi_at = position(&history, "hi-").expect("restored history contains hi-<pid>");
    let prefix_len = history.len();

    match c
        .request(Request::Revive {
            session: sid,
            mode: ReviveMode::Shell,
        })
        .await
    {
        Event::SessionUpdated(m) => assert_eq!(m.status, SessionStatus::Live),
        other => panic!("{other:?}"),
    }
    let attach = c
        .send(Request::Attach {
            session: sid,
            dims: DIMS,
        })
        .await;
    let Event::Screen(first) = c
        .wait_for("attach after revive", |m| m.reply_to == Some(attach))
        .await
        .event
    else {
        panic!();
    };
    assert!(
        first.history_len >= prefix_len as u64,
        "restored prefix precedes the live screen"
    );
    let mut screen = ScreenModel::default();
    screen.apply(&first);
    c.send(Request::Input {
        session: sid,
        data: b"echo revived-$$\n".to_vec(),
    })
    .await;
    c.screen_until(sid, &mut screen, "revived-<pid>", |s| s.has("revived-"))
        .await;
    let Event::Lines { lines: all, .. } = c
        .request(Request::FetchLines {
            session: sid,
            start: 0,
            count: 5000,
        })
        .await
    else {
        panic!();
    };
    let hi = position(&all, "hi-").expect("old line still in virtual history");
    let revived = position(&all, "revived-").expect("new output reachable via FetchLines");
    assert_eq!(hi, hi_at);
    assert!(
        hi < prefix_len && revived >= prefix_len,
        "hi at {hi}, revived at {revived}, prefix {prefix_len}"
    );
    match c.request(Request::DaemonStatus).await {
        Event::Status(s) => assert_eq!((s.sessions_live, s.sessions_total), (1, 1)),
        other => panic!("{other:?}"),
    }
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

/// Task D.4: a 3-row / 4 Hz preview yields at most 5 updates per second.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preview_subscription_is_throttled() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let daemon = start_daemon(&paths);
    let mut c = Client::connect(&paths.socket).await;
    let (_ws, meta) = workspace_and_shell(&mut c, root.path()).await;
    let sid = meta.id;
    // ~2 s of continuous output so there is always something new to preview.
    let script = b"i=0; while [ $i -lt 40 ]; do echo tick-$i; i=$((i+1)); sleep 0.05; done\n";
    c.send(Request::Input {
        session: sid,
        data: script.to_vec(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let sub = c
        .send(Request::Subscribe {
            session: sid,
            mode: SubscribeMode::Preview { rows: 3, max_hz: 4 },
        })
        .await;
    let t0 = Instant::now();
    let first = c
        .wait_for("first preview", |m| m.reply_to == Some(sub))
        .await;
    assert!(matches!(first.event, Event::Preview { session, .. } if session == sid));
    let mut count = 1;
    let mut last_text = Vec::new();
    while let Some(msg) = c.read_one(t0 + Duration::from_secs(1)).await {
        if let Event::Preview { session, lines, .. } = msg.event {
            if session == sid {
                assert!(lines.len() <= 3);
                last_text = lines.iter().map(LineSnapshot::text).collect();
                count += 1;
            }
        }
    }
    assert!(count <= 5, "{count} previews within 1 s");
    assert!(
        count >= 2,
        "preview must refresh while output flows (got {count})"
    );
    assert!(
        last_text.iter().any(|t| t.starts_with("tick-")),
        "{last_text:?}"
    );
    daemon.stop.send_replace(true);
    daemon.join().await;
}

/// First frame must be `Hello`; a protocol mismatch gets `Incompatible`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let daemon = start_daemon(&paths);
    // Wait for the socket via a well-behaved client first.
    drop(Client::connect(&paths.socket).await);

    async fn first_reply(socket: &Path, req: Request) -> (Event, bool) {
        let mut s = UnixStream::connect(socket).await.unwrap();
        s.write_all(&encode_frame(&ClientMsg { id: 1, req }).unwrap())
            .await
            .unwrap();
        let mut bytes = Vec::new();
        let closed = tokio::time::timeout(WAIT, s.read_to_end(&mut bytes))
            .await
            .is_ok();
        let mut r = FrameReader::new();
        r.push(&bytes);
        let msg: DaemonMsg = decode_payload(&r.next_frame().unwrap().expect("one reply")).unwrap();
        (msg.event, closed)
    }
    let (ev, closed) = first_reply(&paths.socket, Request::ListSessions).await;
    assert!(matches!(ev, Event::Error { .. }) && closed, "{ev:?}");
    let hello = Request::Hello {
        role: ClientRole::Cli,
        protocol: PROTOCOL_VERSION + 1,
        client_version: "x".into(),
    };
    let (ev, closed) = first_reply(&paths.socket, hello).await;
    assert_eq!(
        ev,
        Event::Incompatible {
            daemon_protocol: PROTOCOL_VERSION
        }
    );
    assert!(closed);
    daemon.stop.send_replace(true);
    daemon.join().await;
}

/// berth-vt's kill flow: SIGHUP, poll `try_wait`, SIGKILL after `KILL_GRACE`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_escalates_to_sigkill_when_sighup_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let daemon = start_daemon(&paths);
    let mut c = Client::connect(&paths.socket).await;
    let script = "trap '' HUP; echo ready-$((40+2)); while :; do sleep 1; done";
    let (_ws, meta) = workspace_and_command(&mut c, root.path(), &["/bin/sh", "-c", script]).await;
    let sid = meta.id;
    let mut screen = attach(&mut c, sid).await;
    c.screen_until(sid, &mut screen, "ready-42", |s| s.has("ready-"))
        .await;
    let t0 = Instant::now();
    assert_eq!(c.request(Request::Kill { session: sid }).await, Event::Ok);
    let code = exited(&mut c, sid).await;
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_millis(900),
        "SIGHUP is ignored, so only SIGKILL after the grace period ends it: {took:?}"
    );
    assert!(took < Duration::from_secs(5), "escalation took {took:?}");
    assert_eq!(code, Some(128 + 9), "killed by SIGKILL");
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

/// The shell exits while a background job (ignoring SIGHUP) still has the
/// tty open. macOS revokes the tty when the session leader exits, so EOF
/// comes anyway; on a Linux pty it would not and the `try_wait` poll ends
/// the session. Either way it is Exited promptly with its output kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exit_is_detected_while_a_background_job_holds_the_tty() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let daemon = start_daemon(&paths);
    let mut c = Client::connect(&paths.socket).await;
    let script = "(trap '' HUP; exec sleep 5) & echo bg-$((40+2)); exit 3";
    let t0 = Instant::now();
    let (_ws, meta) = workspace_and_command(&mut c, root.path(), &["/bin/sh", "-c", script]).await;
    let sid = meta.id;
    let code = exited(&mut c, sid).await;
    let took = t0.elapsed();
    assert!(
        took < Duration::from_millis(3500),
        "exit noticed after {took:?}"
    );
    assert_eq!(code, Some(3));
    let screen = attach(&mut c, sid).await;
    assert!(screen.has("bg-"), "output kept: {:#?}", screen.rows);
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

/// DEC 2026: output inside a synchronized update whose end marker never
/// comes is released at the terminal's sync deadline instead of freezing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unterminated_synchronized_update_does_not_freeze_the_screen() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let daemon = start_daemon(&paths);
    let mut c = Client::connect(&paths.socket).await;
    let (_ws, meta) = workspace_and_shell(&mut c, root.path()).await;
    let sid = meta.id;
    let mut screen = attach(&mut c, sid).await;
    c.send(Request::Input {
        session: sid,
        data: b"printf '\\033[?2026h'; echo sync-$((40+2))\n".to_vec(),
    })
    .await;
    c.screen_until(sid, &mut screen, "sync-42", |s| s.has("sync-"))
        .await;
    // The terminal keeps working after the timed-out update.
    c.send(Request::Input {
        session: sid,
        data: b"echo after-$((1+1))\n".to_vec(),
    })
    .await;
    c.screen_until(sid, &mut screen, "after-2", |s| s.has("after-"))
        .await;
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

/// Review: a Hook-role connection must not control the daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hook_role_cannot_control_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let daemon = start_daemon(&paths);
    let mut gui = Client::connect(&paths.socket).await;
    let (_ws, meta) = workspace_and_shell(&mut gui, root.path()).await;

    for req in [
        Request::Shutdown,
        Request::Kill { session: meta.id },
        Request::Delete { session: meta.id },
    ] {
        let mut hook = Client::connect_as(&paths.socket, ClientRole::Hook).await;
        match hook.request(req.clone()).await {
            Event::Error { message } => assert!(message.contains("Hook"), "{message}"),
            other => panic!("{req:?} from a hook connection answered {other:?}"),
        }
        // ...and the connection is closed.
        let closed = hook.read_one(Instant::now() + Duration::from_secs(5)).await;
        assert!(closed.is_none(), "connection still open: {closed:?}");
    }
    // Hook events themselves are still accepted on a Hook connection.
    let mut hook = Client::connect_as(&paths.socket, ClientRole::Hook).await;
    hook.send(claude(
        meta.id,
        ClaudeHookEvent::PreToolUse {
            tool_name: "Bash".into(),
        },
    ))
    .await;
    agent_state(
        &mut gui,
        meta.id,
        AgentState::ToolRunning {
            tool: "Bash".into(),
        },
    )
    .await;
    // The daemon kept running and the session is untouched.
    assert!(matches!(
        gui.request(Request::DaemonStatus).await,
        Event::Status(s) if s.sessions_live == 1
    ));
    assert_eq!(gui.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

/// Review #13: the other direction — GUI / CLI connections cannot inject
/// agent events (they could otherwise forge any session's state).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_hook_connections_may_send_hook_events() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let daemon = start_daemon(&paths);
    let mut gui = Client::connect(&paths.socket).await;
    let (_ws, meta) = workspace_and_shell(&mut gui, root.path()).await;
    let forged = || {
        claude(
            meta.id,
            ClaudeHookEvent::PreToolUse {
                tool_name: "Bash".into(),
            },
        )
    };
    for role in [ClientRole::Gui, ClientRole::Cli] {
        let mut c = Client::connect_as(&paths.socket, role).await;
        match c.request(forged()).await {
            Event::Error { message } => assert!(message.contains("Hook"), "{message}"),
            other => panic!("Hook from a {role:?} connection answered {other:?}"),
        }
        let closed = c.read_one(Instant::now() + Duration::from_secs(5)).await;
        assert!(closed.is_none(), "connection still open: {closed:?}");
    }
    // Nothing reached the state machine.
    let agent = session_meta(&mut gui, meta.id).await.agent;
    assert_eq!(agent.state, AgentState::Idle);
    assert_eq!(agent.kind, AgentKind::Shell);
    assert_eq!(gui.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

/// Review high #2: client-supplied FetchLines ranges cannot crash a session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_lines_with_absurd_range_is_harmless() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let daemon = start_daemon(&paths);
    let mut c = Client::connect(&paths.socket).await;
    let (_ws, meta) = workspace_and_shell(&mut c, root.path()).await;
    let sid = meta.id;
    for (start, count) in [(u64::MAX, u32::MAX), (u64::MAX - 1, 5), (0, u32::MAX)] {
        let req = Request::FetchLines {
            session: sid,
            start,
            count,
        };
        match c.request(req).await {
            Event::Lines { lines, .. } => {
                if start > 0 {
                    assert!(lines.is_empty(), "start={start}: {} lines", lines.len());
                } else {
                    assert_eq!(lines.len(), usize::from(DIMS.rows), "whole space");
                }
            }
            other => panic!("FetchLines({start}, {count}) answered {other:?}"),
        }
    }
    // The session survived: still live, attachable and responsive.
    let mut screen = attach(&mut c, sid).await;
    c.send(Request::Input {
        session: sid,
        data: b"echo ok-$$\n".to_vec(),
    })
    .await;
    c.screen_until(sid, &mut screen, "ok-<pid>", |s| s.has("ok-"))
        .await;
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

async fn session_meta(c: &mut Client, sid: SessionId) -> SessionMeta {
    match c.request(Request::ListSessions).await {
        Event::Sessions(all) => all.into_iter().find(|m| m.id == sid).expect("listed"),
        other => panic!("ListSessions answered {other:?}"),
    }
}

async fn all_text(c: &mut Client, sid: SessionId) -> Vec<String> {
    let req = Request::FetchLines {
        session: sid,
        start: 0,
        count: 1000,
    };
    match c.request(req).await {
        Event::Lines { lines, .. } => lines.iter().map(|l| l.text()).collect(),
        other => panic!("FetchLines answered {other:?}"),
    }
}

/// Review high #1 (b): `Revive { ResumeAgent }` executes the configured
/// resume argv itself. Typed into a shell, the command line would be echoed
/// and the shell would outlive the program; executed, only the program's
/// output appears and the session ends with it (Dormant, not stuck Live).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_agent_execs_the_resume_argv_without_a_shell() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let config = berth_daemon::Config::parse(
        "[agents.claude]\nresume_command = \"/bin/echo resumed-by {id}\"\n",
    )
    .unwrap();
    let daemon = start_daemon_with(&paths, config);
    let mut c = Client::connect(&paths.socket).await;
    let (_ws, meta) = workspace_and_shell(&mut c, root.path()).await;
    let sid = meta.id;
    let id = "0f8c2e1a-1111-2222-3333-444455556666";
    let mut hook = Client::connect_as(&paths.socket, ClientRole::Hook).await;
    hook.send(claude_as(sid, id, ClaudeHookEvent::UserPromptSubmit))
        .await;
    c.wait_for("external id", |m| {
        matches!(&m.event, Event::AgentChanged { session, agent }
            if *session == sid && agent.external_id.as_deref() == Some(id))
    })
    .await;
    assert_eq!(c.request(Request::Kill { session: sid }).await, Event::Ok);
    exited(&mut c, sid).await;

    match c
        .request(Request::Revive {
            session: sid,
            mode: ReviveMode::ResumeAgent,
        })
        .await
    {
        Event::SessionUpdated(m) => assert_eq!(m.agent.external_id.as_deref(), Some(id)),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        exited(&mut c, sid).await,
        Some(0),
        "the program is the session"
    );
    let text = all_text(&mut c, sid).await;
    let want = format!("resumed-by {id}");
    assert!(text.iter().any(|t| t.trim_end() == want), "{text:#?}");
    assert!(
        !text.iter().any(|t| t.contains("/bin/echo")),
        "the command line went through a shell: {text:#?}"
    );
    assert!(
        matches!(
            session_meta(&mut c, sid).await.status,
            SessionStatus::Dormant {
                exit_code: Some(0),
                ..
            }
        ),
        "a fast-exiting resume must leave the session Dormant"
    );
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

/// M3 review: after `/exit` (`SessionEnd`) the kind is `Shell` again, and
/// the agent that left (`last_agent`) is what `ResumeCommand` and `Revive {
/// ResumeAgent }` resume — across a daemon restart too. `Revive { Shell }`
/// of an agent session keeps the agent as `last_agent` the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agent_that_left_is_resumed_even_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let config = || {
        berth_daemon::Config::parse(
            "[agents.claude]\nresume_command = \"/bin/echo resumed-by {id}\"\n",
        )
        .unwrap()
    };
    let daemon = start_daemon_with(&paths, config());
    let mut c = Client::connect(&paths.socket).await;
    let (_ws, meta) = workspace_and_shell(&mut c, root.path()).await;
    let sid = meta.id;
    let id = "0f8c2e1a-1111-2222-3333-444455556666";
    let mut hook = Client::connect_as(&paths.socket, ClientRole::Hook).await;
    hook.send(claude_as(sid, id, ClaudeHookEvent::UserPromptSubmit))
        .await;
    let end = ClaudeHookEvent::SessionEnd {
        reason: Some("prompt_input_exit".into()),
    };
    hook.send(claude_as(sid, id, end)).await;
    c.wait_for("the agent left", |m| {
        matches!(&m.event, Event::AgentChanged { session, agent }
            if *session == sid
                && agent.kind == AgentKind::Shell
                && agent.last_agent == Some(AgentKind::Claude))
    })
    .await;
    let want = vec!["/bin/echo".to_string(), "resumed-by".into(), id.into()];
    match c.request(Request::ResumeCommand { session: sid }).await {
        Event::ResumeCommand { command, .. } => assert_eq!(command, Ok(want.clone())),
        other => panic!("{other:?}"),
    }
    assert_eq!(c.request(Request::Kill { session: sid }).await, Event::Ok);
    exited(&mut c, sid).await;
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;

    let daemon = start_daemon_with(&paths, config());
    let mut c = Client::connect(&paths.socket).await;
    let restored = session_meta(&mut c, sid).await;
    assert_eq!(restored.status, SessionStatus::Restored);
    assert_eq!(
        (
            restored.agent.kind,
            restored.agent.last_agent,
            restored.agent.external_id.as_deref()
        ),
        (AgentKind::Shell, Some(AgentKind::Claude), Some(id))
    );
    match c.request(Request::ResumeCommand { session: sid }).await {
        Event::ResumeCommand { command, .. } => assert_eq!(command, Ok(want)),
        other => panic!("{other:?}"),
    }
    let revive = |mode| Request::Revive { session: sid, mode };
    match c.request(revive(ReviveMode::ResumeAgent)).await {
        Event::SessionUpdated(m) => {
            assert_eq!(m.agent.kind, AgentKind::Claude, "the agent is back")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(exited(&mut c, sid).await, Some(0));
    let text = all_text(&mut c, sid).await;
    assert!(
        text.iter()
            .any(|t| t.trim_end() == format!("resumed-by {id}")),
        "{text:#?}"
    );

    // `Revive { Shell }` of a session whose agent never left (killed while
    // it ran, so `last_agent` is still empty) keeps it the same way.
    let root2 = tempfile::tempdir().unwrap();
    let (_ws2, meta2) = workspace_and_shell(&mut c, root2.path()).await;
    let sid2 = meta2.id;
    let mut hook = Client::connect_as(&paths.socket, ClientRole::Hook).await;
    hook.send(claude_as(sid2, id, ClaudeHookEvent::UserPromptSubmit))
        .await;
    c.wait_for("claude runs", |m| {
        matches!(&m.event, Event::AgentChanged { session, agent }
            if *session == sid2 && agent.kind == AgentKind::Claude && agent.last_agent.is_none())
    })
    .await;
    assert_eq!(c.request(Request::Kill { session: sid2 }).await, Event::Ok);
    exited(&mut c, sid2).await;
    let revive2 = Request::Revive {
        session: sid2,
        mode: ReviveMode::Shell,
    };
    match c.request(revive2).await {
        Event::SessionUpdated(m) => assert_eq!(
            (
                m.agent.kind,
                m.agent.last_agent,
                m.agent.external_id.as_deref()
            ),
            (AgentKind::Shell, Some(AgentKind::Claude), Some(id))
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(c.request(Request::Kill { session: sid2 }).await, Event::Ok);
    exited(&mut c, sid2).await;
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

/// A snapshot in format 1 (M1 / M2) as the berthd of main (c606246) wrote
/// it: `fixtures/format1-claude.bin.zst`, a session whose Claude state came
/// from simulated hooks (hand-written JSON through that build's berth-hook:
/// SessionStart, statusline, UserPromptSubmit), saved on SIGTERM. berth-store
/// refuses the format; the daemon reads the old layout, so the history and
/// the agent survive the upgrade.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_format_1_snapshot_of_an_older_berthd_is_restored() {
    // Written by the berthd of main c606246 (M2: snapshot format 1) for a
    // session with simulated Claude hooks.
    let file = include_bytes!("fixtures/format1-claude.bin.zst");
    let sid: SessionId = "f72f6b6f-0724-4534-b6d4-55efc64119e2".parse().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    paths.ensure_dirs().unwrap();
    std::fs::create_dir_all(&paths.snapshots_dir).unwrap();
    std::fs::write(paths.snapshot_file(&sid), file).unwrap();
    let id = "0f8c2e1a-1111-4222-8333-000000000f01";
    let meta = {
        let store = berth_store::Store::open(&paths).unwrap();
        // berth-store reads it into the current types.
        let old = store.read_snapshot(sid).unwrap().expect("the fixture");
        assert_eq!(old.format_version, 1);
        let meta = old.session;
        let a = &meta.agent;
        assert_eq!(
            (
                &a.kind,
                a.external_id.as_deref(),
                a.model.as_deref(),
                a.context_pct,
                a.cost_usd,
                &a.last_agent
            ),
            (
                &AgentKind::Claude,
                Some(id),
                Some("claude-opus-5-5"),
                Some(12.0),
                Some(0.04),
                &None
            )
        );
        assert_eq!(
            (a.state.clone(), a.source),
            (AgentState::Thinking, StateSource::Hook)
        );
        let transcript = format!("/tmp/berth-m3/v1fix/projects/-w/{id}.jsonl");
        assert_eq!(a.transcript_path.as_deref(), Some(Path::new(&transcript)));
        assert_eq!(
            (meta.title_user.as_deref(), meta.cols, meta.rows),
            (Some("v1-claude"), 90, 20)
        );
        let ws = Workspace {
            id: meta.workspace,
            name: "work".into(),
            root: meta.cwd.clone(),
            color: None,
            order: 0,
            created_at_ms: meta.created_at_ms,
        };
        store.upsert_workspace(&ws).unwrap();
        store.upsert_session(&meta).unwrap();
        meta
    };
    let daemon = start_daemon(&paths);
    let mut c = Client::connect(&paths.socket).await;
    let restored = session_meta(&mut c, meta.id).await;
    assert_eq!(restored.status, SessionStatus::Restored);
    assert_eq!(restored.agent.external_id.as_deref(), Some(id));
    let text = all_text(&mut c, meta.id).await;
    assert!(
        text.iter()
            .any(|t| t.trim_end() == "format-1 history marker"),
        "{text:#?}"
    );
    assert!(
        text.iter().any(|t| t.trim_end() == "hooks sent"),
        "{text:#?}"
    );
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

/// Review high #1 (a): an agent session id that is not a plain token
/// (here: Ctrl-U, which erases a typed line, then a command) is dropped at
/// the hook entry, so there is nothing to resume and nothing runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_agent_session_id_is_never_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let config = berth_daemon::Config::parse(
        "[agents.claude]\nresume_command = \"/bin/echo resumed-by {id}\"\n",
    )
    .unwrap();
    let daemon = start_daemon_with(&paths, config);
    let mut c = Client::connect(&paths.socket).await;
    let (_ws, meta) = workspace_and_shell(&mut c, root.path()).await;
    let sid = meta.id;
    let pwned = root.path().join("pwned");
    let hostile = format!("\u{15}touch {} #", pwned.display());
    let mut hook = Client::connect_as(&paths.socket, ClientRole::Hook).await;
    hook.send(claude_as(sid, &hostile, ClaudeHookEvent::UserPromptSubmit))
        .await;
    agent_state(&mut c, sid, AgentState::Thinking).await;
    let stored = session_meta(&mut c, sid).await.agent.external_id;
    assert_eq!(c.request(Request::Kill { session: sid }).await, Event::Ok);
    exited(&mut c, sid).await;

    let answer = c
        .request(Request::Revive {
            session: sid,
            mode: ReviveMode::ResumeAgent,
        })
        .await;
    if !matches!(answer, Event::Error { .. }) {
        // Show what the id would have done before failing.
        let deadline = Instant::now() + Duration::from_secs(3);
        while !pwned.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "ResumeAgent with a hostile id answered {answer:?}; injected command ran: {}",
            pwned.exists()
        );
    }
    assert!(!pwned.exists());
    assert_eq!(stored, None, "the hostile id was stored");
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

/// A session running `sh -c 'trap "" HUP; …; exec sleep 30'` (SIGHUP
/// ignored, input never read) whose writer is stuck in a write; returns its
/// id and the child's pid (`exec` keeps it; an ignored signal stays ignored
/// across `exec`).
async fn stuck_sighup_ignoring_session(
    c: &mut Client,
    ws: WorkspaceId,
    dir: &Path,
    name: &str,
) -> (SessionId, u32) {
    let pidfile = dir.join(name);
    let script = format!(
        "trap '' HUP; echo $$ > '{}'; exec /bin/sleep 30",
        pidfile.display()
    );
    let req = Request::CreateSession {
        workspace: ws,
        cwd: None,
        command: Some(vec!["/bin/sh".into(), "-c".into(), script]),
        title: None,
        dims: DIMS,
    };
    let sid = match c.request(req).await {
        Event::SessionUpdated(meta) => meta.id,
        other => panic!("{other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        let pid = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse().ok());
        if let Some(pid) = pid {
            break pid;
        }
        assert!(Instant::now() < deadline, "no pid file");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let chunk = [vec![b'x'; 63], vec![b'\n']].concat().repeat(1024);
    for _ in 0..20 {
        c.send(Request::Input {
            session: sid,
            data: chunk.clone(),
        })
        .await;
    }
    // In-order barrier; a refused Input means the queue is full, so the
    // writer is stuck in a write.
    let lines = c
        .request(Request::FetchLines {
            session: sid,
            start: 0,
            count: 1,
        })
        .await;
    assert!(matches!(lines, Event::Lines { .. }), "{lines:?}");
    let refused = c
        .backlog
        .iter()
        .filter(
            |m| matches!(&m.event, Event::Error { message } if message.starts_with("backpressure")),
        )
        .count();
    assert!(refused > 0, "the writer never got stuck");
    c.backlog
        .retain(|m| !matches!(m.event, Event::Error { .. }));
    (sid, pid)
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Review #15: with the writer stuck in a write the actor cannot use the
/// handle's own kill; a child that also ignores SIGHUP must still die (and
/// be reaped). `Kill` escalates to SIGKILL after `KILL_GRACE`; `Delete`
/// lets go of the PTY at once and leaves a watchdog that does the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stuck_writer_does_not_keep_a_sighup_ignoring_child_alive() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let daemon = start_daemon(&paths);
    let mut c = Client::connect(&paths.socket).await;
    let ws = match c
        .request(Request::CreateWorkspace {
            name: "w".into(),
            root: root.path().to_path_buf(),
        })
        .await
    {
        Event::WorkspaceUpdated(ws) => ws.id,
        other => panic!("{other:?}"),
    };
    let grace = berth_vt::KILL_GRACE;

    let (sid, _) = stuck_sighup_ignoring_session(&mut c, ws, root.path(), "kill.pid").await;
    let t = Instant::now();
    assert_eq!(c.request(Request::Kill { session: sid }).await, Event::Ok);
    assert_eq!(exited(&mut c, sid).await, Some(128 + 9), "SIGKILL");
    let took = t.elapsed();
    assert!(
        took >= grace && took <= grace + Duration::from_secs(1),
        "Kill took {took:?}"
    );

    let (sid, pid) = stuck_sighup_ignoring_session(&mut c, ws, root.path(), "delete.pid").await;
    let t = Instant::now();
    assert_eq!(c.request(Request::Delete { session: sid }).await, Event::Ok);
    assert!(t.elapsed() < grace, "Delete waited for the child");
    while alive(pid) {
        assert!(
            t.elapsed() <= grace + Duration::from_secs(1),
            "child {pid} still alive {:?} after Delete",
            t.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        t.elapsed() >= grace,
        "it ignores SIGHUP: only SIGKILL ends it"
    );
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

/// Review #15 (refines medium #7): PTY writes run on a writer thread per
/// session behind a 1 MiB queue, so a child that never reads its input
/// (`sleep 30`) cannot block its session. 4 MiB poured in: every Input the
/// queue cannot take gets its own backpressure error, the actor keeps
/// answering (`FetchLines`), and `Kill` ends the session within
/// `KILL_GRACE` + 1 s although the writer is stuck in a write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_that_does_not_read_cannot_block_its_session() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let daemon = start_daemon(&paths);
    let mut c = Client::connect(&paths.socket).await;
    let (_ws, meta) = workspace_and_command(&mut c, root.path(), &["/bin/sleep", "30"]).await;
    let sid = meta.id;
    // Short lines: a canonical-mode tty queues complete lines until its
    // input buffer is full, then blocks the writer. (A line longer than the
    // buffer would be discarded instead, and nothing would ever block.)
    let line: Vec<u8> = [vec![b'x'; 63], vec![b'\n']].concat();
    let chunk: Vec<u8> = line.repeat(1024);
    assert_eq!(chunk.len(), 64 * 1024);
    let mut inputs = HashSet::new();
    for _ in 0..64 {
        let id = c
            .send(Request::Input {
                session: sid,
                data: chunk.clone(),
            })
            .await;
        inputs.insert(id);
    }
    // Requests are handled in order: once this is answered, so is every
    // Input before it.
    let asked = Instant::now();
    let lines = c
        .request(Request::FetchLines {
            session: sid,
            start: 0,
            count: 10,
        })
        .await;
    assert!(
        matches!(lines, Event::Lines { session, .. } if session == sid),
        "{lines:?}"
    );
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "FetchLines took {:?}",
        asked.elapsed()
    );
    let mut refused = HashSet::new();
    for m in &c.backlog {
        if let Event::Error { message } = &m.event {
            let for_input = m.reply_to.filter(|id| inputs.contains(id));
            assert!(
                for_input.is_some() && message.starts_with("backpressure"),
                "unexpected error {:?}: {message}",
                m.reply_to
            );
            assert!(refused.insert(for_input), "two errors for one Input");
        }
    }
    // 16 chunks fill the 1 MiB queue (allow one more for a chunk the tty
    // took whole); each of the others was refused on its own.
    assert!(
        refused.len() >= 64 - 17,
        "only {} of 64 Inputs refused",
        refused.len()
    );

    let killed = Instant::now();
    assert_eq!(c.request(Request::Kill { session: sid }).await, Event::Ok);
    exited(&mut c, sid).await;
    let took = killed.elapsed();
    assert!(
        took <= berth_vt::KILL_GRACE + Duration::from_secs(1),
        "Kill took {took:?}"
    );
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

/// Claude's key for a project directory (`~/.claude/projects/<key>/`).
fn claude_project_key(dir: &Path) -> String {
    dir.to_string_lossy()
        .chars()
        .flat_map(|c| {
            let n = if c.is_ascii_alphanumeric() {
                0
            } else {
                c.len_utf16()
            };
            std::iter::repeat_n('-', n).chain((n == 0).then_some(c))
        })
        .collect()
}

/// A Claude hook event as `berth-hook claude` sends it, with cwd and
/// transcript path.
fn claude_at(
    sid: SessionId,
    session_id: &str,
    cwd: &Path,
    transcript: &Path,
    event: ClaudeHookEvent,
) -> Request {
    Request::Hook(HookEnvelope {
        berth_session: Some(sid),
        pid: std::process::id(),
        sent_at_ms: now_ms(),
        signal: AgentSignal::Claude(ClaudeHook {
            session_id: session_id.into(),
            cwd: Some(cwd.to_path_buf()),
            transcript_path: Some(transcript.to_path_buf()),
            permission_mode: Some("default".into()),
            event,
        }),
    })
}

async fn cwd_event(c: &mut Client, sid: SessionId, want: &Path) {
    c.wait_for(
        &format!("Cwd {}", want.display()),
        |m| matches!(&m.event, Event::Cwd { session, path } if *session == sid && path == want),
    )
    .await;
}

/// M3: `SessionStart` and `CwdChanged` move the session's cwd and tell the
/// clients (`Cwd`); `ListEvents` returns the recorded events newest first;
/// `ResumeCommand` shows what a resume would run, in the transcript's
/// project directory rather than the `cd` target.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claude_cwd_events_list_and_resume_preview() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let config = berth_daemon::Config::parse(
        "[agents.claude]\nresume_command = \"/bin/echo resumed-by {id}\"\n",
    )
    .unwrap();
    let daemon = start_daemon_with(&paths, config);
    let mut c = Client::connect(&paths.socket).await;
    let (_ws, meta) = workspace_and_shell(&mut c, root.path()).await;
    let sid = meta.id;
    let project = std::fs::canonicalize(root.path()).unwrap().join("proj");
    let sub = project.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let transcript = dir
        .path()
        .join("projects")
        .join(claude_project_key(&project))
        .join("t.jsonl");
    let id = "0f8c2e1a-1111-2222-3333-444455556666";
    let mut hook = Client::connect_as(&paths.socket, ClientRole::Hook).await;

    let start = ClaudeHookEvent::SessionStart {
        source: Some("startup".into()),
    };
    hook.send(claude_at(sid, id, &project, &transcript, start))
        .await;
    cwd_event(&mut c, sid, &project).await;
    let changed = ClaudeHookEvent::Other {
        hook_event_name: "CwdChanged".into(),
    };
    hook.send(claude_at(sid, id, &sub, &transcript, changed))
        .await;
    cwd_event(&mut c, sid, &sub).await;
    assert_eq!(session_meta(&mut c, sid).await.cwd, sub);

    let events = match c
        .request(Request::ListEvents {
            session: sid,
            limit: 10,
        })
        .await
    {
        Event::Events { session, events } => {
            assert_eq!(session, sid);
            events
        }
        other => panic!("ListEvents answered {other:?}"),
    };
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["hook:CwdChanged", "hook:SessionStart"],
        "{events:?}"
    );
    match c
        .request(Request::ListEvents {
            session: sid,
            limit: 1,
        })
        .await
    {
        Event::Events { events, .. } => assert_eq!(events.len(), 1),
        other => panic!("ListEvents answered {other:?}"),
    }

    assert_eq!(c.request(Request::Kill { session: sid }).await, Event::Ok);
    exited(&mut c, sid).await;
    match c.request(Request::ResumeCommand { session: sid }).await {
        Event::ResumeCommand {
            session,
            cwd,
            command,
        } => {
            assert_eq!(session, sid);
            assert_eq!(cwd, project, "resume starts where the transcript lives");
            let want: Vec<String> = ["/bin/echo", "resumed-by", id].map(String::from).into();
            assert_eq!(command, Ok(want));
        }
        other => panic!("ResumeCommand answered {other:?}"),
    }
    // Nothing ran: the session is still dormant.
    assert!(!session_meta(&mut c, sid).await.is_live());
    let unknown = SessionId::new();
    for req in [
        Request::ResumeCommand { session: unknown },
        Request::ListEvents {
            session: unknown,
            limit: 1,
        },
    ] {
        assert!(matches!(c.request(req).await, Event::Error { .. }));
    }
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}

/// What the daemon answers Attach / Input / Resize / Revive / Subscribe for
/// an archived session (DESIGN §17.1).
const ARCHIVED: &str = "已归档，先恢复";

fn is_refusal(e: &Event) -> bool {
    matches!(e, Event::Error { message } if message == ARCHIVED)
}

/// M4 (DESIGN §17.1 / §17.5): Archive / Unarchive over the protocol.
/// Archiving a live session kills it before the reply, which carries the
/// mark; other clients get it as `SessionUpdated`. Archived, the session
/// refuses Attach / Input / Resize / Subscribe / Revive, serves history,
/// events and metadata requests, and is left out of `DaemonStatus`. The
/// mark survives a restart (registry); Unarchive clears it and Revive,
/// Attach and Input work again below the old history.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_and_unarchive_over_the_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let daemon = start_daemon(&paths);
    let mut c = Client::connect(&paths.socket).await;
    let mut other = Client::connect(&paths.socket).await;
    let (_ws, meta) = workspace_and_shell(&mut c, root.path()).await;
    let sid = meta.id;
    let mut screen = attach(&mut c, sid).await;
    c.send(Request::Input {
        session: sid,
        data: b"echo before-$$\n".to_vec(),
    })
    .await;
    c.screen_until(sid, &mut screen, "before-<pid>", |s| s.has("before-"))
        .await;

    let t0 = now_ms();
    let archived = match c.request(Request::Archive { session: sid }).await {
        Event::SessionUpdated(m) => m,
        other => panic!("Archive answered {other:?}"),
    };
    let at = archived.archived_at_ms.expect("the reply carries the mark");
    assert!(at >= t0);
    assert!(
        matches!(archived.status, SessionStatus::Dormant { .. }),
        "killed before the mark: {:?}",
        archived.status
    );
    other
        .wait_for("archived broadcast", |m| {
            m.reply_to.is_none()
                && matches!(&m.event, Event::SessionUpdated(u) if u.id == sid && u.archived_at_ms == Some(at))
        })
        .await;

    for req in [
        Request::Attach {
            session: sid,
            dims: DIMS,
        },
        Request::Resize {
            session: sid,
            dims: DIMS,
        },
        Request::Subscribe {
            session: sid,
            mode: SubscribeMode::Full,
        },
        Request::Subscribe {
            session: sid,
            mode: SubscribeMode::Preview { rows: 3, max_hz: 4 },
        },
        Request::Revive {
            session: sid,
            mode: ReviveMode::Shell,
        },
        Request::Revive {
            session: sid,
            mode: ReviveMode::ResumeAgent,
        },
    ] {
        let what = format!("{req:?}");
        let answer = c.request(req).await;
        assert!(is_refusal(&answer), "{what} answered {answer:?}");
    }
    // Input is fire-and-forget: only a refusal is answered.
    let input = c
        .send(Request::Input {
            session: sid,
            data: b"echo x\n".to_vec(),
        })
        .await;
    let answer = c
        .wait_for("input refusal", |m| m.reply_to == Some(input))
        .await
        .event;
    assert!(is_refusal(&answer), "Input answered {answer:?}");

    let history = all_text(&mut c, sid).await;
    assert!(
        history.iter().any(|l| has_marker(l, "before-")),
        "history stays readable: {history:?}"
    );
    match c
        .request(Request::ListEvents {
            session: sid,
            limit: 5,
        })
        .await
    {
        Event::Events { events, .. } => assert_eq!(
            (events[0].kind.as_str(), events[0].detail.as_deref()),
            ("archive", Some("user"))
        ),
        other => panic!("ListEvents answered {other:?}"),
    }
    match c
        .request(Request::Rename {
            session: sid,
            title: Some("old work".into()),
        })
        .await
    {
        Event::SessionUpdated(m) => assert_eq!(m.archived_at_ms, Some(at)),
        other => panic!("Rename answered {other:?}"),
    }
    assert!(matches!(
        c.request(Request::MarkRead { session: sid }).await,
        Event::SessionUpdated(m) if m.archived_at_ms == Some(at)
    ));
    match c.request(Request::Archive { session: sid }).await {
        Event::SessionUpdated(m) => assert_eq!(m.archived_at_ms, Some(at), "idempotent"),
        other => panic!("Archive answered {other:?}"),
    }
    match c.request(Request::DaemonStatus).await {
        Event::Status(s) => assert_eq!((s.sessions_live, s.sessions_total), (0, 0)),
        other => panic!("{other:?}"),
    }

    // Restart on the same directory: the registry keeps the mark.
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
    {
        let store = berth_store::Store::open(&paths).unwrap();
        let stored = store.get_session(sid).unwrap().unwrap();
        assert_eq!(stored.archived_at_ms, Some(at));
    }
    let daemon = start_daemon(&paths);
    let mut c = Client::connect(&paths.socket).await;
    let restored = session_meta(&mut c, sid).await;
    assert_eq!(restored.archived_at_ms, Some(at));
    assert_eq!(restored.status, SessionStatus::Restored);
    assert_eq!(restored.title_user.as_deref(), Some("old work"));
    let answer = c
        .request(Request::Attach {
            session: sid,
            dims: DIMS,
        })
        .await;
    assert!(is_refusal(&answer), "still archived: {answer:?}");

    match c.request(Request::Unarchive { session: sid }).await {
        Event::SessionUpdated(m) => {
            assert_eq!(m.archived_at_ms, None);
            assert_eq!(m.status, SessionStatus::Restored);
        }
        other => panic!("Unarchive answered {other:?}"),
    }
    match c
        .request(Request::Revive {
            session: sid,
            mode: ReviveMode::Shell,
        })
        .await
    {
        Event::SessionUpdated(m) => assert_eq!(m.status, SessionStatus::Live),
        other => panic!("Revive answered {other:?}"),
    }
    let mut screen = attach(&mut c, sid).await;
    c.send(Request::Input {
        session: sid,
        data: b"echo after-$$\n".to_vec(),
    })
    .await;
    c.screen_until(sid, &mut screen, "after-<pid>", |s| s.has("after-"))
        .await;
    let all = all_text(&mut c, sid).await;
    let before = all.iter().position(|l| has_marker(l, "before-"));
    let after = all.iter().position(|l| has_marker(l, "after-"));
    assert!(
        before.is_some() && before < after,
        "old history precedes the revived shell: {all:?}"
    );
    assert_eq!(c.request(Request::Shutdown).await, Event::Ok);
    daemon.join().await;
}
