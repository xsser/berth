//! Client-side mirror of the focused session (integrate.md §1–§2).
//!
//! The daemon sends the visible screen as `Screen` updates (full or per-row
//! deltas, `seq` monotonic per session, style deltas) and answers
//! `FetchLines` from the virtual line space `[restored prefix) ++ [live
//! scrollback) ++ [screen rows)`, in which screen row `r` is line
//! `history_len + r`.
//!
//! Scrolling is local: `display_offset` counts lines above the bottom and
//! the rows above the live screen come from a sparse cache of history lines
//! fetched in pages of at most [`FETCH_PAGE`] (≤ the daemon's 5000-line cap),
//! requested once the view gets within [`PREFETCH_MARGIN`] lines of what is
//! cached. While scrolled up the view stays on the same content when new
//! lines arrive; at the bottom it follows the output.
//!
//! Only `seq` is trusted (the daemon may merge updates): an update whose
//! `seq` is not newer is dropped, except the answer to our own `Attach`,
//! which is a fresh baseline.
//!
//! Cache validity: indices shift when the scrollback is reflowed (resize) or
//! trimmed (`clear`, alternate screen) — both clear the cache. At the
//! scrollback limit the daemon drops its oldest line on every new one while
//! `history_len` stays put; that cannot be told apart from an in-place
//! redraw, so a full redraw with an unchanged `history_len` while scrolled
//! up re-fetches the visible history rows (throttled).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use berth_core::{
    CursorState, Dims, LineSnapshot, ScreenSnapshot, ScreenUpdate, SessionId, Style, StyleId,
    StyleTable, TermModes,
};

use crate::selection::{Selection, SelectionSpans, Span};

/// Lines per prefetch request (the daemon caps one reply at 5000).
pub const FETCH_PAGE: u32 = 2000;
/// Prefetch once the top of the view is this close to the cache boundary.
pub const PREFETCH_MARGIN: u64 = 200;
const REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const FETCH_RETRY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Inflight {
    id: u32,
    start: u64,
    count: u32,
    /// Re-fetch of visible rows (does not count as "available").
    refresh: bool,
}

/// A history range the view wants from the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fetch {
    pub start: u64,
    pub count: u32,
    pub refresh: bool,
}

pub struct SessionView {
    pub id: SessionId,
    styles: StyleTable,
    dims: Dims,
    screen: Vec<LineSnapshot>,
    cursor: CursorState,
    modes: TermModes,
    seq: u64,
    history_len: u64,
    has_screen: bool,
    cache: BTreeMap<u64, LineSnapshot>,
    inflight: Vec<Inflight>,
    display_offset: u64,
    refresh_visible: bool,
    last_refresh: Option<Instant>,
    retry_after: Option<Instant>,
    composed: ScreenSnapshot,
    dirty: bool,
    pub selection: Option<Selection>,
}

impl SessionView {
    pub fn new(id: SessionId) -> SessionView {
        SessionView {
            id,
            styles: StyleTable::new(),
            dims: Dims::default(),
            screen: Vec::new(),
            cursor: CursorState::default(),
            modes: TermModes::empty(),
            seq: 0,
            history_len: 0,
            has_screen: false,
            cache: BTreeMap::new(),
            inflight: Vec::new(),
            display_offset: 0,
            refresh_visible: false,
            last_refresh: None,
            retry_after: None,
            composed: ScreenSnapshot::default(),
            dirty: true,
            selection: None,
        }
    }

    pub fn has_screen(&self) -> bool {
        self.has_screen
    }

    pub fn dims(&self) -> Dims {
        self.dims
    }

    pub fn modes(&self) -> TermModes {
        self.modes
    }

    pub fn styles(&self) -> &StyleTable {
        &self.styles
    }

    pub fn history_len(&self) -> u64 {
        self.history_len
    }

