//! Persistence for the daemon.
//!
//! - `berth.sqlite3`: workspaces, sessions (full `SessionMeta` as JSON plus a
//!   few indexed columns), agent events timeline. WAL mode, `0600`.
//! - `snapshots/<sid>.bin.zst`: `SessionSnapshotFile` (postcard + zstd level
//!   3), written atomically (`.tmp` + fsync + rename).
//! - `journals/<sid>/NNNN.log`: optional raw PTY bytes with timestamps,
//!   rotated at 64 MiB (`JournalWriter`).
//!
//! Nothing here stores prompt or tool-input text: `EventRecord.detail` is
//! limited to short identifiers (tool name, notification type, exit code).
#![forbid(unsafe_code)]

mod fsutil;
mod journal;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use berth_core::{
    Paths, SessionId, SessionMeta, SessionSnapshotFile, SessionStatus, Workspace, WorkspaceId,
    SNAPSHOT_FORMAT_VERSION,
};
use rusqlite::{params, Connection, OptionalExtension};

pub use journal::{read_journal, JournalRecord, JournalWriter, JOURNAL_ROTATE_BYTES};

/// Current SQLite schema version.
pub const SCHEMA_VERSION: i64 = 1;
/// zstd level for snapshots (DESIGN §6).
pub const SNAPSHOT_ZSTD_LEVEL: i32 = 3;
/// Upper bound for `EventRecord.detail` (defence against callers passing
/// free text; details are meant to be identifiers).
pub const MAX_EVENT_DETAIL: usize = 128;

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
    #[error("database schema {found} is newer than supported {want}")]
    Schema { found: i64, want: i64 },
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

/// Thread-safe handle: SQLite access is serialized by an internal mutex,
/// snapshot and journal I/O never take it.
pub struct Store {
    conn: Mutex<Connection>,
    paths: Paths,
}

impl Store {
    /// Open (creating dirs `0700`, the database `0600`, running migrations).
    pub fn open(paths: &Paths) -> Result<Store, StoreError> {
        paths.ensure_dirs()?;
        // SQLite creates -wal/-shm files with the main database's mode, so
        // creating the database file ourselves with 0600 covers all three.
        fsutil::ensure_private_file(&paths.db)?;
        let conn = Connection::open(&paths.db)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(%mode, "sqlite refused WAL journal mode");
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&conn)?;
        for suffix in ["-wal", "-shm"] {
            let mut p = paths.db.clone().into_os_string();
            p.push(suffix);
            fsutil::restrict_if_exists(Path::new(&p))?;
        }
        Ok(Store {
            conn: Mutex::new(conn),
            paths: paths.clone(),
        })
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves the connection usable.
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // -- workspaces ---------------------------------------------------------

