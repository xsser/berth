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
            _ => self.runs.push(Run {
                text: ch.to_string(),
                style,
                cells,
            }),
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
        self.runs
            .iter()
            .all(|r| r.text.chars().all(char::is_whitespace))
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

/// On-disk snapshot of one session, as berth-store reads and writes it
/// (the file layouts are listed at [`crate::SNAPSHOT_FORMAT_VERSION`]).
/// `format_version` is the format the file was read in; berth-store always
/// writes the current one.
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

/// Format 1 (M1 / M2), from before `AgentInfo::last_agent`: the whole
/// `SessionSnapshotFile` as postcard, which is positional, so the added
/// field moved everything after it. These are the layouts of then (fields
/// in the same order), for berth-store to read such files into the current
/// types (`last_agent: None`, `archived_at_ms: None`), and for tests to write
/// them.
pub mod v1 {
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    use super::{LineSnapshot, ScreenSnapshot};
    use crate::style::StyleTable;
    use crate::{
        AgentKind, AgentState, PersistPolicy, SessionId, SessionStatus, StateSource, WorkspaceId,
    };

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct SessionSnapshotFile {
        pub format_version: u32,
        pub saved_at_ms: i64,
        pub session: SessionMeta,
        pub styles: StyleTable,
        pub history: Vec<LineSnapshot>,
        pub screen: Option<ScreenSnapshot>,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct SessionMeta {
        pub id: SessionId,
        pub workspace: WorkspaceId,
        pub title_auto: String,
        pub title_user: Option<String>,
        pub cwd: PathBuf,
        pub command: Vec<String>,
        pub env: Vec<(String, String)>,
        pub status: SessionStatus,
        pub agent: AgentInfo,
        pub created_at_ms: i64,
        pub last_active_ms: i64,
        pub unread: bool,
        pub persist: PersistPolicy,
        pub order: u32,
        pub cols: u16,
        pub rows: u16,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct AgentInfo {
        pub kind: AgentKind,
        pub external_id: Option<String>,
        pub transcript_path: Option<PathBuf>,
        pub model: Option<String>,
        pub context_pct: Option<f32>,
        pub cost_usd: Option<f64>,
        pub state: AgentState,
        pub since_ms: i64,
        pub source: StateSource,
        pub confidence: f32,
    }

    impl From<SessionSnapshotFile> for super::SessionSnapshotFile {
        fn from(f: SessionSnapshotFile) -> Self {
            super::SessionSnapshotFile {
                format_version: f.format_version,
                saved_at_ms: f.saved_at_ms,
                session: f.session.into(),
                styles: f.styles,
                history: f.history,
                screen: f.screen,
            }
        }
    }

    impl From<SessionMeta> for crate::SessionMeta {
        fn from(m: SessionMeta) -> Self {
            crate::SessionMeta {
                id: m.id,
                workspace: m.workspace,
                title_auto: m.title_auto,
                title_user: m.title_user,
                cwd: m.cwd,
                command: m.command,
                env: m.env,
                status: m.status,
                agent: m.agent.into(),
                created_at_ms: m.created_at_ms,
                last_active_ms: m.last_active_ms,
                unread: m.unread,
                persist: m.persist,
                order: m.order,
                cols: m.cols,
                rows: m.rows,
                archived_at_ms: None,
            }
        }
    }

    impl From<AgentInfo> for crate::AgentInfo {
        fn from(a: AgentInfo) -> Self {
            crate::AgentInfo {
                kind: a.kind,
                external_id: a.external_id,
                transcript_path: a.transcript_path,
                model: a.model,
                context_pct: a.context_pct,
                cost_usd: a.cost_usd,
                state: a.state,
                since_ms: a.since_ms,
                source: a.source,
                confidence: a.confidence,
                last_agent: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentInfo, WorkspaceId};
    use crate::{AgentKind, AgentState, PersistPolicy, SessionId, SessionStatus, StateSource};

    #[test]
    fn format_1_files_are_read_into_the_current_types() {
        let mut line = LineSnapshot::blank();
        line.push_str("hello from format 1", StyleId(0));
        // Every field differs from its default, so a layout that is off by
        // anything shows.
        let want = SessionSnapshotFile {
            format_version: 1,
            saved_at_ms: 33,
            session: SessionMeta {
                id: SessionId::new(),
                workspace: WorkspaceId::new(),
                title_auto: "claude".into(),
                title_user: Some("t".into()),
                cwd: "/w".into(),
                command: vec!["/bin/zsh".into()],
                env: vec![("K".into(), "V".into())],
                status: SessionStatus::Restored,
                agent: AgentInfo {
                    kind: AgentKind::Claude,
                    external_id: Some("0f8c2e1a-1111-2222-3333-444455556666".into()),
                    transcript_path: Some("/p/-w/0f8c2e1a.jsonl".into()),
                    model: Some("Opus".into()),
                    context_pct: Some(12.5),
                    cost_usd: Some(0.25),
                    state: AgentState::WaitingPermission {
                        tool: Some("Write".into()),
                    },
                    since_ms: 1_700_000_000_123,
                    source: StateSource::Hook,
                    confidence: 1.0,
                    last_agent: None,
                },
                created_at_ms: 11,
                last_active_ms: 22,
                unread: true,
                persist: PersistPolicy {
                    snapshot: true,
                    journal: true,
                },
                order: 7,
                cols: 100,
                rows: 30,
                archived_at_ms: None,
            },
            styles: StyleTable::default(),
            history: vec![line],
            screen: None,
        };
        // Format 1 is this layout without `last_agent`, the last field of
        // `AgentInfo` (`None`: one zero byte), independent of `v1`'s types.
        // Nor has it `archived_at_ms` (M4), the last field of `SessionMeta`
        // (`None` too), dropped first: it comes after the agent.
        let mut raw = postcard::to_stdvec(&want).unwrap();
        let session = postcard::to_stdvec(&want.session).unwrap();
        assert_eq!(session.last(), Some(&0));
        let at = raw
            .windows(session.len())
            .position(|w| w == session)
            .unwrap();
        raw.remove(at + session.len() - 1);
        let agent = postcard::to_stdvec(&want.session.agent).unwrap();
        assert_eq!(agent.last(), Some(&0));
        let at = raw.windows(agent.len()).position(|w| w == agent).unwrap();
        raw.remove(at + agent.len() - 1);

        let read = postcard::from_bytes::<v1::SessionSnapshotFile>(&raw).unwrap();
        assert_eq!(SessionSnapshotFile::from(read), want);
    }

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
