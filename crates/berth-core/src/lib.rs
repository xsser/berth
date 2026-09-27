//! Shared domain types for berth: ids, cell styles, the compact line format
//! (`LineSnapshot`) used on the wire and on disk, agent state, session
//! metadata, and the client ⟷ daemon protocol.
//!
//! This crate is the **frozen interface** between the daemon, the GUI client
//! and the hook CLI. Changes here must be additive and coordinated.
#![forbid(unsafe_code)]

pub mod agent;
pub mod ids;
pub mod paths;
pub mod protocol;
pub mod session;
pub mod snapshot;
pub mod style;

pub use agent::*;
pub use ids::*;
pub use paths::Paths;
pub use protocol::*;
pub use session::*;
pub use snapshot::*;
pub use style::*;

/// Wire protocol version, checked by `Hello`. Bump on any change to the
/// protocol messages or the types they carry.
/// - 1: M1 / M2.
/// - 2: M3: `ListEvents` / `ResumeCommand` and their answers,
///   `AgentInfo::last_agent`.
/// - 3: M4: `Archive` / `Unarchive`, `SessionMeta::archived_at_ms`.
pub const PROTOCOL_VERSION: u32 = 3;
/// On-disk snapshot format version (berth-store's `snapshots/<sid>.bin.zst`,
/// zstd over postcard). postcard is positional: a field added to a type it
/// encodes changes the layout.
/// - 1: M1 / M2: `SessionSnapshotFile` as postcard ([`snapshot::v1`]).
/// - 2: M3: the same envelope with the session (`SessionMeta`, which gained
///   `AgentInfo::last_agent`) as a JSON string, so a field added to it with
///   `#[serde(default)]` needs no new version (as in the SQLite metadata);
///   styles, history and screen stay postcard. M4's
///   `SessionMeta::archived_at_ms` is such a field.
pub const SNAPSHOT_FORMAT_VERSION: u32 = 2;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
