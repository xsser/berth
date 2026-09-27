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
//! trimmed (`clear`, ED 3, alternate screen, Revive) — both clear the cache.
//! At the scrollback limit the daemon drops its oldest line for every new
//! one while `history_len` stays put, which moves every index; the protocol
//! cannot tell that apart from an in-place redraw. So a full redraw with an
//! unchanged `history_len` starts a new cache generation: cached lines and
//! answers asked for in older generations never count as current. The rows
//! on screen at that moment keep their content as placeholders until
//! re-fetched; every other cached line is dropped and fetched again when
//! scrolled to. While such shifts keep coming only the visible rows are
//! re-fetched, at most every `REFRESH_INTERVAL`; pages resume once the
//! indices stay put that long. Growth of `history_len` keeps indices
//! stable, so it keeps the cache. Known gap: during a flood at the limit a
//! scrolled-up view cannot stay anchored on its content (the eviction count
//! is unknown); that needs absolute line numbers from the daemon.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use berth_core::{
    CursorShape, CursorState, Dims, LineSnapshot, ScreenSnapshot, ScreenUpdate, SessionId, Style,
    StyleId, StyleTable, TermModes,
};

use crate::selection::{Selection, SelectionSpans, Span};

/// Lines per prefetch request (the daemon caps one reply at 5000).
pub const FETCH_PAGE: u32 = 2000;
/// Prefetch once the top of the view is this close to the cache boundary.
pub const PREFETCH_MARGIN: u64 = 200;
/// While history indices keep shifting (scrollback at its limit) the
/// visible rows are re-fetched at most this often, and pages wait until the
/// indices have stayed put this long.
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const FETCH_RETRY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Inflight {
    id: u32,
    start: u64,
    count: u32,
    /// Re-fetch of the visible rows while the history shifts.
    refresh: bool,
    /// Cache generation when asked; answers of older ones are dropped.
    gen: u64,
}

/// A cached history line and the cache generation it was fetched in.
struct Cached {
    gen: u64,
    line: LineSnapshot,
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
    cache: BTreeMap<u64, Cached>,
    /// Bumped whenever the history indices may have shifted.
    gen: u64,
    /// `gen` when `wanted_fetches` last looked, and until when it holds
    /// pages back after seeing it change.
    seen_gen: u64,
    settle_at: Option<Instant>,
    inflight: Vec<Inflight>,
    display_offset: u64,
    last_refresh: Option<Instant>,
    retry_after: Option<Instant>,
    composed: ScreenSnapshot,
    dirty: bool,
    cursor_override: Option<CursorShape>,
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
            gen: 0,
            seen_gen: 0,
            settle_at: None,
            inflight: Vec::new(),
            display_offset: 0,
            last_refresh: None,
            retry_after: None,
            composed: ScreenSnapshot::default(),
            dirty: true,
            cursor_override: None,
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

    #[cfg(test)]
    pub fn display_offset(&self) -> u64 {
        self.display_offset
    }

    #[cfg(test)]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Virtual index of the first visible row.
    pub fn top_line(&self) -> u64 {
        self.history_len - self.display_offset
    }

    #[cfg(test)]
    pub fn cached_lines(&self) -> usize {
        self.cache.len()
    }

    /// The program asked for a blinking cursor.
    pub fn cursor_blinking(&self) -> bool {
        self.cursor.blinking
    }

