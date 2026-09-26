//! VT engine for the daemon: one [`Terminal`] per session wraps
//! `alacritty_terminal`, and one [`PtyHandle`] owns the child process.
//!
//! Data flow per session (driven by the daemon's session actor):
//!
//! ```text
//! PtyHandle reader thread ──PtyOutput::Data──▶ Terminal::process(bytes)
//!                                              ├─ OscPrescanner (OSC 7 / 133 / 9 / 777)
//!                                              ├─ alacritty Processor::advance
//!                                              └─ ProcessOutcome { osc, events }
//! Terminal::take_damage() ──▶ Terminal::lines(&dirty_rows) ──▶ ScreenUpdate
//! ```
//!
//! `alacritty_terminal` drops unknown OSC sequences silently (vte 0.15
//! `osc_dispatch` → `unhandled`), which is why the prescanner runs *before*
//! bytes reach the `Term`.
//!
//! Obligations of the session actor beyond the arrows above:
//! - write every [`TermEvent::PtyWrite`] back to the PTY (DA / CPR / color
//!   and size replies; applications may block waiting for them);
//! - while [`Terminal::sync_deadline`] is `Some`, wake up at that instant and
//!   call [`Terminal::flush_expired_sync`] (DEC 2026 synchronized output is
//!   held back until its end marker or the 150 ms deadline);
//! - after `PtyOutput::Eof`, poll [`PtyHandle::try_wait`] for the exit code;
//!   to stop a session call [`PtyHandle::kill`], then [`PtyHandle::force_kill`]
//!   if it is still alive after [`KILL_GRACE`] (dropping the handle does the
//!   same in the background).
// `unsafe` is confined to `pty.rs` (libc: killpg / proc_pidpath / proc_name /
// proc_pidinfo).
#![deny(unsafe_code)]

pub mod convert;
pub mod osc;
mod palette;
pub mod pty;
pub mod terminal;

pub use osc::{OscEvent, OscPrescanner, PromptMark};
pub use pty::{ProcessInfo, PtyHandle, PtyOutput, PtySpawn, KILL_GRACE};
pub use terminal::{
    Damage, ProcessOutcome, TermEvent, Terminal, TerminalConfig, MAX_SCROLLBACK_LINES,
};

#[derive(Debug, thiserror::Error)]
pub enum VtError {
    #[error("pty: {0}")]
    Pty(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Shell-style exit code: the exit status, or 128 + signal number when the
/// process was killed by a signal.
pub(crate) fn exit_status_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(-1)
}
