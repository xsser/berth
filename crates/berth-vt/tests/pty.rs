//! Integration tests against real PTYs and child processes.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use berth_core::Color;
use berth_vt::{
    OscEvent, PromptMark, PtyHandle, PtyOutput, PtySpawn, Terminal, TerminalConfig, VtError,
};
use crossbeam_channel::{Receiver, RecvTimeoutError};

const TIMEOUT: Duration = Duration::from_secs(10);

fn spec(command: &[&str], cwd: &Path) -> PtySpawn {
    PtySpawn {
        command: command.iter().map(|s| s.to_string()).collect(),
        cwd: cwd.to_path_buf(),
        env: Vec::new(),
        cols: 80,
        rows: 24,
    }
}

fn spawn(spec: &PtySpawn) -> (PtyHandle, Receiver<PtyOutput>) {
    PtyHandle::spawn(spec).expect("spawn")
}

/// Output collected so far and whether EOF was seen.
struct Collected {
    bytes: Vec<u8>,
    eof: bool,
}

impl Collected {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// Read until `done(output)` holds, EOF, or the timeout.
fn read_until(rx: &Receiver<PtyOutput>, mut done: impl FnMut(&str) -> bool) -> Collected {
    let deadline = Instant::now() + TIMEOUT;
    let mut out = Collected {
        bytes: Vec::new(),
        eof: false,
    };
    while !done(&out.text()) {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(PtyOutput::Data(chunk)) => out.bytes.extend_from_slice(&chunk),
            Ok(PtyOutput::Eof) => {
                out.eof = true;
                break;
            }
            Err(RecvTimeoutError::Timeout) => panic!("timed out; output so far: {:?}", out.text()),
            Err(RecvTimeoutError::Disconnected) => panic!("reader disconnected without Eof"),
        }
    }
    out
}

fn read_to_eof(rx: &Receiver<PtyOutput>) -> Collected {
    read_until(rx, |_| false)
}

