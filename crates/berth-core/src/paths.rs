//! Filesystem layout. All directories are created `0700`.
//!
//! Environment overrides (used by tests and by `berth-hook` when spawned with
//! a non-default daemon): `BERTH_DATA_DIR`, `BERTH_SOCKET`, `BERTH_CONFIG`.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub config_file: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub db: PathBuf,
    pub snapshots_dir: PathBuf,
    pub journals_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl Paths {
    pub fn resolve() -> Paths {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
        let data_dir = std::env::var_os("BERTH_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_data_dir(&home));
        let config_file = std::env::var_os("BERTH_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_config_dir(&home).join("config.toml"));
        let socket = std::env::var_os("BERTH_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_socket(&home, &data_dir));
        Paths::from_data_dir(data_dir, config_file, socket)
    }

    /// Layout rooted at an explicit directory (tests).
    pub fn in_dir(root: &Path) -> Paths {
        Paths::from_data_dir(
            root.to_path_buf(),
            root.join("config.toml"),
            root.join("berthd.sock"),
        )
    }

    fn from_data_dir(data_dir: PathBuf, config_file: PathBuf, socket: PathBuf) -> Paths {
        Paths {
            lock: data_dir.join("berthd.lock"),
            db: data_dir.join("berth.sqlite3"),
            snapshots_dir: data_dir.join("snapshots"),
            journals_dir: data_dir.join("journals"),
            logs_dir: data_dir.join("logs"),
            data_dir,
            config_file,
            socket,
        }
    }

    pub fn snapshot_file(&self, session: &crate::SessionId) -> PathBuf {
        self.snapshots_dir.join(format!("{session}.bin.zst"))
    }

    pub fn journal_dir(&self, session: &crate::SessionId) -> PathBuf {
        self.journals_dir.join(session.to_string())
    }

    /// Create data directories with owner-only permissions.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for dir in [&self.data_dir, &self.snapshots_dir, &self.journals_dir, &self.logs_dir] {
            std::fs::create_dir_all(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        if let Some(parent) = self.socket.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }
}

fn default_data_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/berth")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("berth")
    }
}

fn default_config_dir(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("berth")
}

fn default_socket(_home: &Path, data_dir: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        // sun_path is limited to 104 bytes on macOS; the data dir is short enough.
        data_dir.join("berthd.sock")
    } else {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(|d| PathBuf::from(d).join("berth"))
            .unwrap_or_else(|| data_dir.to_path_buf())
            .join("berthd.sock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_dir_layout() {
        let p = Paths::in_dir(Path::new("/tmp/x"));
        assert_eq!(p.db, PathBuf::from("/tmp/x/berth.sqlite3"));
        assert_eq!(p.socket, PathBuf::from("/tmp/x/berthd.sock"));
        let sid = crate::SessionId::nil();
        assert!(p.snapshot_file(&sid).to_string_lossy().ends_with(".bin.zst"));
    }
}
