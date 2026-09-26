//! `alacritty_terminal` cell/row → `berth_core::LineSnapshot` conversion.
//!
//! Rules:
//! - `Flags::WIDE_CHAR_SPACER` cells are skipped (the wide char before them
//!   already counts 2 cells). `LEADING_WIDE_CHAR_SPACER` is skipped too.
//! - Zero-width chars stored in `cell.zerowidth()` are appended to the run
//!   text after the base char.
//! - `Flags::INVERSE` is kept as a flag (the renderer swaps colors), colors
//!   are converted verbatim: `Named` → `Indexed(0..=15)` / `Default`,
//!   `Indexed(n)` → `Indexed(n)`, `Spec(rgb)` → `Rgb`.
//! - Trailing default-style blank cells are trimmed
//!   (`LineSnapshot::trim_trailing_default`).
//!
//! Column invariant: consumers place characters with `berth_core::char_cells`,
//! so the sum of `char_cells` over every emitted character must equal the grid
//! column of the next cell. The converter therefore emits a blank for any
//! cell whose base character has no width of its own (a TAB stored by `HT`,
//! an orphaned spacer) and skips cells hidden under a preceding wide char.

use alacritty_terminal::grid::Row;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use berth_core::{char_cells, CellFlags, Color, LineSnapshot, Style, StyleId, StyleInterner};

/// alacritty flags → berth flags. Layout flags (`WRAPLINE`, `WIDE_CHAR`,
/// spacers) are structural and handled by `row_to_line`.
const FLAG_MAP: [(Flags, CellFlags); 11] = [
    (Flags::BOLD, CellFlags::BOLD),
    (Flags::ITALIC, CellFlags::ITALIC),
    (Flags::DIM, CellFlags::DIM),
    (Flags::UNDERLINE, CellFlags::UNDERLINE),
    (Flags::DOUBLE_UNDERLINE, CellFlags::DOUBLE_UNDERLINE),
    (Flags::UNDERCURL, CellFlags::UNDERCURL),
    (Flags::DOTTED_UNDERLINE, CellFlags::DOTTED_UNDERLINE),
    (Flags::DASHED_UNDERLINE, CellFlags::DASHED_UNDERLINE),
    (Flags::STRIKEOUT, CellFlags::STRIKEOUT),
    (Flags::INVERSE, CellFlags::INVERSE),
    (Flags::HIDDEN, CellFlags::HIDDEN),
];

/// The alacritty flags that influence `cell_style`.
const STYLE_FLAGS: Flags = Flags::BOLD
    .union(Flags::ITALIC)
    .union(Flags::DIM)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::STRIKEOUT)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN);

const SPACER_FLAGS: Flags = Flags::WIDE_CHAR_SPACER.union(Flags::LEADING_WIDE_CHAR_SPACER);

/// Convert an alacritty/vte color. `Foreground`/`Background`/`Cursor` (and
/// the bright/dim foreground variants) become `Color::Default`; `Bright*`
/// map to 8..=15; `Dim*` map to their base color 0..=7 (dimming itself is
/// carried by `CellFlags::DIM`).
pub fn convert_color(color: AnsiColor) -> Color {
    match color {
        AnsiColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(index) => Color::Indexed(index),
        AnsiColor::Named(named) => named_color(named),
    }
}

fn named_color(named: NamedColor) -> Color {
    let index = match named {
        NamedColor::Black | NamedColor::DimBlack => 0,
        NamedColor::Red | NamedColor::DimRed => 1,
        NamedColor::Green | NamedColor::DimGreen => 2,
        NamedColor::Yellow | NamedColor::DimYellow => 3,
        NamedColor::Blue | NamedColor::DimBlue => 4,
        NamedColor::Magenta | NamedColor::DimMagenta => 5,
        NamedColor::Cyan | NamedColor::DimCyan => 6,
        NamedColor::White | NamedColor::DimWhite => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        NamedColor::Foreground
        | NamedColor::Background
        | NamedColor::Cursor
        | NamedColor::BrightForeground
        | NamedColor::DimForeground => return Color::Default,
    };
    Color::Indexed(index)
}

