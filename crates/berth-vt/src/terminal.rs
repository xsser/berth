//! `Terminal`: VT state for one session (no PTY inside; the daemon wires
//! `PtyHandle` output into `process`).

use berth_core::{CursorState, Dims, LineSnapshot, ScreenSnapshot, StyleInterner, TermModes};

use crate::osc::OscEvent;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalConfig {
    pub cols: u16,
    pub rows: u16,
    /// Scrollback lines kept in memory (alacritty `scrolling_history`).
    pub scrollback_lines: usize,
    pub kitty_keyboard: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self { cols: 80, rows: 24, scrollback_lines: 20_000, kitty_keyboard: true }
    }
}

/// Events surfaced by alacritty's `EventListener` during `process`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermEvent {
    Title(Option<String>),
    Bell,
    /// Bytes the terminal must send back to the application (DA, CPR, ...).
    /// The daemon writes them to the PTY.
    PtyWrite(Vec<u8>),
    /// OSC 52 store (only surfaced when allowed by config).
    ClipboardStore(String),
    CursorBlinkingChanged,
    /// Child exit reported through the VT layer (alacritty `ChildExit`).
    ChildExit(i32),
}

#[derive(Debug, Default)]
pub struct ProcessOutcome {
    pub osc: Vec<OscEvent>,
    pub events: Vec<TermEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Damage {
    None,
    Full,
    /// Dirty screen rows (0 = top of the visible screen).
    Lines(Vec<u16>),
}

pub struct Terminal {
    _private: (),
}

impl Terminal {
    pub fn new(_cfg: TerminalConfig) -> Terminal {
        todo!("berth-vt: Terminal::new")
    }

    /// Prescan OSC side channel, then advance the VT parser.
    pub fn process(&mut self, _bytes: &[u8]) -> ProcessOutcome {
        todo!("berth-vt: Terminal::process")
    }

    /// Damage accumulated since the previous call (resets it).
    pub fn take_damage(&mut self) -> Damage {
        todo!("berth-vt: Terminal::take_damage")
    }

    pub fn resize(&mut self, _cols: u16, _rows: u16) {
        todo!("berth-vt: Terminal::resize")
    }

    pub fn dims(&self) -> Dims {
        todo!("berth-vt: Terminal::dims")
    }

    /// The daemon never scrolls the `Term` (display offset stays 0); clients
    /// scroll by fetching history. So `screen` is always the bottom screen.
    pub fn screen(&mut self) -> ScreenSnapshot {
        todo!("berth-vt: Terminal::screen")
    }

    /// Selected visible rows (for damage deltas).
    pub fn lines(&mut self, _rows: &[u16]) -> Vec<(u16, LineSnapshot)> {
        todo!("berth-vt: Terminal::lines")
    }

    /// Number of scrollback lines above the screen.
    pub fn history_len(&self) -> usize {
        todo!("berth-vt: Terminal::history_len")
    }

    /// Scrollback lines `start..start+count` with 0 = oldest.
    pub fn history(&mut self, _start: usize, _count: usize) -> Vec<LineSnapshot> {
        todo!("berth-vt: Terminal::history")
    }

    pub fn cursor(&self) -> CursorState {
        todo!("berth-vt: Terminal::cursor")
    }

    pub fn modes(&self) -> TermModes {
        todo!("berth-vt: Terminal::modes")
    }

    pub fn title(&self) -> Option<&str> {
        todo!("berth-vt: Terminal::title")
    }

    /// Style interner shared by `screen`, `lines` and `history`.
    pub fn interner(&mut self) -> &mut StyleInterner {
        todo!("berth-vt: Terminal::interner")
    }
}