    pub fn display_offset(&self) -> u64 {
        self.display_offset
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Virtual index of the first visible row.
    pub fn top_line(&self) -> u64 {
        self.history_len - self.display_offset
    }

    pub fn cached_lines(&self) -> usize {
        self.cache.len()
    }

    /// Apply a `Screen` update. `baseline`: the answer to our `Attach`
    /// (accepted whatever its `seq`; resets the local view). Returns false
    /// when the update was dropped as out of order.
    pub fn apply_screen(&mut self, u: &ScreenUpdate, baseline: bool) -> bool {
        if self.has_screen && !baseline && u.seq <= self.seq {
            return false;
        }
        self.styles.apply(&u.styles);
        let rows = usize::from(u.dims.rows);
        let fresh = baseline || !self.has_screen;
        if u.dims != self.dims || fresh {
            // New baseline or a resize (which reflows the scrollback):
            // cached history indices no longer mean the same lines.
            if u.dims != self.dims && self.has_screen {
                tracing::debug!(session = %self.id, ?u.dims, "screen resized; history cache cleared");
            }
            self.dims = u.dims;
            self.clear_cache();
            self.display_offset = 0;
            self.selection = None;
        }
        if u.full || self.screen.len() != rows {
            self.screen = vec![LineSnapshot::blank(); rows];
        }
        for (row, line) in &u.lines {
            if let Some(slot) = self.screen.get_mut(usize::from(*row)) {
                *slot = line.clone();
            }
        }
        self.cursor = u.cursor;
        self.modes = u.modes;
        let (old, new) = (self.history_len, u.history_len);
        if !fresh {
            if new < old {
                // Scrollback cleared or trimmed (e.g. alternate screen).
                self.clear_cache();
                self.display_offset = 0;
                self.selection = None;
            } else if new > old && self.display_offset > 0 {
                // Stay on the same content while scrolled up.
                self.display_offset += new - old;
            } else if new == old && u.full && self.display_offset > 0 {
                self.refresh_visible = true;
            }
        }
        self.history_len = new;
        self.display_offset = self.display_offset.min(new);
        self.seq = u.seq;
        self.has_screen = true;
        self.dirty = true;
        true
    }

    /// Lines answering our `FetchLines` request `id`. Answers to requests
    /// made before the cache was cleared are ignored.
    pub fn apply_lines(
        &mut self,
        id: u32,
        start: u64,
        lines: Vec<LineSnapshot>,
        styles: &[(StyleId, Style)],
    ) -> bool {
        let Some(pos) = self.inflight.iter().position(|f| f.id == id) else {
            return false;
        };
        self.inflight.remove(pos);
        self.styles.apply(styles);
        for (i, line) in lines.into_iter().enumerate() {
            let v = start + i as u64;
            if v >= self.history_len {
                break;
            }
            self.cache.insert(v, line);
        }
        self.dirty = true;
        true
    }

    /// A `FetchLines` request failed: forget it and back off.
    pub fn fetch_failed(&mut self, id: u32, now: Instant) {
        self.inflight.retain(|f| f.id != id);
        self.retry_after = Some(now + FETCH_RETRY);
    }

    pub fn note_fetch(&mut self, id: u32, fetch: Fetch) {
        self.inflight.push(Inflight {
            id,
            start: fetch.start,
            count: fetch.count,
            refresh: fetch.refresh,
        });
    }

    fn clear_cache(&mut self) {
        self.cache.clear();
        self.inflight.clear();
        self.refresh_visible = false;
    }

    fn available(&self, v: u64) -> bool {
        self.cache.contains_key(&v)
            || self
                .inflight
                .iter()
                .any(|f| !f.refresh && v >= f.start && v < f.start + u64::from(f.count))
    }

    /// History ranges to request now (visible rows plus the prefetch
    /// margin; each at most [`FETCH_PAGE`] lines).
    pub fn wanted_fetches(&mut self, now: Instant) -> Vec<Fetch> {
        let mut out = Vec::new();
        if !self.has_screen || self.display_offset == 0 {
            return out;
        }
        if self.retry_after.is_some_and(|t| now < t) {
            return out;
        }
        let top = self.top_line();
        let rows = u64::from(self.dims.rows);
        let visible_end = self.history_len.min(top + rows);
        if self.refresh_visible
            && self
                .last_refresh
                .is_none_or(|t| now >= t + REFRESH_INTERVAL)
            && !self.inflight.iter().any(|f| f.refresh)
            && visible_end > top
        {
            self.refresh_visible = false;
            self.last_refresh = Some(now);
            out.push(Fetch {
                start: top,
                count: (visible_end - top) as u32,
                refresh: true,
            });
        }
        let lo = top.saturating_sub(PREFETCH_MARGIN);
        let mut v = visible_end;
        while v > lo {
            if self.available(v - 1) {
                v -= 1;
                continue;
            }
            let end = v;
            let floor = end.saturating_sub(u64::from(FETCH_PAGE));
            let mut start = v - 1;
            while start > floor && !self.available(start - 1) {
                start -= 1;
            }
            out.push(Fetch {
                start,
                count: (end - start) as u32,
                refresh: false,
            });
            v = start;
        }
        out
    }

    /// Scroll by `lines` (positive: up into the history).
    pub fn scroll_by(&mut self, lines: i64) {
        let off = if lines >= 0 {
            self.display_offset.saturating_add(lines as u64)
        } else {
            self.display_offset.saturating_sub(lines.unsigned_abs())
        };
        self.set_display_offset(off);
    }

    pub fn set_display_offset(&mut self, off: u64) {
        let off = off.min(self.history_len);
        if off != self.display_offset {
            self.display_offset = off;
            self.dirty = true;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.set_display_offset(0);
    }

    /// Lines per page (PageUp / PageDown).
    pub fn page(&self) -> i64 {
        i64::from(self.dims.rows.saturating_sub(1).max(1))
    }

    /// Content of virtual line `v` if known (screen or cache).
    pub fn line(&self, v: u64) -> Option<&LineSnapshot> {
        if v >= self.history_len {
            self.screen.get((v - self.history_len) as usize)
        } else {
            self.cache.get(&v)
        }
    }

    /// Selection resolved on the current content.
    pub fn selection_span(&self) -> Option<Span> {
        self.selection.as_ref()?.span(|v| self.line(v))
    }

    pub fn selection_spans(&self) -> Option<SelectionSpans> {
        let span = self.selection_span()?;
        let spans = SelectionSpans::visible(&span, self.top_line(), self.dims.rows, self.dims.cols);
        (!spans.is_empty()).then_some(spans)
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// The last `n` non-blank screen lines (sidebar preview of the focused
    /// session, which has no preview subscription).
    pub fn tail(&self, n: usize) -> Vec<LineSnapshot> {
        let end = self
            .screen
            .iter()
            .rposition(|l| !l.is_blank())
            .map_or(0, |i| i + 1);
        self.screen[end.saturating_sub(n)..end].to_vec()
    }

    /// The visible rows, composed from the cache and the live screen.
    pub fn screen(&mut self) -> &ScreenSnapshot {
        if self.dirty {
            self.compose();
            self.dirty = false;
        }
        &self.composed
    }

    fn compose(&mut self) {
        let rows = usize::from(self.dims.rows);
        let top = self.top_line();
        let mut lines = std::mem::take(&mut self.composed.lines);
        lines.clear();
        for r in 0..rows as u64 {
            lines.push(self.line(top + r).cloned().unwrap_or_default());
        }
        let mut cursor = self.cursor;
        let row = u64::from(cursor.row) + self.display_offset;
        cursor.visible = cursor.visible && row < rows as u64;
        cursor.row = row.min(u64::from(u16::MAX)) as u16;
        self.composed = ScreenSnapshot {
            cols: self.dims.cols,
            rows: self.dims.rows,
            lines,
            cursor,
            modes: self.modes,
            display_offset: self.display_offset.min(u64::from(u32::MAX)) as u32,
            history_len: self.history_len,
            title: String::new(),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(t: &str) -> LineSnapshot {
        let mut l = LineSnapshot::blank();
        l.push_str(t, StyleId::DEFAULT);
        l
    }

    fn update(seq: u64, full: bool, rows: &[(u16, &str)], history_len: u64) -> ScreenUpdate {
        ScreenUpdate {
            session: SessionId::nil(),
            seq,
            dims: Dims { cols: 20, rows: 4 },
            full,
            lines: rows.iter().map(|(r, t)| (*r, line(t))).collect(),
            cursor: CursorState {
                row: 3,
                col: 2,
                visible: true,
                ..Default::default()
            },
            modes: TermModes::SHOW_CURSOR,
            display_offset: 0,
            history_len,
            styles: vec![],
        }
    }

    fn texts(v: &mut SessionView) -> Vec<String> {
        v.screen().lines.iter().map(LineSnapshot::text).collect()
    }

    #[test]
    fn full_then_deltas_and_out_of_order_updates_are_dropped() {
        let mut v = SessionView::new(SessionId::nil());
        assert!(v.apply_screen(
            &update(5, true, &[(0, "a"), (1, "b"), (2, "c"), (3, "$")], 0),
            true
        ));
        assert!(v.apply_screen(&update(6, false, &[(1, "B")], 0), false));
        assert!(!v.apply_screen(&update(6, false, &[(2, "stale")], 0), false));
        assert!(!v.apply_screen(&update(4, true, &[(0, "old")], 0), false));
        assert_eq!(texts(&mut v), ["a", "B", "c", "$"]);
        // A new Attach answer is a baseline even with a lower seq (e.g. a
        // fresh actor after a crash).
        assert!(v.apply_screen(&update(1, true, &[(0, "x")], 0), true));
        assert_eq!(v.seq(), 1);
        assert_eq!(texts(&mut v), ["x", "", "", ""]);
    }

    #[test]
    fn scrolling_composes_cache_and_screen_and_moves_the_cursor() {
        let mut v = SessionView::new(SessionId::nil());
        v.apply_screen(
            &update(1, true, &[(0, "s0"), (1, "s1"), (2, "s2"), (3, "s3")], 10),
            true,
        );
        assert!(v.wanted_fetches(Instant::now()).is_empty(), "at the bottom");
        v.scroll_by(2);
        assert_eq!(v.top_line(), 8);
        let f = v.wanted_fetches(Instant::now());
        assert_eq!(
            f,
            vec![Fetch {
                start: 0,
                count: 10,
                refresh: false
            }]
        );
        v.note_fetch(7, f[0]);
        assert!(v.wanted_fetches(Instant::now()).is_empty(), "in flight");
        let hist: Vec<LineSnapshot> = (0..10).map(|i| line(&format!("h{i}"))).collect();
        assert!(v.apply_lines(7, 0, hist, &[]));
        assert_eq!(texts(&mut v), ["h8", "h9", "s0", "s1"]);
        let c = v.screen().cursor;
        assert_eq!(
            (c.row, c.visible),
            (5, false),
            "cursor scrolled out of view"
        );
        v.scroll_by(-1);
        assert_eq!(v.screen().cursor.row, 4);
        v.scroll_by(100);
        assert_eq!(v.display_offset(), 10);
        assert_eq!(texts(&mut v), ["h0", "h1", "h2", "h3"]);
    }

    #[test]
    fn prefetch_pages_and_stays_within_limits() {
        let mut v = SessionView::new(SessionId::nil());
        v.apply_screen(&update(1, true, &[], 20_000), true);
        v.scroll_by(1);
        let f = v.wanted_fetches(Instant::now());
        // One page ending at the visible history row.
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].start + u64::from(f[0].count), 20_000);
        assert_eq!(f[0].count, FETCH_PAGE);
        v.note_fetch(1, f[0]);
        let lines: Vec<LineSnapshot> = (0..FETCH_PAGE).map(|i| line(&i.to_string())).collect();
        v.apply_lines(1, f[0].start, lines, &[]);
        // Scroll to just inside the prefetch margin of the cached block.
        let lowest = f[0].start;
        v.set_display_offset(20_000 - (lowest + PREFETCH_MARGIN - 1));
        let f2 = v.wanted_fetches(Instant::now());
        assert_eq!(f2.len(), 1);
        assert_eq!(f2[0].start + u64::from(f2[0].count), lowest);
        assert!(f2[0].count <= 5000);
    }

    #[test]
    fn new_output_while_scrolled_up_keeps_the_view_anchored() {
        let mut v = SessionView::new(SessionId::nil());
        v.apply_screen(&update(1, true, &[], 100), true);
        v.scroll_by(10);
        let top = v.top_line();
        v.apply_screen(&update(2, true, &[], 103), false);
        assert_eq!(v.top_line(), top);
        assert_eq!(v.display_offset(), 13);
        // At the bottom the view follows.
        v.scroll_to_bottom();
        v.apply_screen(&update(3, true, &[], 110), false);
        assert_eq!(v.display_offset(), 0);
        assert_eq!(v.top_line(), 110);
    }

    #[test]
    fn shrinking_history_or_resize_clears_the_cache() {
        let mut v = SessionView::new(SessionId::nil());
        v.apply_screen(&update(1, true, &[], 50), true);
        v.scroll_by(5);
        for f in v.wanted_fetches(Instant::now()) {
            v.note_fetch(1, f);
        }
        v.apply_lines(1, 0, (0..50).map(|i| line(&i.to_string())).collect(), &[]);
        assert_eq!(v.cached_lines(), 50);
        v.apply_screen(&update(2, true, &[], 20), false);
        assert_eq!((v.cached_lines(), v.display_offset()), (0, 0));
        // Late answer to a request from before the reset is ignored.
        assert!(!v.apply_lines(1, 0, vec![line("late")], &[]));
        let mut resized = update(3, true, &[], 20);
        resized.dims = Dims { cols: 30, rows: 5 };
        v.scroll_by(3);
        v.apply_screen(&resized, false);
        assert_eq!(v.display_offset(), 0);
        assert_eq!(v.screen().lines.len(), 5);
    }

    #[test]
    fn full_redraw_at_constant_history_refreshes_visible_rows() {
        let mut v = SessionView::new(SessionId::nil());
        v.apply_screen(&update(1, true, &[], 20_000), true);
        v.scroll_by(2);
        let now = Instant::now();
        for f in v.wanted_fetches(now) {
            v.note_fetch(1, f);
        }
        // Scrollback at its limit: a scroll keeps history_len constant.
        v.apply_screen(&update(2, true, &[(0, "x")], 20_000), false);
        let f = v.wanted_fetches(now);
        assert!(f
            .iter()
            .any(|f| f.refresh && f.start == v.top_line() && f.count == 2));
    }

    #[test]
    fn tail_skips_trailing_blank_rows() {
        let mut v = SessionView::new(SessionId::nil());
        v.apply_screen(&update(1, true, &[(0, "one"), (1, "two")], 0), true);
        let t: Vec<String> = v.tail(3).iter().map(LineSnapshot::text).collect();
        assert_eq!(t, ["one", "two"]);
    }
}
