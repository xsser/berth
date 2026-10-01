//! `Terminal`: VT state for one session (no PTY inside; the daemon wires
//! `PtyHandle` output into `process`).
//!
//! Driving contract (single owner thread):
//! - Feed every PTY chunk to [`Terminal::process`]; write every
//!   [`TermEvent::PtyWrite`] back to the PTY (DA / CPR / color replies).
//! - On the batching tick call [`Terminal::take_damage`] and ship
//!   [`Terminal::lines`] for the dirty rows (or [`Terminal::screen`] on
//!   `Damage::Full`), plus `interner().take_pending()` style deltas.
//! - While [`Terminal::sync_deadline`] is `Some`, an application is inside a
//!   synchronized update (DEC mode 2026) and its output is held back; call
//!   [`Terminal::flush_expired_sync`] once the deadline has passed so a
//!   missing "end" marker cannot freeze the screen.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::COUNT as COLOR_COUNT;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{
    Config, Osc52, Term, TermDamage, TermMode, MIN_COLUMNS, MIN_SCREEN_LINES,
};
use alacritty_terminal::vte::ansi::{CursorShape as AnsiCursorShape, Processor, Rgb};
use berth_core::{
    CursorShape, CursorState, Dims, LineSnapshot, ScreenSnapshot, StyleInterner, TermColors,
    TermModes,
};
use parking_lot::Mutex;

use crate::convert::row_to_line;
use crate::osc::{OscEvent, OscPrescanner};
use crate::palette;

/// Upper bound for `TerminalConfig::scrollback_lines` (DESIGN §5).
pub const MAX_SCROLLBACK_LINES: usize = 100_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalConfig {
    pub cols: u16,
    pub rows: u16,
    /// Scrollback lines kept in memory (alacritty `scrolling_history`).
    /// Values above [`MAX_SCROLLBACK_LINES`] are clamped.
    pub scrollback_lines: usize,
    pub kitty_keyboard: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            scrollback_lines: 20_000,
            kitty_keyboard: true,
        }
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
    /// OSC 52 store (only surfaced when allowed by config, see
    /// [`Terminal::set_clipboard_store_allowed`]).
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

/// Collects alacritty events emitted while the parser runs; `Terminal`
/// drains them after each `advance`.
#[derive(Clone, Default)]
struct Listener {
    queue: Arc<Mutex<Vec<Event>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        // Repaint hints: damage tracking already covers them.
        if !matches!(event, Event::Wakeup | Event::MouseCursorDirty) {
            self.queue.lock().push(event);
        }
    }
}

pub struct Terminal {
    term: Term<Listener>,
    parser: Processor,
    events: Arc<Mutex<Vec<Event>>>,
    prescanner: OscPrescanner,
    interner: StyleInterner,
    damage_full: bool,
    damage_lines: BTreeSet<u16>,
    title: Option<String>,
    allow_clipboard_store: bool,
    /// The GUI's colors, for color queries the application has not
    /// answered for itself (see [`Terminal::set_default_colors`]).
    default_colors: Option<Arc<TermColors>>,
}

impl fmt::Debug for Terminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Terminal")
            .field("dims", &self.dims())
            .field("history_len", &self.history_len())
            .field("title", &self.title)
            .finish_non_exhaustive()
    }
}

impl Terminal {
    pub fn new(cfg: TerminalConfig) -> Terminal {
        let scrolling_history = cfg.scrollback_lines.min(MAX_SCROLLBACK_LINES);
        if scrolling_history < cfg.scrollback_lines {
            tracing::warn!(
                requested = cfg.scrollback_lines,
                used = scrolling_history,
                "scrollback clamped"
            );
        }
        let config = Config {
            scrolling_history,
            kitty_keyboard: cfg.kitty_keyboard,
            // Loads are never allowed; stores are gated in `drain_events`.
            osc52: Osc52::OnlyCopy,
            ..Config::default()
        };
        let listener = Listener::default();
        let events = Arc::clone(&listener.queue);
        let mut term = Term::new(config, &term_size(cfg.cols, cfg.rows), listener);
        // Consume alacritty's initial "everything damaged" state; the first
        // `take_damage` reports `Full` through `damage_full` instead.
        let _ = term.damage();
        term.reset_damage();
        Terminal {
            term,
            parser: Processor::new(),
            events,
            prescanner: OscPrescanner::new(),
            interner: StyleInterner::new(),
            damage_full: true,
            damage_lines: BTreeSet::new(),
            title: None,
            allow_clipboard_store: false,
            default_colors: None,
        }
    }

