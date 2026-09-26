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

/// Wire protocol version. Bump on any incompatible change to `protocol`.
pub const PROTOCOL_VERSION: u32 = 1;
/// On-disk snapshot format version (`SessionSnapshotFile`).
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
