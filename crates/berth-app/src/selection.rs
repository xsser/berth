//! Local selection on the mirrored lines (integrate.md §2): by character,
//! word (semantic), line, or block. Positions are virtual line indices (the
//! daemon's `[restored) ++ [scrollback) ++ [screen)` space), so a selection
//! stays on its content while the view scrolls.
//!
//! Copy joins the selected lines with `\n`, except after a soft-wrapped
//! line (`LineSnapshot::wrapped`), where the text simply continues. Trailing
//! whitespace is trimmed like `LineSnapshot::text_trimmed`, but not on
//! wrapped lines, where it is part of the text that continues below.

use berth_core::{char_cells, LineSnapshot};

/// Characters that end a word for double-click selection (Alacritty's
/// default `semantic_escape_chars`, plus whitespace).
const SEPARATORS: &str = ",│`|:\"'()[]{}<>\t";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Point {
    pub line: u64,
    pub col: u16,
}

impl Point {
    pub fn new(line: u64, col: u16) -> Point {
        Point { line, col }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    /// Cell by cell (single click + drag).
    Simple,
    /// Whole words (double click).
    Semantic,
    /// Whole lines, following soft wraps (triple click).
    Lines,
    /// A rectangle of cells (⌥ + drag).
    Block,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub kind: SelectionKind,
    pub anchor: Point,
    pub head: Point,
}

/// Selected cells after expansion; both ends inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: Point,
    pub end: Point,
    pub block: bool,
}

impl Selection {
    pub fn new(kind: SelectionKind, at: Point) -> Selection {
        Selection {
            kind,
            anchor: at,
            head: at,
        }
    }

    pub fn update(&mut self, head: Point) {
        self.head = head;
    }

    /// A plain click that has not moved selects nothing.
    pub fn is_empty(&self) -> bool {
        matches!(self.kind, SelectionKind::Simple | SelectionKind::Block)
            && self.anchor == self.head
    }

    /// The selected cells; `line_at` gives the content of a virtual line
    /// (needed to expand words and wrapped lines).
    pub fn span<'a>(&self, line_at: impl Fn(u64) -> Option<&'a LineSnapshot>) -> Option<Span> {
        if self.is_empty() {
            return None;
        }
        let (a, b) = if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        Some(match self.kind {
            SelectionKind::Simple => Span {
                start: a,
                end: b,
                block: false,
            },
            SelectionKind::Block => Span {
                start: Point::new(a.line, a.col.min(b.col)),
                end: Point::new(b.line, a.col.max(b.col)),
                block: true,
            },
            SelectionKind::Semantic => {
                let start = line_at(a.line)
                    .map(|l| word_bounds(l, a.col).0)
                    .unwrap_or(a.col);
                let end = line_at(b.line)
                    .map(|l| word_bounds(l, b.col).1)
                    .unwrap_or(b.col);
                Span {
                    start: Point::new(a.line, start),
                    end: Point::new(b.line, end),
                    block: false,
                }
            }
            SelectionKind::Lines => {
                let mut first = a.line;
                while first > 0 && line_at(first - 1).is_some_and(|l| l.wrapped) {
                    first -= 1;
                }
                let mut last = b.line;
                while line_at(last).is_some_and(|l| l.wrapped) && line_at(last + 1).is_some() {
                    last += 1;
                }
                Span {
                    start: Point::new(first, 0),
                    end: Point::new(last, u16::MAX),
                    block: false,
                }
            }
        })
    }
}

/// One character cell: base char plus any zero-width marks after it.
struct CellChar {
    col: u16,
    width: u16,
    text: String,
}

fn cell_chars(line: &LineSnapshot) -> Vec<CellChar> {
    let mut out: Vec<CellChar> = Vec::new();
    let mut col = 0u16;
    for run in &line.runs {
        for ch in run.text.chars() {
            let w = char_cells(ch);
            if w == 0 {
                if let Some(last) = out.last_mut() {
                    last.text.push(ch);
                }
                continue;
            }
            out.push(CellChar {
                col,
                width: w,
                text: ch.to_string(),
            });
            col = col.saturating_add(w);
        }
    }
    out
}

fn is_word(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|c| !c.is_whitespace() && !SEPARATORS.contains(c))
}