    /// Prescan OSC side channel, then advance the VT parser.
    pub fn process(&mut self, bytes: &[u8]) -> ProcessOutcome {
        let osc = self.prescanner.scan(bytes);
        // Mirror alacritty's event loop: an expired synchronized update is
        // ended before more bytes are parsed.
        self.end_sync_if_expired(Instant::now());
        let was_syncing = self.sync_deadline().is_some();
        let buffered_before = self.parser.sync_bytes_count();
        self.parser.advance(&mut self.term, bytes);
        // Inside a synchronized update vte only buffers bytes and leaves the
        // `Term` untouched; collecting would just re-damage the cursor row.
        let fully_buffered = was_syncing
            && self.sync_deadline().is_some()
            && self.parser.sync_bytes_count() == buffered_before + bytes.len();
        if !fully_buffered {
            self.collect_damage();
        }
        ProcessOutcome {
            osc,
            events: self.drain_events(),
        }
    }

    /// Deadline of the synchronized update (DEC mode 2026) currently holding
    /// back output, if any. Once it has passed, call
    /// [`Terminal::flush_expired_sync`] (a later `process` call also ends it).
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// End a synchronized update whose deadline has passed and apply the
    /// output it held back. No-op (empty outcome) otherwise.
    pub fn flush_expired_sync(&mut self) -> ProcessOutcome {
        if !self.end_sync_if_expired(Instant::now()) {
            return ProcessOutcome::default();
        }
        self.collect_damage();
        ProcessOutcome {
            osc: Vec::new(),
            events: self.drain_events(),
        }
    }

    /// Damage accumulated since the previous call (resets it).
    ///
    /// Besides rows whose content changed, `Lines` includes the rows of the
    /// previous and current cursor position whenever output was processed
    /// (alacritty's damage model). Scrolling, resizing, screen switches and
    /// palette changes yield `Full`; the first call after `new` is `Full`.
    pub fn take_damage(&mut self) -> Damage {
        if std::mem::take(&mut self.damage_full) {
            self.damage_lines.clear();
            return Damage::Full;
        }
        if self.damage_lines.is_empty() {
            return Damage::None;
        }
        Damage::Lines(std::mem::take(&mut self.damage_lines).into_iter().collect())
    }

