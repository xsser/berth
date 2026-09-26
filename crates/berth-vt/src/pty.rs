//! PTY ownership: spawn the child, pump output on a reader thread, write
//! input, resize, inspect the foreground process.
//!
//! This is the only module in the crate allowed to use `unsafe` (libc FFI);
//! every block needs a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::path::PathBuf;

use crossbeam_channel::Receiver;

use crate::VtError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtySpawn {
    /// argv; empty means "the user's login shell" (`$SHELL`, `-l`).
    pub command: Vec<String>,
    pub cwd: PathBuf,
    /// Extra environment on top of the daemon's environment. The daemon
    /// always sets `BERTH_SESSION_ID`, `BERTH_SOCKET`, `TERM`, `COLORTERM`.
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

/// Messages from the reader thread.
#[derive(Debug)]
pub enum PtyOutput {
    Data(Vec<u8>),
    /// Reader hit EOF; `try_wait` yields the exit code.
    Eof,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    /// Executable base name (e.g. `claude`, `node`, `zsh`).
    pub name: String,
    pub cwd: Option<PathBuf>,
}

pub struct PtyHandle {
    _private: (),
}

impl PtyHandle {
    /// Spawn the child on a new PTY and start the reader thread. Reads are
    /// delivered as `PtyOutput::Data` chunks (up to 64 KiB each).
    pub fn spawn(_spec: &PtySpawn) -> Result<(PtyHandle, Receiver<PtyOutput>), VtError> {
        todo!("berth-vt: PtyHandle::spawn")
    }

    pub fn write(&self, _bytes: &[u8]) -> Result<(), VtError> {
        todo!("berth-vt: PtyHandle::write")
    }

    pub fn resize(&self, _cols: u16, _rows: u16) -> Result<(), VtError> {
        todo!("berth-vt: PtyHandle::resize")
    }

    pub fn child_pid(&self) -> u32 {
        todo!("berth-vt: PtyHandle::child_pid")
    }

    /// Non-blocking; `Some(code)` once the child exited.
    pub fn try_wait(&mut self) -> Result<Option<i32>, VtError> {
        todo!("berth-vt: PtyHandle::try_wait")
    }

    /// SIGHUP the child's process group.
    pub fn kill(&mut self) -> Result<(), VtError> {
        todo!("berth-vt: PtyHandle::kill")
    }

    /// Foreground process group leader of the PTY (`tcgetpgrp` on the master
    /// fd, then `proc_pidinfo` / `/proc`), used for agent-kind heuristics.
    pub fn foreground_process(&self) -> Option<ProcessInfo> {
        todo!("berth-vt: PtyHandle::foreground_process")
    }
}
