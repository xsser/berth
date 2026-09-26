//! End-to-end: in-process `berth_daemon::run` on a temp dir, a real PTY
//! (`/bin/sh`), and a protocol client speaking the framed socket protocol.

use std::collections::{BTreeMap, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use berth_core::{
    decode_payload, encode_frame, now_ms, AgentSignal, AgentState, ClaudeHook, ClaudeHookEvent,
    ClientMsg, ClientRole, DaemonMsg, Dims, Event, FrameReader, HookEnvelope, LineSnapshot, Paths,
    Request, ReviveMode, ScreenUpdate, SessionId, SessionMeta, SessionStatus, SubscribeMode,
    WorkspaceId, PROTOCOL_VERSION,
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
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(berth_daemon::run(
        paths.clone(),
        berth_daemon::Config::default(),
        rx,
    ));
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
    Request::Hook(HookEnvelope {
        berth_session: Some(sid),
        pid: std::process::id(),
        sent_at_ms: now_ms(),
        signal: AgentSignal::Claude(ClaudeHook {
            session_id: "claude-e2e".into(),
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

    // D.2: hook events drive the agent state.
    assert_eq!(
        c.request(claude(
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
    assert_eq!(c.request(claude(sid, permission)).await, Event::Ok);
    agent_state(&mut c, sid, AgentState::WaitingPermission { tool: None }).await;
    assert_eq!(
        c.request(claude(
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