fn wait_exit(handle: &mut PtyHandle) -> Option<i32> {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if let Some(code) = handle.try_wait().expect("try_wait") {
            return Some(code);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

/// Whether `pid` still exists (a zombie counts as existing), via `ps`.
fn process_exists(pid: u32) -> bool {
    Command::new("ps")
        .args(["-o", "pid=", "-p", &pid.to_string()])
        .output()
        .map(|out| out.status.success() && !out.stdout.is_empty())
        .expect("run ps")
}

fn wait_until_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_exists(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

#[test]
fn echo_output_then_eof_then_exit_code() {
    let dir = tmp_dir();
    let (mut handle, rx) = spawn(&spec(&["/bin/sh", "-c", "echo hi; exit 3"], dir.path()));
    assert!(handle.child_pid() > 1);
    let out = read_to_eof(&rx);
    assert!(out.eof);
    assert!(out.text().contains("hi"), "{:?}", out.text());
    assert_eq!(wait_exit(&mut handle), Some(3));
    // Cached after reaping.
    assert_eq!(handle.try_wait().unwrap(), Some(3));
}

#[test]
fn foreground_process_name_and_cwd_then_kill() {
    let dir = tmp_dir();
    let (mut handle, rx) = spawn(&spec(&["/bin/sleep", "5"], dir.path()));
    // `spawn` may return before the child has exec'd; until then the
    // foreground process is unknown (None), never the test binary itself.
    let deadline = Instant::now() + TIMEOUT;
    let info = loop {
        match handle.foreground_process() {
            Some(info) => break info,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            None => panic!("no foreground process"),
        }
    };
    assert!(info.name.ends_with("sleep"), "{info:?}");
    assert_eq!(info.pid, handle.child_pid());
    let expected_cwd: PathBuf = dir.path().canonicalize().unwrap();
    assert_eq!(info.cwd.as_deref(), Some(expected_cwd.as_path()));

    handle.kill().expect("kill");
    assert_eq!(wait_exit(&mut handle), Some(128 + libc::SIGHUP));
    assert!(read_to_eof(&rx).eof);
    // Signalling an already reaped child is a no-op, not an error.
    handle.kill().expect("kill after exit");
    assert_eq!(handle.foreground_process(), None);
}

#[test]
fn foreground_process_follows_the_shells_job() {
    let dir = tmp_dir();
    // An interactive shell puts the job in its own foreground process group.
    let (mut handle, rx) = spawn(&spec(&["/bin/sh", "-i"], dir.path()));
    handle.write(b"/bin/sleep 7\n").unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let info = loop {
        match handle.foreground_process() {
            Some(info) if info.name.ends_with("sleep") => break info,
            _ if Instant::now() > deadline => panic!("sleep never became the foreground job"),
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    assert_ne!(info.pid, handle.child_pid());
    // SIGHUP reaches both the shell's group and the foreground job's group.
    handle.kill().unwrap();
    assert!(
        wait_until_gone(info.pid, Duration::from_secs(5)),
        "sleep job survived"
    );
    assert!(wait_exit(&mut handle).is_some());
    drop(rx);
}

#[test]
fn write_reaches_the_child() {
    let dir = tmp_dir();
    let (mut handle, rx) = spawn(&spec(
        &["/bin/sh", "-c", "read line; echo \"got:$line\""],
        dir.path(),
    ));
    handle.write(b"hello\n").unwrap();
    let out = read_to_eof(&rx);
    assert!(out.text().contains("got:hello"), "{:?}", out.text());
    assert_eq!(wait_exit(&mut handle), Some(0));
}

#[test]
fn resize_is_visible_to_the_child() {
    let dir = tmp_dir();
    let (mut handle, rx) = spawn(&spec(&["/bin/sh", "-c", "read _; stty size"], dir.path()));
    handle.resize(100, 30).unwrap();
    handle.write(b"\n").unwrap();
    let out = read_to_eof(&rx);
    assert!(out.text().contains("30 100"), "{:?}", out.text());
    assert_eq!(wait_exit(&mut handle), Some(0));
}

#[test]
fn environment_and_cwd_are_applied() {
    let dir = tmp_dir();
    let mut spec = spec(
        &[
            "/bin/sh",
            "-c",
            "printf '%s|%s|%s|' \"$TERM\" \"$COLORTERM\" \"$BERTH_TEST\"; pwd -P",
        ],
        dir.path(),
    );
    spec.env.push(("BERTH_TEST".into(), "42".into()));
    let (mut handle, rx) = spawn(&spec);
    let out = read_to_eof(&rx);
    let expected = format!(
        "xterm-256color|truecolor|42|{}",
        dir.path().canonicalize().unwrap().display()
    );
    assert!(
        out.text().contains(&expected),
        "{:?} vs {expected:?}",
        out.text()
    );
    assert_eq!(wait_exit(&mut handle), Some(0));
}

#[test]
fn spawn_failure_is_reported() {
    let dir = tmp_dir();
    match PtyHandle::spawn(&spec(&["/nonexistent/berth-test-binary"], dir.path())) {
        Err(VtError::Pty(message)) => assert!(message.contains("spawn"), "{message}"),
        other => panic!("expected a pty error, got {other:?}"),
    }
}

#[test]
fn force_kill_escalates_when_sighup_is_ignored() {
    let dir = tmp_dir();
    let (mut handle, rx) = spawn(&spec(
        &["/bin/sh", "-c", "trap '' HUP; echo ready; /bin/sleep 30"],
        dir.path(),
    ));
    read_until(&rx, |text| text.contains("ready"));
    handle.kill().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(handle.try_wait().unwrap(), None, "SIGHUP should be ignored");
    handle.force_kill().unwrap();
    assert_eq!(wait_exit(&mut handle), Some(128 + libc::SIGKILL));
    assert!(read_to_eof(&rx).eof);
}

#[test]
fn dropping_the_handle_reaps_the_child() {
    let dir = tmp_dir();
    let (handle, rx) = spawn(&spec(&["/bin/sleep", "30"], dir.path()));
    let pid = handle.child_pid();
    assert!(process_exists(pid));
    drop(handle);
    assert!(
        wait_until_gone(pid, Duration::from_secs(5)),
        "child {pid} not reaped"
    );
    drop(rx);
}

#[test]
fn dropping_the_handle_escalates_to_sigkill() {
    let dir = tmp_dir();
    let (handle, rx) = spawn(&spec(
        &["/bin/sh", "-c", "trap '' HUP; echo ready; /bin/sleep 30"],
        dir.path(),
    ));
    read_until(&rx, |text| text.contains("ready"));
    let pid = handle.child_pid();
    drop(handle);
    assert!(
        wait_until_gone(pid, berth_vt::KILL_GRACE + Duration::from_secs(5)),
        "HUP-ignoring child {pid} not killed"
    );
}

#[test]
fn pty_output_drives_a_terminal() {
    let dir = tmp_dir();
    let script = r#"printf '\033]7;file://host/tmp/berth%%20x\007\033]133;D;5\007'; printf 'plain \033[1;32mOK\033[0m\n'; printf '\033]0;my title\007'"#;
    let (mut handle, rx) = spawn(&spec(&["/bin/sh", "-c", script], dir.path()));
    let mut terminal = Terminal::new(TerminalConfig::default());
    let mut osc = Vec::new();
    let mut titles = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(PtyOutput::Data(chunk)) => {
                let outcome = terminal.process(&chunk);
                osc.extend(outcome.osc);
                titles.extend(outcome.events.into_iter().filter_map(|event| match event {
                    berth_vt::TermEvent::Title(title) => Some(title),
                    _ => None,
                }));
            }
            Ok(PtyOutput::Eof) => break,
            Err(err) => panic!("no EOF: {err:?}"),
        }
    }
    assert_eq!(wait_exit(&mut handle), Some(0));
    assert_eq!(
        osc,
        vec![
            OscEvent::Cwd(PathBuf::from("/tmp/berth x")),
            OscEvent::Prompt(PromptMark::CommandEnd { exit_code: Some(5) }),
        ]
    );
    assert_eq!(titles, vec![Some("my title".to_string())]);
    let screen = terminal.screen();
    assert_eq!(screen.lines[0].text(), "plain OK");
    let ok_run = screen.lines[0].runs.last().unwrap();
    let style = terminal.interner().table().get(ok_run.style);
    assert_eq!(style.fg, Color::Indexed(2));
    assert_eq!(screen.title, "my title");
}