fn convert_flags(flags: Flags) -> CellFlags {
    FLAG_MAP
        .iter()
        .filter(|(from, _)| flags.contains(*from))
        .fold(CellFlags::empty(), |acc, (_, to)| acc | *to)
}

/// Convert one cell's attributes to a `Style` (no interning).
///
/// An underline color (SGR 58) only matters while an underline is drawn, so
/// it is dropped from cells without any underline flag; this keeps visually
/// identical cells on one `StyleId`.
pub fn cell_style(cell: &Cell) -> Style {
    let flags = convert_flags(cell.flags);
    let underline = if flags.intersects(CellFlags::ANY_UNDERLINE) {
        cell.underline_color().map_or(Color::Default, convert_color)
    } else {
        Color::Default
    };
    Style {
        fg: convert_color(cell.fg),
        bg: convert_color(cell.bg),
        underline,
        flags,
    }
}

/// Raw attributes that fully determine `cell_style`; comparing these is much
/// cheaper than hashing a `Style` for every cell.
#[derive(Clone, Copy, PartialEq, Eq)]
struct StyleKey {
    fg: AnsiColor,
    bg: AnsiColor,
    flags: Flags,
    underline: Option<AnsiColor>,
}

impl StyleKey {
    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & STYLE_FLAGS,
            underline: cell.underline_color(),
        }
    }
}

/// A cell that renders as nothing: blank character, no zero-width
/// characters, default style. Such cells at the end of a row are trimmed.
fn is_default_blank(cell: &Cell) -> bool {
    matches!(cell.c, ' ' | '\t')
        && cell.zerowidth().is_none_or(<[char]>::is_empty)
        && cell_style(cell) == Style::default()
}

