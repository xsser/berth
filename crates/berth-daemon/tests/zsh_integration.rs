//! zsh shell integration end to end: the real `berthd` binary runs with a
//! cleared environment (temporary HOME, data dir and socket; no ZDOTDIR),
//! so every zsh it starts reads only the fixture's startup files — never
//! the user's, nor their history file. Skipped where `/bin/zsh` is missing.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use berth_core::{
    decode_payload, encode_frame, AgentState, ClientMsg, ClientRole, DaemonMsg, Dims, Event,
    FrameReader, Request, SessionId, StateSource, PROTOCOL_VERSION,
};

const WAIT: Duration = Duration::from_secs(20);
const ZSH: &str = "/bin/zsh";

struct Berthd {
    child: Child,
    socket: PathBuf,
}

impl Berthd {
    fn start(data: &Path, home: &Path, config: &str) -> Berthd {
        Berthd::start_with(data, home, config, None)
    }

    fn start_with(data: &Path, home: &Path, config: &str, zdotdir: Option<&Path>) -> Berthd {
        std::fs::write(data.join("config.toml"), config).unwrap();
        let log = std::fs::File::create(data.join("berthd.stderr")).unwrap();
        let socket = data.join("s.sock");
        let user = std::env::var("USER").unwrap_or_else(|_| "berth-test".into());
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_berthd"));
        cmd.env_clear();
        if let Some(dir) = zdotdir {
            cmd.env("ZDOTDIR", dir);
        }
        let child = cmd
            .arg("--foreground")
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("SHELL", ZSH)
            .env("USER", &user)
            .env("LOGNAME", &user)
            .env("LANG", "en_US.UTF-8")
            .env("BERTH_DATA_DIR", data)
            .env("BERTH_SOCKET", &socket)
            .env("BERTH_CONFIG", data.join("config.toml"))
            .env("BERTH_LOG", "debug")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("start berthd");
        Berthd { child, socket }
    }