/// Inclusive column range of the word under `col` (or just `col`).
pub fn word_bounds(line: &LineSnapshot, col: u16) -> (u16, u16) {
    let cells = cell_chars(line);
    let Some(idx) = cells
        .iter()
        .position(|c| col >= c.col && col < c.col + c.width)
    else {
        return (col, col);
    };
    let last_col = |c: &CellChar| c.col + c.width - 1;
    if !is_word(&cells[idx].text) {
        return (cells[idx].col, last_col(&cells[idx]));
    }
    let mut lo = idx;
    while lo > 0
        && is_word(&cells[lo - 1].text)
        && cells[lo - 1].col + cells[lo - 1].width == cells[lo].col
    {
        lo -= 1;
    }
    let mut hi = idx;
    while hi + 1 < cells.len()
        && is_word(&cells[hi + 1].text)
        && cells[hi].col + cells[hi].width == cells[hi + 1].col
    {
        hi += 1;
    }
    (cells[lo].col, last_col(&cells[hi]))
}

/// Text of the cells overlapping `[c0, c1]`.
fn cells_text(line: &LineSnapshot, c0: u16, c1: u16) -> String {
    cell_chars(line)
        .into_iter()
        .filter(|c| c.col <= c1 && c.col + c.width > c0)
        .map(|c| c.text)
        .collect()
}

/// The selected text, ready for the clipboard.
pub fn selection_text<'a>(
    span: &Span,
    line_at: impl Fn(u64) -> Option<&'a LineSnapshot>,
) -> String {
    let mut out = String::new();
    let mut v = span.start.line;
    loop {
        let line = line_at(v);
        let (c0, c1) = if span.block {
            (span.start.col, span.end.col)
        } else {
            (
                if v == span.start.line {
                    span.start.col
                } else {
                    0
                },
                if v == span.end.line {
                    span.end.col
                } else {
                    u16::MAX
                },
            )
        };
        let mut text = line.map(|l| cells_text(l, c0, c1)).unwrap_or_default();
        let last = v >= span.end.line;
        let joined = !span.block && !last && line.is_some_and(|l| l.wrapped);
        if !joined {
            let trimmed = text.trim_end().len();
            text.truncate(trimmed);
        }
        out.push_str(&text);
        if last {
            break;
        }
        if !joined {
            out.push('\n');
        }
        v += 1;
    }
    out
}

/// Per visible row, the selected column range (for the renderer).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionSpans {
    rows: Vec<Option<(u16, u16)>>,
}

impl SelectionSpans {
    /// Spans for `rows` visible rows starting at virtual line `top`.
    pub fn visible(span: &Span, top: u64, rows: u16, cols: u16) -> SelectionSpans {
        let last_col = cols.saturating_sub(1);
        let rows = (0..rows as u64)
            .map(|r| {
                let v = top + r;
                if v < span.start.line || v > span.end.line {
                    return None;
                }
                if span.block {
                    return Some((span.start.col.min(last_col), span.end.col.min(last_col)));
                }
                let c0 = if v == span.start.line {
                    span.start.col
                } else {
                    0
                };
                let c1 = if v == span.end.line {
                    span.end.col
                } else {
                    last_col
                };
                Some((c0.min(last_col), c1.min(last_col)))
            })
            .collect();
        SelectionSpans { rows }
    }

    pub fn contains(&self, row: u16, col: u16) -> bool {
        self.rows
            .get(row as usize)
            .copied()
            .flatten()
            .is_some_and(|(a, b)| col >= a && col <= b)
    }