    /// Resize the grid (minimum 2 columns × 1 row). Marks full damage when
    /// the size changed.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let size = term_size(cols, rows);
        if size.columns == self.term.columns() && size.screen_lines == self.term.screen_lines() {
            return;
        }
        self.term.resize(size);
        let _ = self.term.damage();
        self.term.reset_damage();
        self.damage_full = true;
        self.damage_lines.clear();
    }

    pub fn dims(&self) -> Dims {
        Dims {
            cols: to_u16(self.term.columns()),
            rows: to_u16(self.term.screen_lines()),
        }
    }

    /// The daemon never scrolls the `Term` (display offset stays 0); clients
    /// scroll by fetching history. So `screen` is always the bottom screen.
    pub fn screen(&mut self) -> ScreenSnapshot {
        let grid = self.term.grid();
        let lines = (0..grid.screen_lines())
            .map(|row| row_to_line(&grid[Line(row as i32)], &mut self.interner))
            .collect();
        let dims = self.dims();
        ScreenSnapshot {
            cols: dims.cols,
            rows: dims.rows,
            lines,
            cursor: self.cursor(),
            modes: self.modes(),
            display_offset: u32::try_from(grid.display_offset()).unwrap_or(u32::MAX),
            history_len: grid.history_size() as u64,
            title: self.title.clone().unwrap_or_default(),
        }
    }

    /// Selected visible rows (for damage deltas). Rows outside the current
    /// screen (e.g. from damage taken before a resize) are skipped.
    pub fn lines(&mut self, rows: &[u16]) -> Vec<(u16, LineSnapshot)> {
        let grid = self.term.grid();
        let screen_lines = grid.screen_lines();
        rows.iter()
            .filter(|&&row| usize::from(row) < screen_lines)
            .map(|&row| {
                (
                    row,
                    row_to_line(&grid[Line(i32::from(row))], &mut self.interner),
                )
            })
            .collect()
    }

    /// Number of scrollback lines above the screen.
    ///
    /// While the alternate screen is active this is the alternate grid's
    /// history, which is always 0: alacritty keeps the primary scrollback in
    /// its inactive grid and does not expose it.
    pub fn history_len(&self) -> usize {
        self.term.grid().history_size()
    }

    /// Scrollback lines `start..start+count` with 0 = oldest. The range is
    /// clamped to the available history.
    pub fn history(&mut self, start: usize, count: usize) -> Vec<LineSnapshot> {
        let grid = self.term.grid();
        let len = grid.history_size();
        let end = start.saturating_add(count).min(len);
        (start.min(end)..end)
            .map(|index| {
                let line = Line(index as i32 - len as i32);
                row_to_line(&grid[line], &mut self.interner)
            })
            .collect()
    }

    pub fn cursor(&self) -> CursorState {
        let grid = self.term.grid();
        let mut point = grid.cursor.point;
        // Like alacritty's renderer: a cursor on the right half of a wide
        // char is drawn on the wide char itself.
        if point.column.0 > 0 && grid[point].flags.contains(Flags::WIDE_CHAR_SPACER) {
            point.column = Column(point.column.0 - 1);
        }
        let style = self.term.cursor_style();
        let shape = match style.shape {
            AnsiCursorShape::Block => CursorShape::Block,
            AnsiCursorShape::Underline => CursorShape::Underline,
            AnsiCursorShape::Beam => CursorShape::Beam,
            AnsiCursorShape::HollowBlock => CursorShape::HollowBlock,
            AnsiCursorShape::Hidden => CursorShape::Hidden,
        };
        CursorState {
            row: u16::try_from(point.line.0).unwrap_or(0),
            col: to_u16(point.column.0),
            shape,
            visible: self.term.mode().contains(TermMode::SHOW_CURSOR)
                && shape != CursorShape::Hidden,
            blinking: style.blinking,
        }
    }

    /// Input/render modes. The mouse tracking modes (`MOUSE_CLICK` = 1000,
    /// `MOUSE_DRAG` = 1002, `MOUSE_MOTION` = 1003) are mutually exclusive and
    /// each implies click reporting; `SGR_MOUSE` / `UTF8_MOUSE` likewise.
    pub fn modes(&self) -> TermModes {
        let mode = *self.term.mode();
        MODE_MAP
            .iter()
            .filter(|(from, _)| mode.contains(*from))
            .fold(TermModes::empty(), |acc, (_, to)| acc | *to)
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Style interner shared by `screen`, `lines` and `history`.
    pub fn interner(&mut self) -> &mut StyleInterner {
        &mut self.interner
    }

    /// Whether OSC 52 clipboard *stores* are surfaced as
    /// [`TermEvent::ClipboardStore`]. Off by default (DESIGN §11: writes need
    /// explicit opt-in). Clipboard *loads* are always refused.
    pub fn set_clipboard_store_allowed(&mut self, allowed: bool) {
        self.allow_clipboard_store = allowed;
    }

    /// Answer color queries (OSC 4 / 10 / 11 / 12) the application has not
    /// set a color for with `colors`, the ones the GUI paints the grid
    /// with. `None` (the default) answers with Alacritty's default scheme,
    /// which is also what entries missing from `colors.palette` get.
    pub fn set_default_colors(&mut self, colors: Option<Arc<TermColors>>) {
        self.default_colors = colors;
    }

    fn end_sync_if_expired(&mut self, now: Instant) -> bool {
        match self.sync_deadline() {
            Some(deadline) if deadline <= now => {
                self.parser.stop_sync(&mut self.term);
                true
            }
            _ => false,
        }
    }

    /// Move alacritty's damage into the accumulator. Only called after
    /// something may have changed: `Term::damage` always re-damages the
    /// cursor row, so calling it on idle ticks would report phantom damage.
    fn collect_damage(&mut self) {
        let lines: Option<Vec<usize>> = match self.term.damage() {
            TermDamage::Full => None,
            TermDamage::Partial(iter) => Some(iter.map(|bounds| bounds.line).collect()),
        };
        self.term.reset_damage();
        match lines {
            None => {
                self.damage_full = true;
                self.damage_lines.clear();
            }
            Some(lines) if !self.damage_full => {
                self.damage_lines
                    .extend(lines.into_iter().filter_map(|l| u16::try_from(l).ok()));
            }
            Some(_) => {}
        }
    }

    fn drain_events(&mut self) -> Vec<TermEvent> {
        let raw = std::mem::take(&mut *self.events.lock());
        let mut out = Vec::with_capacity(raw.len());
        for event in raw {
            match event {
                Event::Title(title) => {
                    self.title = Some(title.clone());
                    out.push(TermEvent::Title(Some(title)));
                }
                Event::ResetTitle => {
                    self.title = None;
                    out.push(TermEvent::Title(None));
                }
                Event::Bell => out.push(TermEvent::Bell),
                Event::PtyWrite(text) => out.push(TermEvent::PtyWrite(text.into_bytes())),
                Event::ClipboardStore(_, text) => {
                    if self.allow_clipboard_store {
                        out.push(TermEvent::ClipboardStore(text));
                    } else {
                        tracing::debug!(bytes = text.len(), "OSC 52 clipboard store refused");
                    }
                }
                Event::ClipboardLoad(..) => tracing::debug!("OSC 52 clipboard load refused"),
                Event::ColorRequest(index, format) => match self.color_for(index) {
                    Some(color) => out.push(TermEvent::PtyWrite(format(color).into_bytes())),
                    None => tracing::debug!(index, "color query for unknown palette index"),
                },
                Event::TextAreaSizeRequest(format) => {
                    // The daemon has no pixel geometry: report 0×0 pixels.
                    let dims = self.dims();
                    let size = WindowSize {
                        num_lines: dims.rows,
                        num_cols: dims.cols,
                        cell_width: 0,
                        cell_height: 0,
                    };
                    out.push(TermEvent::PtyWrite(format(size).into_bytes()));
                }
                Event::CursorBlinkingChange => out.push(TermEvent::CursorBlinkingChanged),
                Event::ChildExit(status) => {
                    out.push(TermEvent::ChildExit(crate::exit_status_code(status)));
                }
                Event::Wakeup | Event::MouseCursorDirty | Event::Exit => {}
            }
        }
        out
    }

    /// Palette entry for a color query: the application's own override if it
    /// set one (OSC 4/10/11/12), otherwise the GUI's color, otherwise the
    /// default scheme.
    fn color_for(&self, index: usize) -> Option<Rgb> {
        if index >= COLOR_COUNT {
            return None;
        }
        self.term.colors()[index]
            .or_else(|| {
                let gui = self.default_colors.as_deref()?;
                palette::gui_color(gui, index)
            })
            .or_else(|| palette::default_color(index))
    }
}