    /// Every visible row is known and current (no history fetch
    /// outstanding for it).
    pub fn visible_complete(&self) -> bool {
        let top = self.top_line();
        self.has_screen && (top..top + u64::from(self.dims.rows)).all(|v| self.current(v))
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
            } else if new == old && u.full {
                // Maybe the scrollback limit: the oldest line was dropped
                // for each new one and every index moved (module docs).
                self.invalidate_history();
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
    /// made before the cache was cleared, or before the history last
    /// shifted, are dropped: their lines may belong to other indices now
    /// (each answer carries the styles it uses, so nothing else is lost).
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
        if self.inflight.remove(pos).gen != self.gen {
            return false;
        }
        self.styles.apply(styles);
        for (i, line) in lines.into_iter().enumerate() {
            let v = start + i as u64;
            if v >= self.history_len {
                break;
            }
            self.cache.insert(
                v,
                Cached {
                    gen: self.gen,
                    line,
                },
            );
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
            gen: self.gen,
        });
    }

    fn clear_cache(&mut self) {
        self.cache.clear();
        self.inflight.clear();
    }

    /// Start a new cache generation: the history indices may have shifted.
    /// The rows on screen keep their content as placeholders (not current)
    /// until re-fetched; every other cached line is dropped, so scrolling
    /// to it shows nothing stale and fetches it again.
    fn invalidate_history(&mut self) {
        self.gen += 1;
        let (top, end) = self.visible_history();
        if top >= end {
            self.cache.clear();
        } else {
            let mut shown = self.cache.split_off(&top);
            shown.split_off(&end);
            self.cache = shown;
        }
    }

    /// The history rows on screen, `[top, end)` (empty at the bottom).
    fn visible_history(&self) -> (u64, u64) {
        let top = self.top_line();
        (top, self.history_len.min(top + u64::from(self.dims.rows)))
    }

    /// Line `v` is known for the current generation (screen rows are).
    fn current(&self, v: u64) -> bool {
        if v >= self.history_len {
            self.line(v).is_some()
        } else {
            self.cache.get(&v).is_some_and(|c| c.gen == self.gen)
        }
    }

    /// Current, or asked for in the current generation.
    fn available(&self, v: u64) -> bool {
        self.current(v)
            || self
                .inflight
                .iter()
                .any(|f| f.gen == self.gen && v >= f.start && v < f.start + u64::from(f.count))
    }

    /// A visible-rows refresh of the current generation is in flight.
    fn refresh_pending(&self) -> bool {
        self.inflight.iter().any(|f| f.refresh && f.gen == self.gen)
    }

    /// History ranges to request now (visible rows plus the prefetch
    /// margin; each at most [`FETCH_PAGE`] lines). While the history
    /// indices keep shifting only the visible rows are asked for, at most
    /// every `REFRESH_INTERVAL`; pages resume once they have stayed put
    /// that long.
    pub fn wanted_fetches(&mut self, now: Instant) -> Vec<Fetch> {
        let mut out = Vec::new();
        if self.seen_gen != self.gen {
            self.seen_gen = self.gen;
            self.settle_at = Some(now + REFRESH_INTERVAL);
        }
        let settling = self.settle_at.is_some_and(|t| now < t);
        if !settling {
            self.settle_at = None;
        }
        if !self.has_screen || self.display_offset == 0 {
            return out;
        }
        if self.retry_after.is_some_and(|t| now < t) {
            return out;
        }
        self.retry_after = None;
        let (top, visible_end) = self.visible_history();
        if settling {
            if !self.refresh_pending()
                && self
                    .last_refresh
                    .is_none_or(|t| now >= t + REFRESH_INTERVAL)
                && (top..visible_end).any(|v| !self.current(v))
            {
                self.last_refresh = Some(now);
                out.push(Fetch {
                    start: top,
                    count: (visible_end - top) as u32,
                    refresh: true,
                });
            }
            return out;
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

    /// When `wanted_fetches` next has work waiting on a timer: the failure
    /// back-off, the next visible refresh while the history shifts, or the
    /// end of the settling period. Read it right after `wanted_fetches`,
    /// which retires timers that have passed.
    pub fn next_deadline(&self) -> Option<Instant> {
        if !self.has_screen || self.display_offset == 0 {
            return None;
        }
        if self.retry_after.is_some() {
            return self.retry_after;
        }
        let (top, end) = self.visible_history();
        let refresh = self
            .last_refresh
            .filter(|_| self.settle_at.is_some() && !self.refresh_pending())
            .filter(|_| (top..end).any(|v| !self.current(v)))
            .map(|t| t + REFRESH_INTERVAL);
        [self.settle_at, refresh].into_iter().flatten().min()
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

    /// Content of virtual line `v` if known (screen or cache; right after a
    /// history shift the rows that were on screen show their last content
    /// until re-fetched, see [`Self::visible_complete`]).
    pub fn line(&self, v: u64) -> Option<&LineSnapshot> {
        if v >= self.history_len {
            self.screen.get((v - self.history_len) as usize)
        } else {
            self.cache.get(&v).map(|c| &c.line)
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
        self.frame().0
    }

    /// The visible rows and the style table (what the renderer needs).
    pub fn frame(&mut self) -> (&ScreenSnapshot, &StyleTable) {
        self.refresh();
        self.composed()
    }

    /// Compose the visible rows if anything changed since the last time;
    /// then [`Self::composed`] is current. Split panes refresh every view
    /// first and borrow them all at once for the frame.
    pub fn refresh(&mut self) {
        if self.dirty {
            self.compose();
            self.dirty = false;
        }
    }

    /// The screen as of the last [`Self::refresh`] / [`Self::frame`].
    pub fn composed(&self) -> (&ScreenSnapshot, &StyleTable) {
        (&self.composed, &self.styles)
    }

    /// Draw the cursor with this shape whatever the program asked for
    /// (`--cursor-style`).
    pub fn set_cursor_override(&mut self, shape: Option<CursorShape>) {
        if self.cursor_override != shape {
            self.cursor_override = shape;
            self.dirty = true;
        }
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
        if let Some(shape) = self.cursor_override {
            cursor.shape = shape;
        }
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

    fn text(v: &SessionView, n: u64) -> Option<String> {
        v.line(n).map(LineSnapshot::text)
    }

    /// `history` lines of scrollback, scrolled `up`, with every history line
    /// up to the bottom of the view cached as "h{index}".
    fn scrolled_with_cache(history: u64, up: i64) -> SessionView {
        let mut v = SessionView::new(SessionId::nil());
        v.apply_screen(&update(1, true, &[(0, "s0")], history), true);
        v.scroll_by(up);
        for (id, f) in (1..).zip(v.wanted_fetches(Instant::now())) {
            v.note_fetch(id, f);
            let lines = (f.start..f.start + u64::from(f.count))
                .map(|n| line(&format!("h{n}")))
                .collect();
            assert!(v.apply_lines(id, f.start, lines, &[]));
        }
        v
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
    fn a_shift_at_the_scrollback_limit_invalidates_cached_lines_off_screen() {
        let mut v = scrolled_with_cache(100, 10);
        assert_eq!(v.top_line(), 90);
        assert_eq!(text(&v, 50).as_deref(), Some("h50"));
        let t = Instant::now();
        // At the limit a new line evicts the oldest: every index moves while
        // history_len stays put, and the scroll arrives as a full redraw.
        assert!(v.apply_screen(&update(2, true, &[(0, "s1")], 100), false));
        // Lines off screen read as missing, not as their old text...
        assert_eq!(text(&v, 50), None);
        assert_eq!(text(&v, 0), None);
        assert_eq!(text(&v, 89), None);
        // ...the rows on screen keep their content until re-fetched, which
        // is asked for right away (only them while the history shifts).
        assert_eq!(texts(&mut v), ["h90", "h91", "h92", "h93"]);
        assert!(!v.visible_complete());
        assert_eq!(
            v.wanted_fetches(t),
            vec![Fetch {
                start: 90,
                count: 4,
                refresh: true
            }]
        );
        // Scrolling to lines that were cached fetches them again once the
        // history stays put.
        v.set_display_offset(50);
        assert_eq!(text(&v, 50), None);
        assert!(v.wanted_fetches(t + Duration::from_millis(10)).is_empty());
        let f = v.wanted_fetches(t + REFRESH_INTERVAL);
        assert_eq!(
            f,
            vec![Fetch {
                start: 0,
                count: 54,
                refresh: false
            }]
        );
    }

    #[test]
    fn growing_history_keeps_the_cache() {
        let mut v = scrolled_with_cache(100, 10);
        let cached = v.cached_lines();
        // Below the limit new lines are appended: indices are stable.
        assert!(v.apply_screen(&update(2, true, &[(0, "s1")], 103), false));
        assert_eq!(v.cached_lines(), cached);
        assert_eq!(text(&v, 50).as_deref(), Some("h50"));
        assert_eq!(v.top_line(), 90, "the view stays on the same lines");
        assert!(v.visible_complete());
        assert!(v.wanted_fetches(Instant::now()).is_empty());
        assert_eq!(v.next_deadline(), None);
    }

    #[test]
    fn answers_asked_before_a_shift_do_not_enter_the_new_generation() {
        let mut v = SessionView::new(SessionId::nil());
        v.apply_screen(&update(1, true, &[], 100), true);
        v.scroll_by(10);
        let t = Instant::now();
        let f = v.wanted_fetches(t);
        assert_eq!(
            f,
            vec![Fetch {
                start: 0,
                count: 94,
                refresh: false
            }]
        );
        v.note_fetch(1, f[0]);
        // The history shifts while the request is in flight.
        v.apply_screen(&update(2, true, &[], 100), false);
        let old = (0..94).map(|n| line(&format!("old{n}"))).collect();
        assert!(!v.apply_lines(1, 0, old, &[]), "asked before the shift");
        assert_eq!(v.cached_lines(), 0);
        assert_eq!((text(&v, 50), text(&v, 92)), (None, None));
        // A refresh asked after the shift is current.
        let r = v.wanted_fetches(t);
        assert_eq!(
            r,
            vec![Fetch {
                start: 90,
                count: 4,
                refresh: true
            }]
        );
        v.note_fetch(2, r[0]);
        let new = (90..94).map(|n| line(&format!("new{n}"))).collect();
        assert!(v.apply_lines(2, 90, new, &[]));
        assert_eq!(texts(&mut v), ["new90", "new91", "new92", "new93"]);
        assert!(v.visible_complete());
        // The rest waits until the history has stayed put, then is fetched.
        assert!(v.wanted_fetches(t + Duration::from_millis(10)).is_empty());
        assert_eq!(v.next_deadline(), Some(t + REFRESH_INTERVAL));
        assert_eq!(
            v.wanted_fetches(t + REFRESH_INTERVAL),
            vec![Fetch {
                start: 0,
                count: 90,
                refresh: false
            }]
        );
        assert_eq!(v.next_deadline(), None);
    }

    #[test]
    fn shifts_keep_refreshing_the_visible_rows_at_the_throttle() {
        let mut v = SessionView::new(SessionId::nil());
        v.apply_screen(&update(1, true, &[], 100), true);
        v.scroll_by(10);
        let t = Instant::now();
        let ms = Duration::from_millis;
        let mut seq = 1;
        let mut shift = |v: &mut SessionView| {
            seq += 1;
            v.apply_screen(&update(seq, true, &[], 100), false);
        };
        shift(&mut v);
        let r = v.wanted_fetches(t);
        assert!(matches!(r.as_slice(), [Fetch { refresh: true, .. }]));
        v.note_fetch(1, r[0]);
        // Shifts keep coming: the refresh in flight is outdated, but a new
        // one waits for the interval and no pages are asked for.
        for i in 1..25 {
            shift(&mut v);
            assert!(v.wanted_fetches(t + ms(i * 10)).is_empty(), "{i}");
        }
        shift(&mut v);
        let r = v.wanted_fetches(t + REFRESH_INTERVAL);
        assert_eq!(
            r,
            vec![Fetch {
                start: 90,
                count: 4,
                refresh: true
            }]
        );
        assert!(!v.apply_lines(1, 90, vec![line("late")], &[]));
        assert_eq!(texts(&mut v), ["", "", "", ""]);
    }

    #[test]
    fn at_the_bottom_a_shift_only_drops_the_cache() {
        let mut v = scrolled_with_cache(100, 10);
        v.scroll_to_bottom();
        assert!(v.apply_screen(&update(2, true, &[(0, "s1")], 100), false));
        assert_eq!(v.cached_lines(), 0);
        assert!(v.wanted_fetches(Instant::now()).is_empty());
        assert_eq!(v.next_deadline(), None);
        assert_eq!(texts(&mut v), ["s1", "", "", ""]);
    }

    #[test]
    fn tail_skips_trailing_blank_rows() {
        let mut v = SessionView::new(SessionId::nil());
        v.apply_screen(&update(1, true, &[(0, "one"), (1, "two")], 0), true);
        let t: Vec<String> = v.tail(3).iter().map(LineSnapshot::text).collect();
        assert_eq!(t, ["one", "two"]);
    }
}
