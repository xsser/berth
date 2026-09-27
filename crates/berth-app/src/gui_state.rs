//! `<data dir>/gui-state.json` (DESIGN §17.3): the split layout and the
//! focused pane, restored when the GUI starts.
//!
//! Written after every layout change and at exit, atomically (a temporary
//! file in the same directory, then `rename`), owner-only like the rest of
//! the data directory. A missing file is a first start; an unreadable or
//! malformed one, or one of another `version`, is reported and ignored (the
//! next write replaces it). Session ids are the only content: sessions that
//! no longer exist are pruned by the controller once the list arrives.

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panes::SplitDir;
    use std::os::unix::fs::PermissionsExt;

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
