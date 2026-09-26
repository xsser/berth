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

use alacritty_terminal::grid::Row;
use alacritty_terminal::term::cell::Cell;
use berth_core::{LineSnapshot, Style, StyleInterner};

/// Convert one cell's attributes to a `Style` (no interning).
pub fn cell_style(_cell: &Cell) -> Style {
    todo!("berth-vt: cell_style")
}

/// Convert a grid row to a line, interning styles.
pub fn row_to_line(_row: &Row<Cell>, _interner: &mut StyleInterner) -> LineSnapshot {
    todo!("berth-vt: row_to_line")
}
