//! Client-side terminal view state.
//!
//! In M0 there is no daemon: the screen comes from `fixture`, and keyboard /
//! IME input is echoed locally on the prompt row so the whole input path
//! (winit event → xterm bytes → terminal) is visible. In M2 `input_bytes`
//! becomes `Request::Input` and the screen is replaced by `Event::Screen`.

use berth_core::{
    char_cells, CellFlags, Color, LineSnapshot, ScreenSnapshot, Style, StyleId, StyleInterner,
    StyleTable,
};

use crate::fixture::{Fixture, LineWriter, Selection, PROMPT, PROMPT_ROW, STATUS_ROW};
use crate::input::caret_notation;

pub struct Terminal {
    pub screen: ScreenSnapshot,
    interner: StyleInterner,
    pub selection: Option<Selection>,
    echo: String,
    last_input: Option<(String, Vec<u8>)>,
    prompt_style: StyleId,
    status_style: StyleId,
    status_hex_style: StyleId,
    /// Grid size the window currently fits (shown on the status row; the
    /// PTY resize request in M2).
    grid: (u16, u16),
}

impl Terminal {
    pub fn fixture() -> Self {
        let Fixture {
            screen,
            mut interner,
            selection,
        } = Fixture::build();
        let prompt_style = interner.intern(Style {
            fg: Color::Indexed(5),
            flags: CellFlags::BOLD,
            ..Default::default()
        });
        let status_style = interner.intern(Style {
            flags: CellFlags::DIM | CellFlags::ITALIC,
            ..Default::default()
        });
        let status_hex_style = interner.intern(Style {
            fg: Color::Indexed(3),
            ..Default::default()
        });
        let mut term = Self {
            screen,
            interner,
            selection,
            echo: String::new(),
            last_input: None,
            prompt_style,
            status_style,
            status_hex_style,
            grid: (0, 0),
        };
        term.grid = (term.screen.cols, term.screen.rows);
        term.refresh_prompt();
        term
    }

    pub fn styles(&self) -> &StyleTable {
        self.interner.table()
    }

    #[cfg(test)]
    pub fn echo_text(&self) -> &str {
        &self.echo
    }

    /// Record the grid size the window fits after a resize.
    pub fn set_grid_size(&mut self, cols: u16, rows: u16) {
        if self.grid != (cols, rows) {
            self.grid = (cols, rows);
            self.refresh_prompt();
        }
    }

    /// Bytes that would be written to the PTY. `desc` names the key for the
    /// status row. Local echo: printable text is appended, Backspace
    /// deletes, Enter clears the line, other controls show in caret form.
    pub fn input_bytes(&mut self, desc: &str, bytes: &[u8]) {
        match bytes {
            [0x7f] | [0x08] => {
                self.echo.pop();
            }
            b"\r" => self.echo.clear(),
            _ => self.echo.push_str(&caret_notation(bytes)),
        }
        self.last_input = Some((desc.to_string(), bytes.to_vec()));
        self.refresh_prompt();
    }

    fn refresh_prompt(&mut self) {
        let cols = self.screen.cols;
        // Status row: what the last key encoded to.
        let mut status = LineWriter::new(cols);
        match &self.last_input {
            Some((desc, bytes)) => {
                let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
                status
                    .put(&format!("input: {desc} → "), self.status_style)
                    .put(&caret_notation(bytes), self.status_hex_style)
                    .put(&format!("  [{}]", hex.join(" ")), self.status_style);
            }
            None => {
                status.put("input: 在此窗口按键或用中文输入法输入，编码后的字节会回显在下一行（本地回显，M0 无 daemon）", self.status_style);
            }
        }
        let size = format!("grid {}×{}", self.grid.0, self.grid.1);
        let size_cells: u16 = size.chars().map(char_cells).sum();
        if status.cells() + size_cells + 2 <= cols {
            status
                .pad_to(cols - size_cells, StyleId::DEFAULT)
                .put(&size, self.status_style);
        }
        // Prompt row: prompt + echo, scrolled so the tail stays visible.
        let prompt_cells: u16 = PROMPT.chars().map(char_cells).sum();
        let room = cols.saturating_sub(prompt_cells + 1);
        let mut shown: Vec<char> = Vec::new();
        let mut used = 0u16;
        for ch in self.echo.chars().rev() {
            let w = char_cells(ch);
            if used + w > room {
                break;
            }
            used += w;
            shown.push(ch);
        }
        shown.reverse();
        let mut prompt = LineWriter::new(cols);
        prompt
            .put(PROMPT, self.prompt_style)
            .put(&shown.iter().collect::<String>(), StyleId::DEFAULT);
        let cursor_col = prompt.cells();
        self.set_line(STATUS_ROW, status.finish());
        self.set_line(PROMPT_ROW, prompt.finish());
        self.screen.cursor.row = PROMPT_ROW;
        self.screen.cursor.col = cursor_col.min(cols.saturating_sub(1));
    }

    fn set_line(&mut self, row: u16, line: LineSnapshot) {
        if let Some(slot) = self.screen.lines.get_mut(row as usize) {
            *slot = line;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_appends_deletes_and_clears() {
        let mut t = Terminal::fixture();
        let start = t.screen.cursor.col;
        t.input_bytes("a", b"a");
        t.input_bytes("中", "中".as_bytes());
        assert_eq!(t.echo_text(), "a中");
        assert_eq!(t.screen.cursor.col, start + 3);
        t.input_bytes("Backspace", b"\x7f");
        assert_eq!(t.echo_text(), "a");
        t.input_bytes("ArrowUp", b"\x1b[A");
        assert_eq!(t.echo_text(), "a^[[A");
        assert!(t.screen.lines[PROMPT_ROW as usize]
            .text()
            .ends_with("a^[[A"));
        assert!(t.screen.lines[STATUS_ROW as usize]
            .text()
            .contains("[1b 5b 41]"));
        t.input_bytes("Enter", b"\r");
        assert_eq!(t.echo_text(), "");
        assert_eq!(t.screen.cursor.col, start);
    }

    #[test]
    fn long_echo_keeps_tail_visible_and_cursor_in_bounds() {
        let mut t = Terminal::fixture();
        for _ in 0..300 {
            t.input_bytes("x", b"x");
        }
        assert!(t.screen.cursor.col < t.screen.cols);
        assert!(t.screen.lines[PROMPT_ROW as usize].cells() <= t.screen.cols);
    }

    #[test]
    fn grid_size_is_shown_on_the_status_row() {
        let mut t = Terminal::fixture();
        assert!(t.screen.lines[STATUS_ROW as usize]
            .text()
            .trim_end()
            .ends_with("grid 120×40"));
        t.set_grid_size(100, 30);
        assert!(t.screen.lines[STATUS_ROW as usize]
            .text()
            .trim_end()
            .ends_with("grid 100×30"));
    }
}
