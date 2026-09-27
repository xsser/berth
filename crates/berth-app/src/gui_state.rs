//! `<data dir>/gui-state.json` (DESIGN §17.3): the split layout and the
//! focused pane, restored when the GUI starts.
//!
//! Written after every layout change and at exit, atomically (a temporary
//! file in the same directory, flushed to disk, then `rename`), owner-only
//! like the rest of the data directory. The writes happen on a thread of
//! their own ([`Saver`]): the UI does not wait for the disk, except for the
//! last write when it quits. A missing file is a first start; an unreadable
//! or malformed one, or one of another `version`, is reported and ignored
//! (the next write replaces it). Session ids are the only content: sessions
//! that no longer exist are pruned by the controller once the list arrives.

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use berth_core::{Paths, SessionId};
use serde::{Deserialize, Serialize};

use crate::panes::PaneTree;

pub const FILE_NAME: &str = "gui-state.json";
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GuiState {
    pub version: u32,
    /// `None`: no pane (no session was shown).
    pub layout: Option<PaneTree>,
    pub focused: Option<SessionId>,
}

impl GuiState {
    pub fn new(layout: Option<PaneTree>, focused: Option<SessionId>) -> GuiState {
        GuiState {
            version: VERSION,
            layout,
            focused,
        }
    }
}

pub fn path(paths: &Paths) -> PathBuf {
    paths.data_dir.join(FILE_NAME)
}

/// The saved state; `Ok(None)` when there is none yet.
pub fn load(path: &Path) -> Result<Option<GuiState>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读取 {} 失败：{e}", path.display())),
    };
    let state: GuiState = serde_json::from_str(&text)
        .map_err(|e| format!("{} 格式无效，已忽略：{e}", path.display()))?;
    if state.version != VERSION {
        return Err(format!(
            "{} 的版本 {} 不认识（本版本 {VERSION}），已忽略",
            path.display(),
            state.version
        ));
    }
    Ok(Some(state))
}