    pub fn list_workspaces(&self) -> Result<Vec<Workspace>, StoreError> {
        let db = self.db();
        let mut stmt = db.prepare(
            r#"SELECT id, name, root, color, "order", created_at_ms FROM workspaces
               ORDER BY "order", created_at_ms"#,
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, root, color, order, created_at_ms) = row?;
            out.push(Workspace {
                id: id
                    .parse()
                    .map_err(|e| StoreError::Codec(format!("workspace id: {e}")))?,
                name,
                root: PathBuf::from(root),
                color: color.as_deref().and_then(parse_color),
                order: u32::try_from(order).unwrap_or(0),
                created_at_ms,
            });
        }
        Ok(out)
    }

    pub fn upsert_workspace(&self, ws: &Workspace) -> Result<(), StoreError> {
        let root = path_str(&ws.root)?;
        self.db().execute(
            r#"INSERT INTO workspaces (id, name, root, color, "order", created_at_ms)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)
               ON CONFLICT(id) DO UPDATE SET name = excluded.name, root = excluded.root,
                 color = excluded.color, "order" = excluded."order""#,
            params![
                ws.id.to_string(),
                ws.name,
                root,
                ws.color.map(format_color),
                i64::from(ws.order),
                ws.created_at_ms
            ],
        )?;
        Ok(())
    }

    pub fn delete_workspace(&self, id: WorkspaceId) -> Result<(), StoreError> {
        self.db().execute(
            "DELETE FROM workspaces WHERE id = ?1",
            params![id.to_string()],
        )?;
        Ok(())
    }

    // -- sessions -----------------------------------------------------------

    /// All sessions ordered by `order`. Rows whose JSON no longer decodes are
    /// skipped with a warning instead of failing the whole listing.
    pub fn list_sessions(&self) -> Result<Vec<SessionMeta>, StoreError> {
        let db = self.db();
        let mut stmt = db.prepare(r#"SELECT id, meta_json FROM sessions ORDER BY "order", id"#)?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, json) = row?;
            match serde_json::from_str::<SessionMeta>(&json) {
                Ok(meta) => out.push(meta),
                Err(e) => {
                    tracing::warn!(session = %id, error = %e, "skipping undecodable session row")
                }
            }
        }
        Ok(out)
    }

    pub fn get_session(&self, id: SessionId) -> Result<Option<SessionMeta>, StoreError> {
        let json: Option<String> = self
            .db()
            .query_row(
                "SELECT meta_json FROM sessions WHERE id = ?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|j| serde_json::from_str(&j).map_err(|e| StoreError::Codec(e.to_string())))
            .transpose()
    }

    pub fn upsert_session(&self, s: &SessionMeta) -> Result<(), StoreError> {
        let json = serde_json::to_string(s).map_err(|e| StoreError::Codec(e.to_string()))?;
        self.db().execute(
            r#"INSERT INTO sessions (id, workspace, status, "order", last_active_ms, meta_json)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)
               ON CONFLICT(id) DO UPDATE SET workspace = excluded.workspace,
                 status = excluded.status, "order" = excluded."order",
                 last_active_ms = excluded.last_active_ms, meta_json = excluded.meta_json"#,
            params![
                s.id.to_string(),
                s.workspace.to_string(),
                status_name(&s.status),
                i64::from(s.order),
                s.last_active_ms,
                json
            ],
        )?;
        Ok(())
    }

    /// Remove metadata, events, snapshot and journal. Missing files are fine.
    /// Remove everything stored for a session. Files go first: a file that
    /// cannot be removed is logged and the rows are deleted anyway, while a
    /// failing row deletion leaves the session listed (so the purge can be
    /// retried) instead of leaving snapshot / journal files — possibly with
    /// sensitive output — that no row points to any more.
    pub fn purge_session(&self, id: SessionId) -> Result<(), StoreError> {
        if let Err(e) = self.delete_snapshot(id) {
            tracing::warn!(session = %id, error = %e, "purge: cannot delete snapshot");
        }
        let journal = self.paths.journal_dir(&id);
        if let Err(e) = fsutil::remove_dir_if_exists(&journal) {
            tracing::warn!(session = %id, dir = %journal.display(), error = %e, "purge: cannot delete journal");
        }
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute(
            "DELETE FROM events WHERE session = ?1",
            params![id.to_string()],
        )?;
        tx.execute(
            "DELETE FROM sessions WHERE id = ?1",
            params![id.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    // -- events -------------------------------------------------------------

    pub fn record_event(&self, e: &EventRecord) -> Result<(), StoreError> {
        let detail = e.detail.as_deref().map(|d| truncate(d, MAX_EVENT_DETAIL));
        self.db().execute(
            "INSERT INTO events (session, at_ms, kind, state, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![e.session.to_string(), e.at_ms, e.kind, e.state, detail],
        )?;
        Ok(())
    }

    /// Most recent first (`at_ms` descending, insertion order breaks ties).
    pub fn list_events(
        &self,
        session: SessionId,
        limit: u32,
    ) -> Result<Vec<EventRecord>, StoreError> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT at_ms, kind, state, detail FROM events WHERE session = ?1
             ORDER BY at_ms DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session.to_string(), i64::from(limit)], |row| {
            Ok(EventRecord {
                session,
                at_ms: row.get(0)?,
                kind: row.get(1)?,
                state: row.get(2)?,
                detail: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // -- snapshots ----------------------------------------------------------

    /// postcard → zstd(3) → `<sid>.bin.zst.tmp` (0600) → fsync → rename.
    pub fn write_snapshot(&self, snap: &SessionSnapshotFile) -> Result<(), StoreError> {
        let raw = postcard::to_stdvec(snap).map_err(|e| StoreError::Codec(e.to_string()))?;
        let compressed = zstd::bulk::compress(&raw, SNAPSHOT_ZSTD_LEVEL)?;
        fsutil::ensure_private_dir(&self.paths.snapshots_dir)?;
        let path = self.paths.snapshot_file(&snap.session.id);
        fsutil::write_atomic_private(&path, &compressed)?;
        Ok(())
    }

    /// `Ok(None)` when no snapshot exists. The format version is checked
    /// before decoding the rest of the file.
    pub fn read_snapshot(&self, id: SessionId) -> Result<Option<SessionSnapshotFile>, StoreError> {
        let path = self.paths.snapshot_file(&id);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let raw = zstd::stream::decode_all(&bytes[..])?;
        let (found, _) = postcard::take_from_bytes::<u32>(&raw)
            .map_err(|e| StoreError::Codec(format!("snapshot header: {e}")))?;
        if found != SNAPSHOT_FORMAT_VERSION {
            return Err(StoreError::Format {
                found,
                want: SNAPSHOT_FORMAT_VERSION,
            });
        }
        let snap: SessionSnapshotFile =
            postcard::from_bytes(&raw).map_err(|e| StoreError::Codec(e.to_string()))?;
        if snap.session.id != id {
            return Err(StoreError::Codec(format!(
                "snapshot {} contains session {}",
                path.display(),
                snap.session.id
            )));
        }
        Ok(Some(snap))
    }

    pub fn delete_snapshot(&self, id: SessionId) -> Result<(), StoreError> {
        let path = self.paths.snapshot_file(&id);
        fsutil::remove_file_if_exists(&path)?;
        fsutil::remove_file_if_exists(&fsutil::tmp_path(&path))?;
        Ok(())
    }

    // -- journals -----------------------------------------------------------

    pub fn open_journal(&self, id: SessionId) -> Result<JournalWriter, StoreError> {
        JournalWriter::open(&self.paths.journal_dir(&id))
    }
}

fn migrate(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS workspaces (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            root TEXT NOT NULL,
            color TEXT,
            "order" INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            workspace TEXT NOT NULL,
            status TEXT NOT NULL,
            "order" INTEGER NOT NULL,
            last_active_ms INTEGER NOT NULL,
            meta_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS sessions_by_workspace ON sessions (workspace, "order");
        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session TEXT NOT NULL,
            at_ms INTEGER NOT NULL,
            kind TEXT NOT NULL,
            state TEXT NOT NULL,
            detail TEXT
        );
        CREATE INDEX IF NOT EXISTS events_by_session_time ON events (session, at_ms);
        "#,
    )?;
    let found: Option<i64> =
        conn.query_row("SELECT MAX(version) FROM schema_version", [], |row| {
            row.get(0)
        })?;
    match found {
        None => {
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![SCHEMA_VERSION],
            )?;
        }
        Some(v) if v > SCHEMA_VERSION => {
            return Err(StoreError::Schema {
                found: v,
                want: SCHEMA_VERSION,
            });
        }
        Some(_) => {}
    }
    Ok(())
}

fn status_name(s: &SessionStatus) -> &'static str {
    match s {
        SessionStatus::Live => "live",
        SessionStatus::Dormant { .. } => "dormant",
        SessionStatus::Restored => "restored",
    }
}

fn path_str(p: &Path) -> Result<&str, StoreError> {
    p.to_str()
        .ok_or_else(|| StoreError::Codec(format!("non-UTF-8 path: {}", p.display())))
}

fn format_color(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn parse_color(s: &str) -> Option<[u8; 3]> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests;