const MODE_MAP: [(TermMode, TermModes); 17] = [
    (TermMode::ALT_SCREEN, TermModes::ALT_SCREEN),
    (TermMode::BRACKETED_PASTE, TermModes::BRACKETED_PASTE),
    (TermMode::APP_CURSOR, TermModes::APP_CURSOR),
    (TermMode::APP_KEYPAD, TermModes::APP_KEYPAD),
    (TermMode::FOCUS_IN_OUT, TermModes::FOCUS_IN_OUT),
    (TermMode::MOUSE_REPORT_CLICK, TermModes::MOUSE_CLICK),
    (TermMode::MOUSE_DRAG, TermModes::MOUSE_DRAG),
    (TermMode::MOUSE_MOTION, TermModes::MOUSE_MOTION),
    (TermMode::SGR_MOUSE, TermModes::SGR_MOUSE),
    (TermMode::UTF8_MOUSE, TermModes::UTF8_MOUSE),
    (TermMode::ALTERNATE_SCROLL, TermModes::ALTERNATE_SCROLL),
    (TermMode::SHOW_CURSOR, TermModes::SHOW_CURSOR),
    (
        TermMode::DISAMBIGUATE_ESC_CODES,
        TermModes::KITTY_DISAMBIGUATE,
    ),
    (
        TermMode::REPORT_EVENT_TYPES,
        TermModes::KITTY_REPORT_EVENT_TYPES,
    ),
    (
        TermMode::REPORT_ALTERNATE_KEYS,
        TermModes::KITTY_REPORT_ALTERNATE,
    ),
    (
        TermMode::REPORT_ALL_KEYS_AS_ESC,
        TermModes::KITTY_REPORT_ALL_AS_ESC,
    ),
    (
        TermMode::REPORT_ASSOCIATED_TEXT,
        TermModes::KITTY_REPORT_ASSOC_TEXT,
    ),
];

