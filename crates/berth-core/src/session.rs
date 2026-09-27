//! Workspace and session metadata (persisted in SQLite, sent to clients).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::agent::AgentInfo;
use crate::ids::{SessionId, WorkspaceId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub root: PathBuf,
    pub color: Option<[u8; 3]>,
    pub order: u32,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    /// PTY alive in the daemon.
    #[default]
    Live,
    /// Child exited (or daemon lost it) but history is still in memory/disk.
    Dormant { exit_code: Option<i32>, at_ms: i64 },
    /// Loaded from a snapshot after daemon restart; history only.
    Restored,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistPolicy {
    pub snapshot: bool,
    pub journal: bool,
}

impl Default for PersistPolicy {
    fn default() -> Self {
        Self {
            snapshot: true,
            journal: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub workspace: WorkspaceId,
    /// Title from OSC 0/2 or the foreground process.
    pub title_auto: String,
    /// User override.
    pub title_user: Option<String>,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub status: SessionStatus,
    pub agent: AgentInfo,
    pub created_at_ms: i64,
    pub last_active_ms: i64,
    pub unread: bool,
    pub persist: PersistPolicy,
    pub order: u32,
    pub cols: u16,
    pub rows: u16,
    /// Archived sessions leave the workspace list but keep all data (DESIGN §17.1).
    /// Never set while the session is `Live`.
    #[serde(default)]
    pub archived_at_ms: Option<i64>,
}

impl SessionMeta {
    pub fn title(&self) -> &str {
        match &self.title_user {
            Some(t) if !t.is_empty() => t,
            _ if !self.title_auto.is_empty() => &self.title_auto,
            _ => self
                .cwd
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("session"),
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self.status, SessionStatus::Live)
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at_ms.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SQLite rows and format 2 snapshots hold the session as JSON: metadata
    /// stored before `archived_at_ms` existed reads as not archived.
    #[test]
    fn metadata_stored_before_archived_at_ms_reads_as_not_archived() {
        let archived = SessionMeta {
            title_auto: "claude".into(),
            status: SessionStatus::Restored,
            archived_at_ms: Some(1_700_000_000_123),
            ..SessionMeta::default()
        };
        assert!(archived.is_archived());
        let mut stored = serde_json::to_value(&archived).unwrap();
        assert!(stored
            .as_object_mut()
            .unwrap()
            .remove("archived_at_ms")
            .is_some());
        let read: SessionMeta = serde_json::from_value(stored).unwrap();
        assert_eq!(read.archived_at_ms, None);
        assert!(!read.is_archived());
        assert_eq!(read.title_auto, "claude");
        assert_eq!(read.status, SessionStatus::Restored);
    }
}
