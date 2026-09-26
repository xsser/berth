//! Single-instance lock: `flock(LOCK_EX | LOCK_NB)` on `berthd.lock`. The
//! lock file holds the owner's pid for diagnostics. The lock is released
//! when the `DaemonLock` is dropped (fd closed) or the process dies.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use berth_core::Paths;

#[derive(Debug)]
pub struct DaemonLock {
    _file: File,
}

#[derive(Debug)]
pub enum LockOutcome {
    Acquired(DaemonLock),
    /// Another berthd holds the lock.
    Busy {
        pid: Option<u32>,
    },
}

pub fn acquire(paths: &Paths) -> io::Result<LockOutcome> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&paths.lock)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    if !try_flock(&file)? {
        let mut text = String::new();
        let _ = file.read_to_string(&mut text);
        return Ok(LockOutcome::Busy {
            pid: text.trim().parse().ok(),
        });
    }
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{}", std::process::id())?;
    file.sync_all()?;
    Ok(LockOutcome::Acquired(DaemonLock { _file: file }))
}

/// `Ok(false)` when another open file description holds the lock.
#[allow(unsafe_code)]
fn try_flock(file: &File) -> io::Result<bool> {
    loop {
        // SAFETY: `file` owns a valid open fd for the duration of the call;
        // flock has no memory-safety preconditions beyond that.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => return Ok(false),
            Some(libc::EINTR) => continue,
            _ => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_reports_owner_pid_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(dir.path());
        let first = match acquire(&paths).unwrap() {
            LockOutcome::Acquired(l) => l,
            other => panic!("expected lock, got {other:?}"),
        };
        let mode = std::fs::metadata(&paths.lock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // flock conflicts between separate open file descriptions, even in
        // one process, which is what makes this testable in-process.
        match acquire(&paths).unwrap() {
            LockOutcome::Busy { pid } => assert_eq!(pid, Some(std::process::id())),
            other => panic!("expected busy, got {other:?}"),
        }
        drop(first);
        assert!(matches!(acquire(&paths).unwrap(), LockOutcome::Acquired(_)));
    }
}
