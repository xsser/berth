//! Persistence for the daemon.
//!
//! - `berth.sqlite3`: workspaces, sessions (`SessionMeta` fields), agent
//!   events timeline. WAL mode, `0600`.
//! - `snapshots/<sid>.bin.zst`: `SessionSnapshotFile` (postcard + zstd),
//!   written atomically (tmp + fsync + rename).
//! - `journals/<sid>/NNNN.log`: optional raw PTY bytes with timestamps,
//!   rotated at 64 MiB (`JournalWriter`).
//!
//! Nothing here stores prompt or tool-input text: `EventRecord.detail` is
//! limited to short identifiers (tool name, notification type).
#![forbid(unsafe_code)]

use std::path::PathBuf;

use berth_core::{Paths, SessionId, SessionMeta, SessionSnapshotFile, Workspace, WorkspaceId};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("codec: {0}")]
    Codec(String),
    #[error("snapshot format {found} unsupported (want {want})")]
    Format { found: u32, want: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventRecord {
    pub session: SessionId,
    pub at_ms: i64,
    /// e.g. `hook:PreToolUse`, `osc:133D`, `heuristic:idle`, `pty:exit`.
    pub kind: String,
    /// `AgentState::name()` after the transition.
    pub state: String,
    /// Short identifier only (tool name, exit code); never prompt text.
    pub detail: Option<String>,
}

pub struct Store {
    _private: (),
}

impl Store {
    /// Open (creating dirs `0700` and running migrations).
    pub fn open(_paths: &Paths) -> Result<Store, StoreError> {
        todo!("berth-store: Store::open")
    }

    pub fn list_workspaces(&self) -> Result<Vec<Workspace>, StoreError> {
        todo!("berth-store: list_workspaces")
    }
    pub fn upsert_workspace(&self, _ws: &Workspace) -> Result<(), StoreError> {
        todo!("berth-store: upsert_workspace")
    }
    pub fn delete_workspace(&self, _id: WorkspaceId) -> Result<(), StoreError> {
        todo!("berth-store: delete_workspace")
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionMeta>, StoreError> {
        todo!("berth-store: list_sessions")
    }
    pub fn upsert_session(&self, _s: &SessionMeta) -> Result<(), StoreError> {
        todo!("berth-store: upsert_session")
    }
    /// Remove metadata, events, snapshot and journal.
    pub fn purge_session(&self, _id: SessionId) -> Result<(), StoreError> {
        todo!("berth-store: purge_session")
    }

    pub fn record_event(&self, _e: &EventRecord) -> Result<(), StoreError> {
        todo!("berth-store: record_event")
    }
    pub fn list_events(&self, _session: SessionId, _limit: u32) -> Result<Vec<EventRecord>, StoreError> {
        todo!("berth-store: list_events")
    }

    pub fn write_snapshot(&self, _snap: &SessionSnapshotFile) -> Result<(), StoreError> {
        todo!("berth-store: write_snapshot")
    }
    pub fn read_snapshot(&self, _id: SessionId) -> Result<Option<SessionSnapshotFile>, StoreError> {
        todo!("berth-store: read_snapshot")
    }

    pub fn open_journal(&self, _id: SessionId) -> Result<JournalWriter, StoreError> {
        todo!("berth-store: open_journal")
    }

    pub fn paths(&self) -> &Paths {
        todo!("berth-store: paths")
    }
}

/// Append-only raw output log for one session.
pub struct JournalWriter {
    _dir: PathBuf,
}

impl JournalWriter {
    /// Append a chunk with a millisecond timestamp; rotates files at 64 MiB.
    pub fn append(&mut self, _at_ms: i64, _bytes: &[u8]) -> Result<(), StoreError> {
        todo!("berth-store: JournalWriter::append")
    }
    pub fn flush(&mut self) -> Result<(), StoreError> {
        todo!("berth-store: JournalWriter::flush")
    }
}