    fn stop(mut self, c: &mut Client) {
        assert_eq!(c.request(Request::Shutdown), Event::Ok);
        let deadline = Instant::now() + WAIT;
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "berthd did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Berthd {
    fn drop(&mut self) {
        // Only this test's own daemon (its pid), if it is still running.
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct Client {
    stream: UnixStream,
    frames: FrameReader,
    backlog: VecDeque<DaemonMsg>,
    next_id: u32,
}

impl Client {
    fn connect(socket: &Path) -> Client {
        let deadline = Instant::now() + WAIT;
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(s) => break s,
                Err(e) => {
                    assert!(Instant::now() < deadline, "cannot connect: {e}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut c = Client {
            stream,
            frames: FrameReader::new(),
            backlog: VecDeque::new(),
            next_id: 1,
        };
        let hello = c.request(Request::Hello {
            role: ClientRole::Gui,
            protocol: PROTOCOL_VERSION,
            client_version: "zsh-e2e".into(),
        });
        assert!(matches!(hello, Event::Hello { .. }), "{hello:?}");
        c
    }

    fn send(&mut self, req: Request) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let frame = encode_frame(&ClientMsg { id, req }).unwrap();
        self.stream.write_all(&frame).unwrap();
        id
    }

    fn wait_for(&mut self, what: &str, mut pred: impl FnMut(&DaemonMsg) -> bool) -> DaemonMsg {
        if let Some(pos) = self.backlog.iter().position(&mut pred) {
            return self.backlog.remove(pos).unwrap();
        }
        let deadline = Instant::now() + WAIT;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            while let Some(frame) = self.frames.next_frame().unwrap() {
                let msg: DaemonMsg = decode_payload(&frame).unwrap();
                if pred(&msg) {
                    return msg;
                }
                self.backlog.push_back(msg);
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            match self.stream.read(&mut buf) {
                Ok(0) => panic!("daemon closed the connection waiting for {what}"),
                Ok(n) => self.frames.push(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("read: {e}"),
            }
        }
    }

    fn request(&mut self, req: Request) -> Event {
        let id = self.send(req);
        self.wait_for("reply", |m| m.reply_to == Some(id)).event
    }

    fn type_line(&mut self, sid: SessionId, line: &str) {
        let data = format!("{line}\r").into_bytes();
        let id = self.send(Request::Input { session: sid, data });
        // Input is answered only on error.
        let _ = id;
    }
}

fn new_session(c: &mut Client, root: &Path, command: Option<Vec<String>>) -> SessionId {
    let ws = match c.request(Request::CreateWorkspace {
        name: "zsh".into(),
        root: root.to_path_buf(),
    }) {
        Event::WorkspaceUpdated(ws) => ws.id,
        other => panic!("{other:?}"),
    };
    match c.request(Request::CreateSession {
        workspace: ws,
        cwd: None,
        command,
        title: None,
        dims: Dims {
            cols: 100,
            rows: 30,
        },
    }) {
        Event::SessionUpdated(meta) => meta.id,
        other => panic!("{other:?}"),
    }
}

fn wait_file(path: &Path) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Startup files of a user with an alias in `.zshenv`, a prompt, aliases,
/// options, a function and a precmd hook in `.zshrc`.
fn fixture_home(home: &Path) {
    std::fs::create_dir_all(home.join("proj/sub")).unwrap();
    fixture_rc(home);
}

fn fixture_rc(home: &Path) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join(".zshenv"),
        "typeset -g FIX_ENV=\"env:${0:t}\"\nalias fxenv='echo from-zshenv'\n",
    )
    .unwrap();
    std::fs::write(
        home.join(".zshrc"),
        "PROMPT='fixture %~ %# '\nRPROMPT='[%?]'\nalias ll='ls -l'\n\
         setopt autocd extendedglob\nfx_func() { echo fx; }\n\
         fx_precmd() { FIX_LAST=$? }\nprecmd_functions+=(fx_precmd)\n",
    )
    .unwrap();
    std::fs::write(home.join(".zprofile"), "FIX_PROFILE=1\n").unwrap();
    std::fs::write(home.join(".zlogin"), "FIX_LOGIN=1\n").unwrap();
}

/// What a user can see of their configuration, written to `out`.
fn probe(c: &mut Client, sid: SessionId, out: &Path) {
    std::fs::create_dir_all(out).unwrap();
    let o = out.display();
    for line in [
        format!("alias > {o}/alias"),
        format!("print -r -- \"$PROMPT|$RPROMPT\" > {o}/prompt"),
        format!("print -l ${{(ok)functions}} > {o}/functions"),
        format!("setopt > {o}/setopt"),
        format!(
            "print -r -- \"$FIX_ENV|$FIX_PROFILE|$FIX_LOGIN|${{ZDOTDIR-unset}}|\
             ${{BERTH_ORIG_ZDOTDIR-unset}}|$HISTFILE\" > {o}/vars"
        ),
        format!("print -l $precmd_functions > {o}/precmd"),
        format!("print -l $preexec_functions > {o}/preexec"),
        format!("touch {o}/done"),
    ] {
        c.type_line(sid, &line);
    }
    wait_file(&out.join("done"));
}

fn read(out: &Path, name: &str) -> String {
    std::fs::read_to_string(out.join(name)).unwrap()
}

fn without_berth(lines: &str) -> Vec<&str> {
    lines
        .lines()
        .filter(|l| !l.starts_with("__berth_"))
        .collect()
}

/// `shell_integration = "auto"`: the user's configuration is untouched
/// (compared with the same zsh started without the integration), and the
/// daemon receives OSC 7 (`cd` moves the session's cwd) and OSC 133
/// (running command → Thinking, prompt → Idle, from shell integration).
#[test]
fn zsh_integration_is_transparent_and_reports_cwd_and_commands() {
    if !Path::new(ZSH).exists() {
        eprintln!("skipped: {ZSH} not found");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let home_dir = tempfile::tempdir().unwrap();
    let home = home_dir.path();
    fixture_home(home);
    let daemon = Berthd::start(data.path(), home, "");
    let mut c = Client::connect(&daemon.socket);

    // Without the integration: a program other than zsh starts it.
    let base = new_session(
        &mut c,
        &home.join("proj"),
        Some(vec!["/usr/bin/env".into(), ZSH.into(), "-l".into()]),
    );
    probe(&mut c, base, &home.join("out-base"));
    // The login shell (SHELL=/bin/zsh), with the integration.
    let sid = new_session(&mut c, &home.join("proj"), None);
    probe(&mut c, sid, &home.join("out-shim"));

    let (b, s) = (home.join("out-base"), home.join("out-shim"));
    for name in ["alias", "prompt", "setopt", "vars"] {
        assert_eq!(read(&b, name), read(&s, name), "{name} differs");
    }
    assert!(read(&s, "alias").contains("ll='ls -l'"));
    assert!(read(&s, "vars").starts_with("env:zsh|1|1|unset|unset|"));
    assert_eq!(read(&s, "prompt"), "fixture %~ %# |[%?]\n");
    assert_eq!(
        without_berth(&read(&b, "functions")),
        without_berth(&read(&s, "functions"))
    );
    assert!(read(&s, "functions").contains("__berth_precmd"));
    assert!(!read(&b, "functions").contains("__berth_"));
    // Hooks go after the user's own.
    assert_eq!(read(&s, "precmd"), "fx_precmd\n__berth_precmd\n");
    assert_eq!(read(&s, "preexec"), "__berth_preexec\n");

    // OSC 7: `cd` moves the session's cwd.
    let sub = std::fs::canonicalize(home.join("proj/sub")).unwrap();
    c.type_line(sid, "cd sub");
    c.wait_for("Cwd proj/sub", |m| {
        matches!(&m.event, Event::Cwd { session, path }
            if *session == sid && std::fs::canonicalize(path).ok().as_deref() == Some(&sub))
    });
    // OSC 133: a running command is Thinking (shown as running), its end
    // Idle again, both from shell integration. Earlier commands' events
    // (the probes, `cd`) came before `Cwd`: forget them.
    c.backlog.clear();
    c.type_line(sid, "sleep 1; false");
    for want in [AgentState::Thinking, AgentState::Idle] {
        c.wait_for(&format!("{want:?} from shell integration"), |m| {
            matches!(&m.event, Event::AgentChanged { session, agent }
                if *session == sid && agent.state == want
                    && agent.source == StateSource::ShellIntegration)
        });
    }
    let events = match c.request(Request::ListEvents {
        session: sid,
        limit: 20,
    }) {
        Event::Events { events, .. } => events,
        other => panic!("{other:?}"),
    };
    // Newest first: `sleep 1; false` ended with status 1.
    let last_end = events.iter().find(|e| e.kind == "osc:133D").expect("133;D");
    assert_eq!(last_end.detail.as_deref(), Some("1"), "{events:?}");
    assert!(events.iter().any(|e| e.kind == "osc:133C"), "{events:?}");
    // The baseline shell never reported anything.
    match c.request(Request::ListEvents {
        session: base,
        limit: 20,
    }) {
        Event::Events { events, .. } => {
            assert!(
                events.iter().all(|e| !e.kind.starts_with("osc:")),
                "{events:?}"
            )
        }
        other => panic!("{other:?}"),
    }
    daemon.stop(&mut c);
}

/// `shell_integration = "none"`: the login zsh starts as without berth.
#[test]
fn zsh_integration_off_leaves_zsh_alone() {
    if !Path::new(ZSH).exists() {
        eprintln!("skipped: {ZSH} not found");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let home_dir = tempfile::tempdir().unwrap();
    let home = home_dir.path();
    fixture_home(home);
    let daemon = Berthd::start(
        data.path(),
        home,
        "[terminal]\nshell_integration = \"none\"\n",
    );
    let mut c = Client::connect(&daemon.socket);
    let sid = new_session(&mut c, &home.join("proj"), None);
    let out = home.join("out-none");
    probe(&mut c, sid, &out);
    assert!(!read(&out, "functions").contains("__berth_"));
    assert!(read(&out, "vars").starts_with("env:zsh|1|1|unset|unset|"));
    assert!(!data.path().join("shell-integration").exists());
    daemon.stop(&mut c);
}

/// A user whose startup files live in `$ZDOTDIR` (inherited by berthd):
/// the integrated shell reads those, never `$HOME`'s, and keeps ZDOTDIR.
#[test]
fn zsh_integration_honours_the_users_zdotdir() {
    if !Path::new(ZSH).exists() {
        eprintln!("skipped: {ZSH} not found");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let home_dir = tempfile::tempdir().unwrap();
    let home = home_dir.path();
    std::fs::create_dir_all(home.join("proj")).unwrap();
    let zdot = home.join("zdot");
    fixture_rc(&zdot);
    // Decoys: read only if ZDOTDIR were lost.
    std::fs::write(home.join(".zshenv"), "FIX_ENV=wrong-home\n").unwrap();
    std::fs::write(home.join(".zshrc"), "PROMPT='wrong-home '\n").unwrap();
    let daemon = Berthd::start_with(data.path(), home, "", Some(&zdot));
    let mut c = Client::connect(&daemon.socket);
    let base = new_session(
        &mut c,
        &home.join("proj"),
        Some(vec!["/usr/bin/env".into(), ZSH.into(), "-l".into()]),
    );
    probe(&mut c, base, &home.join("out-base"));
    let sid = new_session(&mut c, &home.join("proj"), None);
    probe(&mut c, sid, &home.join("out-shim"));
    let (b, s) = (home.join("out-base"), home.join("out-shim"));
    for name in ["alias", "prompt", "setopt", "vars"] {
        assert_eq!(read(&b, name), read(&s, name), "{name} differs");
    }
    assert_eq!(read(&s, "prompt"), "fixture %~ %# |[%?]\n");
    let want = format!(
        "env:zsh|1|1|{}|unset|{}/.zsh_history\n",
        zdot.display(),
        zdot.display()
    );
    assert_eq!(read(&s, "vars"), want);
    assert!(read(&s, "functions").contains("__berth_precmd"));
    daemon.stop(&mut c);
}

/// Review medium: a zsh started inside the session (here `zsh -i`) sees the
/// user's ZDOTDIR and starts without berth. `BERTH_SHELL_INTEGRATION` names
/// the integration script: with the documented line in the user's
/// `.zshrc` the nested shell loads it and its commands report OSC 133
/// again, while the session's own shell, which has it already, still runs
/// its hooks once.
#[test]
fn a_nested_zsh_loads_the_integration_from_the_users_zshrc() {
    if !Path::new(ZSH).exists() {
        eprintln!("skipped: {ZSH} not found");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let home_dir = tempfile::tempdir().unwrap();
    let home = home_dir.path();
    fixture_home(home);
    let mut rc = std::fs::OpenOptions::new()
        .append(true)
        .open(home.join(".zshrc"))
        .unwrap();
    rc.write_all(
        b"[[ -n $BERTH_SHELL_INTEGRATION ]] && source \"$BERTH_SHELL_INTEGRATION\"\n\
          [[ -n $NESTED_MARK ]] && : > $NESTED_MARK\n",
    )
    .unwrap();
    let daemon = Berthd::start(data.path(), home, "");
    let mut c = Client::connect(&daemon.socket);
    let sid = new_session(&mut c, &home.join("proj"), None);
    let out = home.join("out-nested");
    std::fs::create_dir_all(&out).unwrap();
    let o = out.display();
    c.type_line(
        sid,
        &format!(
            "print -r -- \"$BERTH_SHELL_INTEGRATION\" > {o}/var; \
             print -l $precmd_functions > {o}/outer; touch {o}/outer-done"
        ),
    );
    wait_file(&out.join("outer-done"));
    let script = data
        .path()
        .join("shell-integration/zsh/berth-integration.zsh");
    let script = std::fs::canonicalize(script).unwrap();
    assert_eq!(read(&out, "var"), format!("{}\n", script.display()));
    assert_eq!(read(&out, "outer"), "fx_precmd\n__berth_precmd\n");

    c.type_line(sid, &format!("NESTED_MARK={o}/nested-up {ZSH} -i"));
    wait_file(&out.join("nested-up"));
    c.type_line(
        sid,
        &format!("print -l $precmd_functions > {o}/inner; touch {o}/inner-done"),
    );
    wait_file(&out.join("inner-done"));
    assert_eq!(read(&out, "inner"), "fx_precmd\n__berth_precmd\n");

    // A command of the nested shell reports OSC 133 again: `false` is the
    // only command here ending with status 1 (the session's own shell is
    // still running `zsh -i`).
    c.type_line(sid, "sleep 1; false");
    let deadline = Instant::now() + WAIT;
    loop {
        let events = match c.request(Request::ListEvents {
            session: sid,
            limit: 20,
        }) {
            Event::Events { events, .. } => events,
            other => panic!("{other:?}"),
        };
        if events
            .iter()
            .any(|e| e.kind == "osc:133D" && e.detail.as_deref() == Some("1"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "no 133;D;1: {events:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
    c.type_line(sid, "exit");
    daemon.stop(&mut c);
}