/// Replace the file atomically: a private temp file, flushed to disk, then
/// renamed over it (a crash leaves the old layout or the new one).
pub fn save(path: &Path, state: &GuiState) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !dir.exists() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let json = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(FILE_NAME);
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&json)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Writes `gui-state.json` on a thread of its own: [`save`] flushes the file
/// to disk (`F_FULLFSYNC` on macOS: milliseconds on an idle disk, far more on
/// a busy one), which the UI must not wait for. States handed over while a
/// write is in progress are coalesced: only the newest is written next.
pub struct Saver {
    /// `None` once finishing.
    tx: Option<Sender<GuiState>>,
    /// The messages of failed writes, oldest first.
    errors: Receiver<String>,
    /// Disconnected once the thread has ended.
    ended: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl Saver {
    /// Write to `path`. After a failed write `wake` runs (on the saver's
    /// thread) and the message is in [`Self::take_error`].
    pub fn spawn(path: PathBuf, wake: impl Fn() + Send + 'static) -> std::io::Result<Saver> {
        Saver::spawn_with(
            move |state| {
                save(&path, state)
                    .map_err(|e| format!("无法保存分屏布局到 {}：{e}", path.display()))
            },
            wake,
        )
    }

    /// With `write` in place of [`save`].
    fn spawn_with(
        mut write: impl FnMut(&GuiState) -> Result<(), String> + Send + 'static,
        wake: impl Fn() + Send + 'static,
    ) -> std::io::Result<Saver> {
        let (tx, states) = mpsc::channel::<GuiState>();
        let (failed, errors) = mpsc::channel();
        let (ending, ended) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("berth-gui-state".into())
            .spawn(move || {
                // Dropped when the thread ends, however it ends.
                let _ending = ending;
                while let Ok(mut state) = states.recv() {
                    while let Ok(newer) = states.try_recv() {
                        state = newer;
                    }
                    if let Err(e) = write(&state) {
                        let _ = failed.send(e);
                        wake();
                    }
                }
            })?;
        Ok(Saver {
            tx: Some(tx),
            errors,
            ended,
            thread: Some(thread),
        })
    }

    /// Hand `state` over for writing; never waits.
    pub fn save(&self, state: GuiState) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(state);
        }
    }

    /// The message of a failed write, oldest first.
    pub fn take_error(&self) -> Option<String> {
        self.errors.try_recv().ok()
    }

    /// Write what was handed over, then end the thread, waiting `wait` at
    /// most. `false`: it was still writing then, and is left to finish on
    /// its own (or not, if the process ends first).
    pub fn finish(&mut self, wait: Duration) -> bool {
        self.tx = None;
        if let Err(RecvTimeoutError::Timeout) = self.ended.recv_timeout(wait) {
            return false;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panes::SplitDir;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    fn state() -> GuiState {
        let (a, b) = (SessionId::new(), SessionId::new());
        let mut t = PaneTree::Leaf(a);
        t.split(a, b, SplitDir::Down).unwrap();
        GuiState::new(Some(t), Some(b))
    }

    #[test]
    fn roundtrip_through_an_atomic_owner_only_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data").join(FILE_NAME);
        assert_eq!(load(&path), Ok(None), "missing file: first start");
        let s = state();
        save(&path, &s).unwrap();
        assert_eq!(load(&path), Ok(Some(s.clone())));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        // Replaced, and no temporary file is left behind.
        let empty = GuiState::new(None, None);
        save(&path, &empty).unwrap();
        assert_eq!(load(&path), Ok(Some(empty)));
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![FILE_NAME.to_string()]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\": 1"), "{text}");
    }

    /// A writer that waits for `gate` (a slow disk) and logs what it wrote.
    fn gated(
        started: Sender<()>,
        gate: Receiver<()>,
        log: Arc<std::sync::Mutex<Vec<GuiState>>>,
    ) -> impl FnMut(&GuiState) -> Result<(), String> + Send + 'static {
        move |s| {
            let _ = started.send(());
            let _ = gate.recv();
            log.lock().unwrap().push(s.clone());
            Ok(())
        }
    }

    #[test]
    fn states_handed_over_during_a_write_are_coalesced_into_the_last() {
        let written = Arc::default();
        let (started_tx, started) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let mut saver =
            Saver::spawn_with(gated(started_tx, gate, Arc::clone(&written)), || {}).unwrap();
        let states: Vec<GuiState> = (0..12).map(|_| state()).collect();
        saver.save(states[0].clone());
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        // Eleven ⌥⌘ presses while the first write is on the disk …
        for s in &states[1..] {
            saver.save(s.clone());
        }
        // … make one more write, of the last state (later writes do not wait).
        drop(release);
        assert!(saver.finish(Duration::from_secs(5)));
        let written = written.lock().unwrap();
        assert_eq!(*written, [states[0].clone(), states[11].clone()]);
        assert_eq!(saver.take_error(), None);
    }

    #[test]
    fn finishing_waits_until_the_last_state_is_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data").join(FILE_NAME);
        let target = path.clone();
        // A slow disk: every write takes 100 ms.
        let slow = move |s: &GuiState| {
            std::thread::sleep(Duration::from_millis(100));
            save(&target, s).map_err(|e| e.to_string())
        };
        let mut saver = Saver::spawn_with(slow, || {}).unwrap();
        let states: Vec<GuiState> = (0..5).map(|_| state()).collect();
        for s in &states {
            saver.save(s.clone());
        }
        assert!(saver.finish(Duration::from_secs(2)));
        assert_eq!(load(&path), Ok(Some(states[4].clone())));
        assert_eq!(saver.take_error(), None);
    }

    #[test]
    fn finishing_gives_up_on_a_write_that_does_not_end() {
        let written = Arc::default();
        let (started_tx, started) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let mut saver =
            Saver::spawn_with(gated(started_tx, gate, Arc::clone(&written)), || {}).unwrap();
        saver.save(state());
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let t = std::time::Instant::now();
        assert!(!saver.finish(Duration::from_millis(100)));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(written.lock().unwrap().is_empty());
        drop(release);
    }

    #[test]
    fn a_failed_write_is_reported_and_wakes_the_ui() {
        let dir = tempfile::tempdir().unwrap();
        // The data directory's place is taken by a file.
        std::fs::write(dir.path().join("data"), "").unwrap();
        let path = dir.path().join("data").join(FILE_NAME);
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&woken);
        let mut saver = Saver::spawn(path.clone(), move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
        .unwrap();
        saver.save(state());
        assert!(saver.finish(Duration::from_secs(2)));
        let e = saver.take_error().expect("an error");
        let expected = format!("无法保存分屏布局到 {}：", path.display());
        assert!(e.starts_with(&expected), "{e}");
        assert_eq!(saver.take_error(), None);
        assert_eq!(woken.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn malformed_or_foreign_files_are_reported_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "{not json").unwrap();
        assert!(load(&path).unwrap_err().contains("格式无效"));
        let mut s = state();
        s.version = 2;
        std::fs::write(&path, serde_json::to_string(&s).unwrap()).unwrap();
        assert!(load(&path).unwrap_err().contains("版本 2"));
        // The documented shape.
        let sid = SessionId::new();
        let json = format!(r#"{{"version":1,"layout":{{"leaf":"{sid}"}},"focused":"{sid}"}}"#);
        std::fs::write(&path, json).unwrap();
        let got = load(&path).unwrap().unwrap();
        assert_eq!(got.layout, Some(PaneTree::Leaf(sid)));
        assert_eq!(got.focused, Some(sid));
    }
}
