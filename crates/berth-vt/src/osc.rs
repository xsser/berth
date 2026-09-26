//! Side-channel scanner for OSC sequences that `alacritty_terminal` ignores.
//!
//! Recognised: OSC 7 (`file://host/path` cwd), OSC 133 A/B/C/D (shell
//! integration prompt marks; `D;<exit>` carries the exit code), OSC 9 (iTerm
//! / ConEmu notification text) and OSC 777 (`notify;title;body`).
//!
//! The scanner is stateful so sequences split across PTY read chunks are
//! reassembled. Sequence payloads are capped (4 KiB) to bound memory; a
//! longer sequence is discarded. Everything else passes through untouched —
//! the scanner never modifies the byte stream.

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptMark {
    /// 133;A — prompt is about to be drawn (shell is idle).
    PromptStart,
    /// 133;B — user finished typing, command about to run.
    CommandStart,
    /// 133;C — command output begins.
    OutputStart,
    /// 133;D[;exit] — command finished.
    CommandEnd { exit_code: Option<i32> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OscEvent {
    Cwd(PathBuf),
    Prompt(PromptMark),
    Notify { title: Option<String>, body: String },
}

pub const MAX_OSC_LEN: usize = 4096;

/// Incremental OSC scanner. See module docs.
#[derive(Debug, Default)]
pub struct OscPrescanner {
    // Implementation detail is up to `berth-vt`; keep fields private.
    _state: (),
}

impl OscPrescanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan a chunk; returns events found (possibly completing a sequence
    /// started in a previous chunk).
    pub fn scan(&mut self, _bytes: &[u8]) -> Vec<OscEvent> {
        todo!("berth-vt: implement OSC prescanner (see docs/tasks/vt.md)")
    }
}