/// Convert a grid row to a line, interning styles.
pub fn row_to_line(row: &Row<Cell>, interner: &mut StyleInterner) -> LineSnapshot {
    let cells: &[Cell] = &row[..];
    let wrapped = cells
        .last()
        .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
    let mut line = LineSnapshot {
        runs: Vec::new(),
        wrapped,
    };
    let end = cells
        .iter()
        .rposition(|cell| !is_default_blank(cell))
        .map_or(0, |i| i + 1);

    // Grid column at which the next emitted character will be drawn.
    let mut next_col = 0usize;
    let mut cached: Option<(StyleKey, StyleId)> = None;
    for (col, cell) in cells[..end].iter().enumerate() {
        if col < next_col {
            // Right half of the preceding wide char (its WIDE_CHAR_SPACER).
            continue;
        }
        if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) && col + 1 == cells.len() {
            // Placeholder left in the last column when a wide char wrapped.
            break;
        }

        let key = StyleKey::of(cell);
        let style = match cached {
            Some((cached_key, id)) if cached_key == key => id,
            _ => {
                let id = interner.intern(cell_style(cell));
                cached = Some((key, id));
                id
            }
        };

        let base = if cell.flags.intersects(SPACER_FLAGS) || char_cells(cell.c) == 0 {
            // Orphaned spacer, or a character without its own width (TAB).
            ' '
        } else {
            cell.c
        };
        line.push(base, style);
        next_col = col + usize::from(char_cells(base));
        for &zerowidth in cell.zerowidth().unwrap_or_default() {
            line.push(zerowidth, style);
            next_col += usize::from(char_cells(zerowidth));
        }
    }

    line.trim_trailing_default();
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::Line;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::{Processor, Rgb};
    use berth_core::Run;

    fn term_with(cols: usize, rows: usize, input: &str) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &TermSize::new(cols, rows), VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, input.as_bytes());
        term
    }

    fn line_of(term: &Term<VoidListener>, row: i32, interner: &mut StyleInterner) -> LineSnapshot {
        row_to_line(&term.grid()[Line(row)], interner)
    }

    fn style_of(interner: &StyleInterner, run: &Run) -> Style {
        interner.table().get(run.style)
    }

    #[test]
    fn plain_text_trims_trailing_blanks() {
        let term = term_with(20, 3, "hello   ");
        let mut interner = StyleInterner::new();
        let line = line_of(&term, 0, &mut interner);
        assert_eq!(
            line.runs,
            vec![Run {
                text: "hello".into(),
                style: StyleId::DEFAULT,
                cells: 5
            }]
        );
        assert!(!line.wrapped);
        assert_eq!(line_of(&term, 1, &mut interner), LineSnapshot::blank());
    }

    #[test]
    fn colors_are_converted_verbatim() {
        let term = term_with(
            40,
            2,
            "\x1b[31mR\x1b[92mG\x1b[38;5;200mI\x1b[38;2;1;2;3mT\x1b[39;44mB\x1b[49;48;5;17mX\x1b[0m",
        );
        let mut interner = StyleInterner::new();
        let line = line_of(&term, 0, &mut interner);
        let styles: Vec<(String, Color, Color)> = line
            .runs
            .iter()
            .map(|run| {
                let style = style_of(&interner, run);
                (run.text.clone(), style.fg, style.bg)
            })
            .collect();
        assert_eq!(
            styles,
            vec![
                ("R".into(), Color::Indexed(1), Color::Default),
                ("G".into(), Color::Indexed(10), Color::Default),
                ("I".into(), Color::Indexed(200), Color::Default),
                ("T".into(), Color::Rgb(1, 2, 3), Color::Default),
                ("B".into(), Color::Default, Color::Indexed(4)),
                ("X".into(), Color::Default, Color::Indexed(17)),
            ]
        );
    }

    #[test]
    fn named_color_mapping() {
        assert_eq!(
            convert_color(AnsiColor::Named(NamedColor::Foreground)),
            Color::Default
        );
        assert_eq!(
            convert_color(AnsiColor::Named(NamedColor::Background)),
            Color::Default
        );
        assert_eq!(
            convert_color(AnsiColor::Named(NamedColor::BrightBlack)),
            Color::Indexed(8)
        );
        assert_eq!(
            convert_color(AnsiColor::Named(NamedColor::BrightWhite)),
            Color::Indexed(15)
        );
        assert_eq!(
            convert_color(AnsiColor::Named(NamedColor::DimRed)),
            Color::Indexed(1)
        );
        assert_eq!(
            convert_color(AnsiColor::Named(NamedColor::DimForeground)),
            Color::Default
        );
        assert_eq!(
            convert_color(AnsiColor::Spec(Rgb { r: 9, g: 8, b: 7 })),
            Color::Rgb(9, 8, 7)
        );
    }

    #[test]
    fn attribute_flags_and_underline_color() {
        let term = term_with(
            40,
            2,
            "\x1b[1ma\x1b[0;3mb\x1b[0;2;31mc\x1b[0;4:3;58;2;255;0;0md\x1b[0;4:2me\x1b[0;4:4mf\
             \x1b[0;4:5mg\x1b[0;9mh\x1b[0;7mi\x1b[0;8mj\x1b[0;58;5;1mk\x1b[0m",
        );
        let mut interner = StyleInterner::new();
        let line = line_of(&term, 0, &mut interner);
        let got: Vec<(String, Style)> = line
            .runs
            .iter()
            .map(|run| (run.text.clone(), style_of(&interner, run)))
            .collect();
        let flags = |flags| Style {
            flags,
            ..Style::default()
        };
        assert_eq!(
            got,
            vec![
                ("a".into(), flags(CellFlags::BOLD)),
                ("b".into(), flags(CellFlags::ITALIC)),
                (
                    "c".into(),
                    Style {
                        fg: Color::Indexed(1),
                        flags: CellFlags::DIM,
                        ..Style::default()
                    }
                ),
                (
                    "d".into(),
                    Style {
                        underline: Color::Rgb(255, 0, 0),
                        flags: CellFlags::UNDERCURL,
                        ..Style::default()
                    }
                ),
                ("e".into(), flags(CellFlags::DOUBLE_UNDERLINE)),
                ("f".into(), flags(CellFlags::DOTTED_UNDERLINE)),
                ("g".into(), flags(CellFlags::DASHED_UNDERLINE)),
                ("h".into(), flags(CellFlags::STRIKEOUT)),
                ("i".into(), flags(CellFlags::INVERSE)),
                ("j".into(), flags(CellFlags::HIDDEN)),
                // Underline color without an underline is dropped.
                ("k".into(), Style::default()),
            ]
        );
    }

    #[test]
    fn wide_chars_count_two_cells_and_skip_spacers() {
        let term = term_with(20, 2, "你好a");
        let mut interner = StyleInterner::new();
        let line = line_of(&term, 0, &mut interner);
        assert_eq!(
            line.runs,
            vec![Run {
                text: "你好a".into(),
                style: StyleId::DEFAULT,
                cells: 5
            }]
        );
    }

    #[test]
    fn zero_width_chars_join_their_base() {
        let term = term_with(20, 2, "e\u{301}x\u{200d}\u{fe0f}!");
        let mut interner = StyleInterner::new();
        let line = line_of(&term, 0, &mut interner);
        assert_eq!(line.text(), "e\u{301}x\u{200d}\u{fe0f}!");
        assert_eq!(line.cells(), 3);
    }

    #[test]
    fn wrapline_sets_wrapped() {
        let term = term_with(10, 3, "0123456789abc");
        let mut interner = StyleInterner::new();
        let first = line_of(&term, 0, &mut interner);
        let second = line_of(&term, 1, &mut interner);
        assert!(first.wrapped);
        assert_eq!(first.text(), "0123456789");
        assert!(!second.wrapped);
        assert_eq!(second.text(), "abc");
    }

    #[test]
    fn leading_wide_char_spacer_is_skipped() {
        // The wide char does not fit in the last column, so it wraps and
        // leaves a LEADING_WIDE_CHAR_SPACER placeholder behind.
        let term = term_with(5, 3, "abcd你");
        assert!(term.grid()[Line(0)][alacritty_terminal::index::Column(4)]
            .flags
            .contains(Flags::LEADING_WIDE_CHAR_SPACER));
        let mut interner = StyleInterner::new();
        let first = line_of(&term, 0, &mut interner);
        assert!(first.wrapped);
        assert_eq!(first.text(), "abcd");
        assert_eq!(first.cells(), 4);
        assert_eq!(line_of(&term, 1, &mut interner).text(), "你");
    }

    #[test]
    fn tab_cells_keep_column_alignment() {
        let term = term_with(20, 2, "a\tb");
        let mut interner = StyleInterner::new();
        let line = line_of(&term, 0, &mut interner);
        assert_eq!(line.text(), "a       b");
        assert_eq!(line.cells(), 9, "b must land on the tab stop at column 8");
    }

    #[test]
    fn styled_trailing_blanks_are_kept() {
        let term = term_with(20, 2, "x\x1b[41m  \x1b[0m   ");
        let mut interner = StyleInterner::new();
        let line = line_of(&term, 0, &mut interner);
        assert_eq!(line.runs.len(), 2);
        assert_eq!(line.runs[1].text, "  ");
        assert_eq!(style_of(&interner, &line.runs[1]).bg, Color::Indexed(1));
        assert_eq!(line.cells(), 3);
    }

    #[test]
    fn styles_are_interned_once_per_distinct_style() {
        let term = term_with(20, 3, "\x1b[31mab\x1b[0mc\x1b[31md\r\n\x1b[31me");
        let mut interner = StyleInterner::new();
        let first = line_of(&term, 0, &mut interner);
        let second = line_of(&term, 1, &mut interner);
        assert_eq!(first.runs.len(), 3);
        assert_eq!(first.runs[0].style, first.runs[2].style);
        assert_eq!(first.runs[0].style, second.runs[0].style);
        assert_eq!(interner.take_pending().len(), 1);
    }
}