fn term_size(cols: u16, rows: u16) -> TermSize {
    TermSize::new(
        usize::from(cols).max(MIN_COLUMNS),
        usize::from(rows).max(MIN_SCREEN_LINES),
    )
}

fn to_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    use berth_core::{CellFlags, Color, Style};

    fn term(cols: u16, rows: u16) -> Terminal {
        Terminal::new(TerminalConfig {
            cols,
            rows,
            ..TerminalConfig::default()
        })
    }

    fn pty_writes(outcome: &ProcessOutcome) -> Vec<String> {
        outcome
            .events
            .iter()
            .filter_map(|event| match event {
                TermEvent::PtyWrite(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn hello_lands_on_row_zero() {
        let mut t = term(80, 24);
        t.process(b"hello\r\n");
        assert_eq!(t.lines(&[0])[0].1.text(), "hello");
        let screen = t.screen();
        assert_eq!(screen.lines.len(), 24);
        assert_eq!(screen.lines[0].text(), "hello");
        assert_eq!((screen.cursor.row, screen.cursor.col), (1, 0));
    }

    #[test]
    fn sgr_bold_red_style() {
        let mut t = term(80, 24);
        t.process(b"\x1b[1;31mX\x1b[0m");
        let line = t.lines(&[0]).remove(0).1;
        assert_eq!(line.runs.len(), 1);
        assert_eq!(line.runs[0].text, "X");
        let style = t.interner().table().get(line.runs[0].style);
        assert_eq!(
            style,
            Style {
                fg: Color::Indexed(1),
                flags: CellFlags::BOLD,
                ..Style::default()
            }
        );
    }

    #[test]
    fn wide_chars_form_one_four_cell_run() {
        let mut t = term(80, 24);
        t.process("你好".as_bytes());
        let line = t.lines(&[0]).remove(0).1;
        assert_eq!(line.runs.len(), 1);
        assert_eq!(line.runs[0].text, "你好");
        assert_eq!(line.runs[0].cells, 4);
        // The cursor sits right after the two wide chars.
        assert_eq!(t.cursor().col, 4);
    }

    #[test]
    fn scrollback_history_order() {
        let mut t = term(80, 24);
        let input = (1..=30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\r\n");
        t.process(input.as_bytes());
        assert_eq!(t.history_len(), 6);
        let first = t.history(0, 1);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].text(), "line 1");
        let all = t.history(0, 100);
        assert_eq!(
            all.iter().map(LineSnapshot::text).collect::<Vec<_>>(),
            (1..=6).map(|i| format!("line {i}")).collect::<Vec<_>>()
        );
        assert_eq!(t.history(5, 3)[0].text(), "line 6");
        assert!(t.history(6, 1).is_empty());
        let screen = t.screen();
        assert_eq!(screen.history_len, 6);
        assert_eq!(screen.lines[0].text(), "line 7");
        assert_eq!(screen.lines[23].text(), "line 30");
    }

    #[test]
    fn scrollback_is_bounded() {
        let mut t = Terminal::new(TerminalConfig {
            cols: 20,
            rows: 5,
            scrollback_lines: 10,
            kitty_keyboard: true,
        });
        let input = (1..=100)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\r\n");
        t.process(input.as_bytes());
        assert_eq!(t.history_len(), 10);
        assert_eq!(t.history(0, 1)[0].text(), "l86");
    }

    #[test]
    fn alt_screen_mode() {
        let mut t = term(80, 24);
        t.process(b"primary");
        t.process(b"\x1b[?1049h");
        assert!(t.modes().contains(TermModes::ALT_SCREEN));
        assert_eq!(t.screen().lines[0].text(), "");
        t.process(b"\x1b[?1049l");
        assert!(!t.modes().contains(TermModes::ALT_SCREEN));
        assert_eq!(t.screen().lines[0].text(), "primary");
    }

    #[test]
    fn input_modes_map() {
        let mut t = term(80, 24);
        let defaults = t.modes();
        assert!(defaults.contains(TermModes::SHOW_CURSOR | TermModes::ALTERNATE_SCROLL));
        t.process(b"\x1b[?1000h");
        assert!(t.modes().contains(TermModes::MOUSE_CLICK));
        // Mouse tracking modes replace each other (alacritty semantics).
        t.process(b"\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[?1h\x1b=\x1b[?1004h");
        let modes = t.modes();
        assert!(!modes.contains(TermModes::MOUSE_CLICK));
        for expected in [
            TermModes::BRACKETED_PASTE,
            TermModes::MOUSE_DRAG,
            TermModes::SGR_MOUSE,
            TermModes::APP_CURSOR,
            TermModes::APP_KEYPAD,
            TermModes::FOCUS_IN_OUT,
        ] {
            assert!(
                modes.contains(expected),
                "{expected:?} missing from {modes:?}"
            );
        }
        assert!(modes.mouse_reporting());
    }

    #[test]
    fn kitty_keyboard_flags_follow_config() {
        let mut t = term(80, 24);
        t.process(b"\x1b[>1u");
        assert!(t.modes().contains(TermModes::KITTY_DISAMBIGUATE));
        t.process(b"\x1b[=31;1u");
        let modes = t.modes();
        for expected in [
            TermModes::KITTY_DISAMBIGUATE,
            TermModes::KITTY_REPORT_EVENT_TYPES,
            TermModes::KITTY_REPORT_ALTERNATE,
            TermModes::KITTY_REPORT_ALL_AS_ESC,
            TermModes::KITTY_REPORT_ASSOC_TEXT,
        ] {
            assert!(
                modes.contains(expected),
                "{expected:?} missing from {modes:?}"
            );
        }
        let mut off = Terminal::new(TerminalConfig {
            kitty_keyboard: false,
            ..TerminalConfig::default()
        });
        off.process(b"\x1b[>1u");
        assert!(!off.modes().contains(TermModes::KITTY_DISAMBIGUATE));
    }

    #[test]
    fn cursor_position_report_is_a_pty_write() {
        let mut t = term(80, 24);
        let outcome = t.process(b"ab\x1b[6n");
        let writes = pty_writes(&outcome);
        assert_eq!(writes, vec!["\x1b[1;3R".to_string()]);
        assert!(writes[0].starts_with("\x1b["));
    }

    #[test]
    fn device_attributes_reply() {
        let mut t = term(80, 24);
        let outcome = t.process(b"\x1b[c");
        assert_eq!(pty_writes(&outcome), vec!["\x1b[?6c".to_string()]);
    }

    #[test]
    fn color_queries_use_overrides_then_default_palette() {
        let mut t = term(80, 24);
        let outcome = t.process(b"\x1b]11;?\x07\x1b]4;1;?\x1b\\");
        assert_eq!(
            pty_writes(&outcome),
            vec![
                "\x1b]11;rgb:1818/1818/1818\x07".to_string(),
                "\x1b]4;1;rgb:acac/4242/4242\x1b\\".to_string(),
            ]
        );
        let outcome = t.process(b"\x1b]11;#102030\x07\x1b]11;?\x07");
        assert_eq!(
            pty_writes(&outcome),
            vec!["\x1b]11;rgb:1010/2020/3030\x07".to_string()]
        );
    }

    /// With the GUI's colors set, queries are answered from them: here the
    /// light theme's white background, which is what Codex reads to pick
    /// its input box. The application's own colors still come first, and
    /// palette entries the GUI left out keep the default scheme.
    #[test]
    fn color_queries_answer_with_the_gui_colors() {
        let mut t = term(80, 24);
        t.set_default_colors(Some(Arc::new(TermColors {
            foreground: [0x1f, 0x23, 0x28],
            background: [0xff, 0xff, 0xff],
            cursor: [0x0a, 0x0b, 0x0c],
            palette: vec![[0x38, 0x3a, 0x42], [0xc8, 0x4c, 0x40]],
        })));
        let outcome =
            t.process(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07\x1b]4;1;?\x07\x1b]4;2;?\x07");
        assert_eq!(
            pty_writes(&outcome),
            vec![
                "\x1b]10;rgb:1f1f/2323/2828\x07".to_string(),
                "\x1b]11;rgb:ffff/ffff/ffff\x07".to_string(),
                "\x1b]12;rgb:0a0a/0b0b/0c0c\x07".to_string(),
                "\x1b]4;1;rgb:c8c8/4c4c/4040\x07".to_string(),
                // Past the end of the GUI's palette: Alacritty's green.
                "\x1b]4;2;rgb:9090/a9a9/5959\x07".to_string(),
            ]
        );
        // The application's own background wins; once it resets it
        // (OSC 111), the GUI's is back. (Separate chunks: queries are
        // answered after the chunk that holds them, with its final state.)
        let outcome = t.process(b"\x1b]11;#102030\x07\x1b]11;?\x07");
        assert_eq!(
            pty_writes(&outcome),
            vec!["\x1b]11;rgb:1010/2020/3030\x07".to_string()]
        );
        let outcome = t.process(b"\x1b]111\x07\x1b]11;?\x07");
        assert_eq!(
            pty_writes(&outcome),
            vec!["\x1b]11;rgb:ffff/ffff/ffff\x07".to_string()]
        );
        t.set_default_colors(None);
        let outcome = t.process(b"\x1b]11;?\x07");
        assert_eq!(
            pty_writes(&outcome),
            vec!["\x1b]11;rgb:1818/1818/1818\x07".to_string()]
        );
    }

    #[test]
    fn text_area_size_queries() {
        let mut t = term(100, 30);
        let outcome = t.process(b"\x1b[14t\x1b[18t");
        assert_eq!(
            pty_writes(&outcome),
            vec!["\x1b[4;0;0t".to_string(), "\x1b[8;30;100t".to_string()]
        );
    }

    #[test]
    fn title_events_and_reset() {
        let mut t = term(80, 24);
        let outcome = t.process(b"\x1b[22;0t\x1b]2;build\x07");
        assert_eq!(outcome.events, vec![TermEvent::Title(Some("build".into()))]);
        assert_eq!(t.title(), Some("build"));
        assert_eq!(t.screen().title, "build");
        // Popping the pushed (unset) title resets it.
        let outcome = t.process(b"\x1b[23;0t");
        assert_eq!(outcome.events, vec![TermEvent::Title(None)]);
        assert_eq!(t.title(), None);
    }

    #[test]
    fn bell_and_cursor_blinking_events() {
        let mut t = term(80, 24);
        let outcome = t.process(b"\x07\x1b[5 q");
        assert_eq!(
            outcome.events,
            vec![TermEvent::Bell, TermEvent::CursorBlinkingChanged]
        );
        let cursor = t.cursor();
        assert_eq!(cursor.shape, CursorShape::Beam);
        assert!(cursor.blinking);
    }

    #[test]
    fn cursor_state_tracks_position_and_visibility() {
        let mut t = term(80, 24);
        t.process(b"\x1b[3;5H");
        let cursor = t.cursor();
        assert_eq!((cursor.row, cursor.col, cursor.visible), (2, 4, true));
        assert_eq!(cursor.shape, CursorShape::Block);
        t.process(b"\x1b[?25l");
        assert!(!t.cursor().visible);
        assert!(!t.modes().contains(TermModes::SHOW_CURSOR));
        // A cursor placed on a wide char's spacer reports the wide char.
        t.process("\x1b[?25h\x1b[5;1H你\x1b[5;2H".as_bytes());
        assert_eq!((t.cursor().row, t.cursor().col), (4, 0));
    }

    #[test]
    fn clipboard_store_requires_opt_in() {
        let mut t = term(80, 24);
        let outcome = t.process(b"\x1b]52;c;aGVsbG8=\x07\x1b]52;c;?\x07");
        assert!(outcome.events.is_empty(), "{:?}", outcome.events);
        t.set_clipboard_store_allowed(true);
        let outcome = t.process(b"\x1b]52;c;aGVsbG8=\x07\x1b]52;c;?\x07");
        assert_eq!(
            outcome.events,
            vec![TermEvent::ClipboardStore("hello".into())]
        );
    }

    #[test]
    fn osc_side_channel_is_reported_and_not_rendered() {
        let mut t = term(80, 24);
        let outcome = t.process(b"\x1b]7;file://host/tmp/a%20b\x07\x1b]133;D;1\x07ok");
        assert_eq!(
            outcome.osc,
            vec![
                OscEvent::Cwd("/tmp/a b".into()),
                OscEvent::Prompt(crate::osc::PromptMark::CommandEnd { exit_code: Some(1) }),
            ]
        );
        assert_eq!(t.screen().lines[0].text(), "ok");
    }

    #[test]
    fn damage_first_frame_full_then_lines() {
        let mut t = term(80, 24);
        t.process(b"one\r\ntwo\r\n");
        assert_eq!(t.take_damage(), Damage::Full);
        assert_eq!(t.take_damage(), Damage::None);
        // The cursor is already on the third row; rewrite that row.
        t.process(b"\x1b[3;1Hthree");
        assert_eq!(t.take_damage(), Damage::Lines(vec![2]));
        assert_eq!(t.take_damage(), Damage::None);
    }

    #[test]
    fn damage_includes_old_and_new_cursor_rows() {
        let mut t = term(80, 24);
        let _ = t.take_damage();
        t.process(b"\x1b[5;1Hx");
        assert_eq!(t.take_damage(), Damage::Lines(vec![0, 4]));
        // Damage accumulates across process calls until taken.
        t.process(b"\x1b[5;1Hy");
        t.process(b"\x1b[7;1Hz");
        assert_eq!(t.take_damage(), Damage::Lines(vec![4, 6]));
    }

    #[test]
    fn scrolling_is_full_damage() {
        let mut t = term(20, 3);
        t.process(b"a\r\nb\r\nc");
        let _ = t.take_damage();
        t.process(b"\r\nd");
        assert_eq!(t.take_damage(), Damage::Full);
    }

    #[test]
    fn resize_updates_dims_and_damages_everything() {
        let mut t = term(80, 24);
        t.process(b"hello");
        let _ = t.take_damage();
        t.resize(100, 30);
        assert_eq!(
            t.dims(),
            Dims {
                cols: 100,
                rows: 30
            }
        );
        assert_eq!(t.take_damage(), Damage::Full);
        let screen = t.screen();
        assert_eq!(
            (screen.cols, screen.rows, screen.lines.len()),
            (100, 30, 30)
        );
        assert_eq!(screen.lines[0].text(), "hello");
        // Same size again: no damage.
        t.resize(100, 30);
        assert_eq!(t.take_damage(), Damage::None);
        // Degenerate sizes are clamped to alacritty's minimum.
        t.resize(0, 0);
        assert_eq!(t.dims(), Dims { cols: 2, rows: 1 });
        // Rows beyond the screen are skipped rather than panicking.
        assert_eq!(t.lines(&[0, 5]).len(), 1);
    }

    #[test]
    fn synchronized_update_is_held_until_end_marker() {
        let mut t = term(80, 24);
        t.process(b"\x1b[?2026hframe");
        assert!(t.sync_deadline().is_some());
        assert_eq!(t.screen().lines[0].text(), "");
        let _ = t.take_damage();
        // Fully buffered chunks change nothing on screen: no damage.
        t.process(b" in");
        assert_eq!(t.take_damage(), Damage::None);
        t.process(b" done\x1b[?2026l");
        assert!(t.sync_deadline().is_none());
        assert_eq!(t.screen().lines[0].text(), "frame in done");
        assert_eq!(t.take_damage(), Damage::Lines(vec![0]));
    }

    #[test]
    fn synchronized_update_is_flushed_after_deadline() {
        let mut t = term(80, 24);
        let _ = t.take_damage();
        t.process(b"\x1b[?2026hstuck\x1b[6n");
        assert!(
            pty_writes(&t.flush_expired_sync()).is_empty(),
            "not expired yet"
        );
        let deadline = t.sync_deadline().expect("sync pending");
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        let outcome = t.flush_expired_sync();
        assert_eq!(pty_writes(&outcome), vec!["\x1b[1;6R".to_string()]);
        assert!(t.sync_deadline().is_none());
        assert_eq!(t.screen().lines[0].text(), "stuck");
        assert_ne!(t.take_damage(), Damage::None);
    }

    #[test]
    fn scrollback_config_is_clamped() {
        let t = Terminal::new(TerminalConfig {
            scrollback_lines: MAX_SCROLLBACK_LINES * 10,
            ..TerminalConfig::default()
        });
        assert_eq!(t.history_len(), 0);
        assert!(format!("{t:?}").contains("Terminal"));
    }

    #[test]
    fn terminal_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Terminal>();
    }
}