    pub fn is_empty(&self) -> bool {
        self.rows.iter().all(Option::is_none)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_core::StyleId;
    use std::collections::BTreeMap;

    fn line(t: &str, wrapped: bool) -> LineSnapshot {
        let mut l = LineSnapshot::blank();
        l.push_str(t, StyleId::DEFAULT);
        l.wrapped = wrapped;
        l
    }

    fn doc(lines: &[(&str, bool)]) -> BTreeMap<u64, LineSnapshot> {
        lines
            .iter()
            .enumerate()
            .map(|(i, (t, w))| (i as u64 + 100, line(t, *w)))
            .collect()
    }

    fn copy(d: &BTreeMap<u64, LineSnapshot>, sel: Selection) -> String {
        let span = sel.span(|v| d.get(&v)).expect("non-empty");
        selection_text(&span, |v| d.get(&v))
    }

    #[test]
    fn simple_selection_across_lines_trims_and_joins() {
        let d = doc(&[
            ("hello world   ", false),
            ("second line", false),
            ("third", false),
        ]);
        let mut s = Selection::new(SelectionKind::Simple, Point::new(100, 6));
        assert!(s.is_empty());
        s.update(Point::new(101, 5));
        assert_eq!(copy(&d, s), "world\nsecond");
        // Dragging backwards selects the same cells.
        let mut back = Selection::new(SelectionKind::Simple, Point::new(101, 5));
        back.update(Point::new(100, 6));
        assert_eq!(copy(&d, back), "world\nsecond");
    }

    #[test]
    fn wrapped_lines_continue_without_newline() {
        let d = doc(&[("abcdefghij", true), ("klm", false), ("next", false)]);
        let mut s = Selection::new(SelectionKind::Simple, Point::new(100, 0));
        s.update(Point::new(102, 3));
        assert_eq!(copy(&d, s), "abcdefghijklm\nnext");
        // A wrap point on a space keeps the space.
        let d = doc(&[("hello ", true), ("world", false)]);
        let mut s = Selection::new(SelectionKind::Simple, Point::new(100, 0));
        s.update(Point::new(101, 4));
        assert_eq!(copy(&d, s), "hello world");
    }

    #[test]
    fn wide_characters_map_to_two_cells() {
        let d = doc(&[("a中文b", false)]);
        // Cells: a=0, 中=1-2, 文=3-4, b=5. Column 2 is the second half of 中.
        let mut s = Selection::new(SelectionKind::Simple, Point::new(100, 2));
        s.update(Point::new(100, 3));
        assert_eq!(copy(&d, s), "中文");
    }

    #[test]
    fn double_click_selects_a_word() {
        let d = doc(&[("ls -la /tmp/foo.txt, next", false)]);
        let s = Selection::new(SelectionKind::Semantic, Point::new(100, 9));
        assert_eq!(copy(&d, s), "/tmp/foo.txt");
        let s = Selection::new(SelectionKind::Semantic, Point::new(100, 19));
        assert_eq!(copy(&d, s), ",");
        assert_eq!(word_bounds(&line("中文 abc", false), 1), (0, 3));
    }

    #[test]
    fn triple_click_selects_the_logical_line() {
        let d = doc(&[
            ("before", false),
            ("long line par", true),
            ("t two", false),
            ("after", false),
        ]);
        let s = Selection::new(SelectionKind::Lines, Point::new(102, 1));
        assert_eq!(copy(&d, s), "long line part two");
    }

    #[test]
    fn block_selection_is_rectangular_with_newlines() {
        let d = doc(&[("0123456789", true), ("abcdefghij", false), ("xy", false)]);
        let mut s = Selection::new(SelectionKind::Block, Point::new(100, 2));
        s.update(Point::new(102, 4));
        assert_eq!(copy(&d, s), "234\ncde\n");
        let span = s.span(|v| d.get(&v)).unwrap();
        let vis = SelectionSpans::visible(&span, 101, 3, 80);
        assert!(vis.contains(0, 3) && !vis.contains(0, 5));
        assert!(vis.contains(1, 2) && !vis.contains(2, 2));
    }

    #[test]
    fn visible_spans_follow_the_scroll_position() {
        let span = Span {
            start: Point::new(10, 5),
            end: Point::new(12, 2),
            block: false,
        };
        let vis = SelectionSpans::visible(&span, 9, 5, 20);
        assert!(!vis.contains(0, 5)); // line 9
        assert!(!vis.contains(1, 4) && vis.contains(1, 5) && vis.contains(1, 19));
        assert!(vis.contains(2, 0) && vis.contains(2, 19));
        assert!(vis.contains(3, 2) && !vis.contains(3, 3));
        assert!(!vis.contains(4, 0));
        assert!(SelectionSpans::visible(&span, 100, 5, 20).is_empty());
    }
}
