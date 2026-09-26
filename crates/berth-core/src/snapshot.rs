//! The compact line format shared by the wire protocol, the on-disk snapshot
//! and sidebar previews.
//!
//! A `LineSnapshot` is a sequence of `Run`s: maximal spans of text sharing one
//! `StyleId`. Cell positions are *not* stored per character; both producer
//! (daemon, converting from `alacritty_terminal` cells) and consumer (renderer)
//! derive them with `char_cells`, which wraps `unicode-width` — the same crate
//! alacritty uses for placement — so the two sides always agree.
//!
//! Zero-width characters (combining marks, ZWJ sequence parts) are appended to
//! the run text and contribute 0 cells; the renderer shapes them together with
//! the preceding base character.

use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthChar;

use crate::session::SessionMeta;
use crate::style::{StyleId, StyleTable};

/// Display width of one character in terminal cells (0, 1 or 2).
pub fn char_cells(c: char) -> u16 {
    UnicodeWidthChar::width(c).unwrap_or(0) as u16
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub text: String,
    pub style: StyleId,
    /// Number of terminal cells this run occupies (>= number of chars for
    /// wide chars, < for zero-width chars). Trailing empty cells of a line are
    /// represented as a run of spaces with the default style, or omitted.
    pub cells: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineSnapshot {
    pub runs: Vec<Run>,
    /// True when this line soft-wraps into the next one (for reflow and copy).
    pub wrapped: bool,
}

impl LineSnapshot {
    pub fn blank() -> Self {
        Self::default()
    }

    /// Append one character, merging into the last run when the style matches.
    pub fn push(&mut self, ch: char, style: StyleId) {
        let cells = char_cells(ch);
        match self.runs.last_mut() {
            Some(run) if run.style == style => {
                run.text.push(ch);
                run.cells += cells;
            }
            _ => self.runs.push(Run { text: ch.to_string(), style, cells }),
        }
    }

    /// Append text that is known to be all width-1 (fast path for ASCII).
    pub fn push_str(&mut self, s: &str, style: StyleId) {
        for ch in s.chars() {
            self.push(ch, style);
        }
    }

    pub fn cells(&self) -> u16 {
        self.runs.iter().map(|r| r.cells).sum()
    }

    pub fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }

    /// Text with trailing whitespace removed (what `copy` should yield).
    pub fn text_trimmed(&self) -> String {
        let t = self.text();
        t.trim_end().to_string()
    }

    pub fn is_blank(&self) -> bool {
        self.runs.iter().all(|r| r.text.chars().all(char::is_whitespace))
    }

    /// Drop trailing runs that are only spaces in the default style.
    pub fn trim_trailing_default(&mut self) {
        while let Some(last) = self.runs.last() {
            if last.style == StyleId::DEFAULT && last.text.chars().all(|c| c == ' ') {
                self.runs.pop();
            } else {
                break;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    /// Unfocused-window style.
    HollowBlock,
    Hidden,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorState {
    /// Row within the visible screen (0 = top), already adjusted for
    /// `display_offset` by the producer: if the cursor is scrolled out of view
    /// `visible` is false.
    pub row: u16,
    pub col: u16,
    pub shape: CursorShape,
    pub visible: bool,
    pub blinking: bool,
}

bitflags::bitflags! {
    /// Terminal modes the client needs for input encoding and rendering.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct TermModes: u32 {
        const ALT_SCREEN        = 1 << 0;
        const BRACKETED_PASTE   = 1 << 1;
        const APP_CURSOR        = 1 << 2;
        const APP_KEYPAD        = 1 << 3;
        const FOCUS_IN_OUT      = 1 << 4;
        const MOUSE_CLICK       = 1 << 5;
        const MOUSE_DRAG        = 1 << 6;
        const MOUSE_MOTION      = 1 << 7;
        const SGR_MOUSE         = 1 << 8;
        const UTF8_MOUSE        = 1 << 9;
        const ALTERNATE_SCROLL  = 1 << 10;
        const SHOW_CURSOR       = 1 << 11;
        const KITTY_DISAMBIGUATE       = 1 << 16;
        const KITTY_REPORT_EVENT_TYPES = 1 << 17;
        const KITTY_REPORT_ALTERNATE   = 1 << 18;
        const KITTY_REPORT_ALL_AS_ESC  = 1 << 19;
        const KITTY_REPORT_ASSOC_TEXT  = 1 << 20;
    }
}

impl TermModes {
    pub fn mouse_reporting(&self) -> bool {
        self.intersects(TermModes::MOUSE_CLICK | TermModes::MOUSE_DRAG | TermModes::MOUSE_MOTION)
    }
}

/// Full visible screen of a live session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    pub cols: u16,
    pub rows: u16,
    /// Exactly `rows` entries, top to bottom, at the current `display_offset`.
    pub lines: Vec<LineSnapshot>,
    pub cursor: CursorState,
    pub modes: TermModes,
    /// How many history lines the view is scrolled up by (0 = bottom).
    pub display_offset: u32,
    /// Number of scrollback lines available above the screen (live only).
    pub history_len: u64,
    pub title: String,
}

/// On-disk snapshot of one session (`SNAPSHOT_FORMAT_VERSION`).
///
/// Lifecycle across daemon restarts:
/// - While live: `history` = restored prefix from earlier lives ++ current
///   scrollback; `screen` = current visible screen.
/// - On restart the session becomes `Restored`; `history ++ screen.lines`
///   (trailing blank lines trimmed) is the new restored prefix and `screen` is
///   `None`. Reviving spawns a fresh PTY whose lines are appended below.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshotFile {
    pub format_version: u32,
    pub saved_at_ms: i64,
    pub session: SessionMeta,
    pub styles: StyleTable,
    pub history: Vec<LineSnapshot>,
    pub screen: Option<ScreenSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_merges_runs_and_counts_cells() {
        let mut l = LineSnapshot::blank();
        l.push_str("ab", StyleId(0));
        l.push('你', StyleId(0));
        l.push('c', StyleId(1));
        l.push('\u{301}', StyleId(1)); // combining acute: zero width
        assert_eq!(l.runs.len(), 2);
        assert_eq!(l.runs[0].cells, 4);
        assert_eq!(l.runs[1].cells, 1);
        assert_eq!(l.cells(), 5);
        assert_eq!(l.text(), "ab你c\u{301}");
    }

    #[test]
    fn trim_trailing_default_spaces() {
        let mut l = LineSnapshot::blank();
        l.push_str("x", StyleId(2));
        l.push_str("   ", StyleId(0));
        l.trim_trailing_default();
        assert_eq!(l.runs.len(), 1);
        assert_eq!(l.text_trimmed(), "x");
    }

    #[test]
    fn snapshot_file_roundtrip() {
        let file = SessionSnapshotFile {
            format_version: crate::SNAPSHOT_FORMAT_VERSION,
            saved_at_ms: 1,
            session: SessionMeta::default(),
            styles: StyleTable::new(),
            history: vec![LineSnapshot::blank()],
            screen: None,
        };
        let bytes = postcard::to_stdvec(&file).unwrap();
        let back: SessionSnapshotFile = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(file, back);
    }
}
