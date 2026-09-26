//! Owner-only file helpers: directories `0700`, files `0600`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub(crate) const FILE_MODE: u32 = 0o600;
pub(crate) const DIR_MODE: u32 = 0o700;

/// Create `dir` (and parents) if needed and force mode `0700` on it.
pub(crate) fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))
}

/// Create an empty `0600` file if missing; tighten the mode if it exists.
pub(crate) fn ensure_private_file(path: &Path) -> io::Result<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(FILE_MODE)
        .open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))
}

pub(crate) fn restrict_if_exists(path: &Path) -> io::Result<()> {
    match fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Open for appending, creating with `0600`.
pub(crate) fn open_private_append(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(FILE_MODE)
        .open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))?;
    Ok(file)
}

/// `<path>.tmp` next to `path`.
pub(crate) fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Write `bytes` to `<path>.tmp` (0600), fsync, rename over `path`, then
/// fsync the directory so the rename itself is durable.
pub(crate) fn write_atomic_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = tmp_path(path);
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(FILE_MODE)
            .open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Some(dir) = path.parent() {
        // Directory fsync is best-effort (not supported everywhere).
        if let Err(e) = File::open(dir).and_then(|d| d.sync_all()) {
            tracing::debug!(dir = %dir.display(), error = %e, "directory fsync failed");
        }
    }
    Ok(())
}

pub(crate) fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

pub(crate) fn remove_dir_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}
