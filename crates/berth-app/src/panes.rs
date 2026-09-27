//! Split panes (DESIGN §17.3): which sessions the terminal area shows, and
//! where. Pure data and geometry (no winit, no wgpu), so every operation is
//! unit-tested.
//!
//! A [`PaneTree`] is a binary tree: a leaf shows one session; a split
//! divides its area between two subtrees along an [`Axis`] at `ratio`
//! (kept within [`MIN_RATIO`]..=[`MAX_RATIO`]). A session is shown by at
//! most one leaf. A new pane goes right of / below the pane it splits and
//! gets half of its space. Removing a pane lets its sibling take the
//! parent's place: closing a pane and 「从分屏移除」 are the same tree
//! operation and differ only in what the controller does with the session.
//!
//! [`PaneTree::layout`] turns the tree into pixel rectangles for the panes
//! and for the dividers between them, and [`Geometry`] turns a pane's
//! rectangle into cells. The renderer and the controller (which sizes each
//! pane's PTY) use the same functions, so they agree to the pixel.

use std::cmp::Ordering;
use std::collections::HashSet;

use berth_core::{Dims, SessionId};
use serde::{Deserialize, Serialize};

pub const MIN_RATIO: f32 = 0.2;
pub const MAX_RATIO: f32 = 0.8;

/// How a split arranges its two children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    /// Side by side: `first` left, `second` right (⌘D).
    Horizontal,
    /// Stacked: `first` above, `second` below (⌘⇧D).
    Vertical,
}

/// Where a new pane goes relative to the pane it splits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDir {
    Right,
    Down,
}

impl SplitDir {
    pub fn axis(self) -> Axis {
        match self {
            SplitDir::Right => Axis::Horizontal,
            SplitDir::Down => Axis::Vertical,
        }
    }
}

/// Focus movement between panes (⌥⌘ arrows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Which child of a split.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Branch {
    First,
    Second,
}

/// A split node, as the branches taken from the root.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct SplitPath(pub Vec<Branch>);

/// An axis-aligned rectangle in physical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// A pane of a [`Layout`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneRect {
    pub session: SessionId,
    pub rect: Rect,
}

/// The gap between the two children of a split.
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    pub rect: Rect,
    pub path: SplitPath,
    pub axis: Axis,
    /// The whole area of the split (a drag maps a position to a ratio in it).
    pub span: Rect,
}

impl Divider {
    /// Where the divider can be grabbed: its line widened across the split
    /// to `reach` (never narrower than the line). The line stays thin and
    /// the grab area overlaps the panes' edges, as in macOS split views.
    pub fn grab_rect(&self, reach: f32) -> Rect {
        let r = self.rect;
        match self.axis {
            Axis::Horizontal => {
                let grow = ((reach - r.w) / 2.0).max(0.0);
                Rect::new(r.x - grow, r.y, r.w + 2.0 * grow, r.h)
            }
            Axis::Vertical => {
                let grow = ((reach - r.h) / 2.0).max(0.0);
                Rect::new(r.x, r.y - grow, r.w, r.h + 2.0 * grow)
            }
        }
    }

    /// The center of the line along the split's axis: where
    /// [`Self::ratio_at`] puts it back.
    pub fn center(&self) -> f32 {
        match self.axis {
            Axis::Horizontal => self.rect.x + self.rect.w / 2.0,
            Axis::Vertical => self.rect.y + self.rect.h / 2.0,
        }
    }

    /// The ratio that centers this divider on `pos`, the pointer coordinate
    /// along the split's axis (x for side by side, y for stacked).
    pub fn ratio_at(&self, pos: f32, divider: f32) -> f32 {
        let (start, len) = match self.axis {
            Axis::Horizontal => (self.span.x, self.span.w),
            Axis::Vertical => (self.span.y, self.span.h),
        };
        let d = divider.clamp(0.0, len.max(0.0));
        let avail = len - d;
        if avail <= 0.0 {
            return 0.5;
        }
        clamp_ratio((pos - start - d / 2.0) / avail)
    }
}

/// Where everything goes, in reading order (first before second).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    pub panes: Vec<PaneRect>,
    pub dividers: Vec<Divider>,
}

impl Layout {
    pub fn rect_of(&self, sid: SessionId) -> Option<Rect> {
        self.panes.iter().find(|p| p.session == sid).map(|p| p.rect)
    }

    /// The pane under a point.
    pub fn pane_at(&self, x: f32, y: f32) -> Option<PaneRect> {
        self.panes.iter().copied().find(|p| p.rect.contains(x, y))
    }

    /// The divider that can be grabbed at a point: its line, else its
    /// grab area ([`Divider::grab_rect`], `reach` wide), which wins over the
    /// pane edges it overlaps.
    pub fn divider_at(&self, x: f32, y: f32, reach: f32) -> Option<&Divider> {
        let on = |r: Rect| r.contains(x, y);
        self.dividers
            .iter()
            .find(|d| on(d.rect))
            .or_else(|| self.dividers.iter().find(|d| on(d.grab_rect(reach))))
    }
}

/// Pixel geometry of the terminal area, shared by the renderer and the
/// controller.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// Right of the sidebar, the whole window height.
    pub area: Rect,
    pub cell_w: f32,
    pub cell_h: f32,
    /// Between a pane's edge and its first cell.
    pub pad: f32,
    /// Thickness of the gap between split panes.
    pub divider: f32,
}

impl Geometry {
    /// Top-left of cell (0, 0) of a pane.
    pub fn origin(&self, pane: Rect) -> [f32; 2] {
        [pane.x + self.pad, pane.y + self.pad]
    }

    /// The cells that fit in a pane.
    pub fn cells(&self, pane: Rect) -> Dims {
        let fit = |len: f32, cell: f32| -> u16 {
            if cell <= 0.0 {
                return 0;
            }
            ((len - 2.0 * self.pad) / cell)
                .floor()
                .clamp(0.0, f32::from(u16::MAX)) as u16
        };
        Dims {
            cols: fit(pane.w, self.cell_w),
            rows: fit(pane.h, self.cell_h),
        }
    }
}

/// Why a split was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneError {
    /// The session to split is not shown.
    NotShown,
    /// The new session is shown already (one pane per session).
    AlreadyShown,
}

/// What [`PaneTree::remove`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// The pane is gone; `focus` is the pane that took its place (the
    /// nearest leaf of its sibling).
    Removed {
        focus: SessionId,
    },
    NotShown,
    /// The only pane: a tree is never empty, the caller decides.
    LastPane,
}

pub fn clamp_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(MIN_RATIO, MAX_RATIO)
    } else {
        0.5
    }
}

/// `area` cut along `axis`: (first, divider, second). Integral when the
/// inputs are.
fn split_rect(area: Rect, axis: Axis, ratio: f32, divider: f32) -> (Rect, Rect, Rect) {
    let ratio = clamp_ratio(ratio);
    match axis {
        Axis::Horizontal => {
            let d = divider.clamp(0.0, area.w.max(0.0));
            let avail = (area.w - d).max(0.0);
            let a = (avail * ratio).round();
            (
                Rect::new(area.x, area.y, a, area.h),
                Rect::new(area.x + a, area.y, d, area.h),
                Rect::new(area.x + a + d, area.y, avail - a, area.h),
            )
        }
        Axis::Vertical => {
            let d = divider.clamp(0.0, area.h.max(0.0));
            let avail = (area.h - d).max(0.0);
            let a = (avail * ratio).round();
            (
                Rect::new(area.x, area.y, area.w, a),
                Rect::new(area.x, area.y + a, area.w, d),
                Rect::new(area.x, area.y + a + d, area.w, avail - a),
            )
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneTree {
    Leaf(SessionId),
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<PaneTree>,
        second: Box<PaneTree>,
    },
}

impl PaneTree {
    pub fn contains(&self, sid: SessionId) -> bool {
        match self {
            PaneTree::Leaf(s) => *s == sid,
            PaneTree::Split { first, second, .. } => first.contains(sid) || second.contains(sid),
        }
    }

    /// The shown sessions in reading order.
    pub fn leaves(&self) -> Vec<SessionId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<SessionId>) {
        match self {
            PaneTree::Leaf(s) => out.push(*s),
            PaneTree::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }

    pub fn pane_count(&self) -> usize {
        match self {
            PaneTree::Leaf(_) => 1,
            PaneTree::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }

    pub fn first_leaf(&self) -> SessionId {
        match self {
            PaneTree::Leaf(s) => *s,
            PaneTree::Split { first, .. } => first.first_leaf(),
        }
    }

    pub fn last_leaf(&self) -> SessionId {
        match self {
            PaneTree::Leaf(s) => *s,
            PaneTree::Split { second, .. } => second.last_leaf(),
        }
    }

    fn leaf_mut(&mut self, sid: SessionId) -> Option<&mut PaneTree> {
        if matches!(self, PaneTree::Leaf(s) if *s == sid) {
            return Some(self);
        }
        match self {
            PaneTree::Leaf(_) => None,
            PaneTree::Split { first, second, .. } => match first.leaf_mut(sid) {
                Some(leaf) => Some(leaf),
                None => second.leaf_mut(sid),
            },
        }
    }

    /// Split `target`'s pane: `new` goes right of / below it, each gets
    /// half of the space.
    pub fn split(
        &mut self,
        target: SessionId,
        new: SessionId,
        dir: SplitDir,
    ) -> Result<(), PaneError> {
        if self.contains(new) {
            return Err(PaneError::AlreadyShown);
        }
        let leaf = self.leaf_mut(target).ok_or(PaneError::NotShown)?;
        *leaf = PaneTree::Split {
            axis: dir.axis(),
            ratio: 0.5,
            first: Box::new(PaneTree::Leaf(target)),
            second: Box::new(PaneTree::Leaf(new)),
        };
        Ok(())
    }

    /// Remove `sid`'s pane; its sibling takes the parent's place.
    pub fn remove(&mut self, sid: SessionId) -> Removal {
        let (first, second) = match self {
            PaneTree::Leaf(s) if *s == sid => return Removal::LastPane,
            PaneTree::Leaf(_) => return Removal::NotShown,
            PaneTree::Split { first, second, .. } => (first, second),
        };
        let taken = if matches!(**first, PaneTree::Leaf(s) if s == sid) {
            Some((std::mem::replace(&mut **second, PaneTree::Leaf(sid)), true))
        } else if matches!(**second, PaneTree::Leaf(s) if s == sid) {
            Some((std::mem::replace(&mut **first, PaneTree::Leaf(sid)), false))
        } else {
            None
        };
        match taken {
            Some((sibling, removed_first)) => {
                // Focus the sibling's leaf nearest to the removed pane.
                let focus = if removed_first {
                    sibling.first_leaf()
                } else {
                    sibling.last_leaf()
                };
                *self = sibling;
                Removal::Removed { focus }
            }
            None => match first.remove(sid) {
                Removal::NotShown => second.remove(sid),
                done => done,
            },
        }
    }

    /// Show `new` in `old`'s pane. Refused when `new` has a pane already.
    pub fn replace(&mut self, old: SessionId, new: SessionId) -> bool {
        if old != new && self.contains(new) {
            return false;
        }
        match self.leaf_mut(old) {
            Some(leaf) => {
                *leaf = PaneTree::Leaf(new);
                true
            }
            None => false,
        }
    }

    /// Set a split's ratio (clamped). Returns whether it changed.
    pub fn set_ratio(&mut self, path: &SplitPath, ratio: f32) -> bool {
        let mut node = self;
        for branch in &path.0 {
            node = match node {
                PaneTree::Leaf(_) => return false,
                PaneTree::Split { first, second, .. } => match branch {
                    Branch::First => &mut **first,
                    Branch::Second => &mut **second,
                },
            };
        }
        match node {
            PaneTree::Leaf(_) => false,
            PaneTree::Split { ratio: r, .. } => {
                let new = clamp_ratio(ratio);
                let changed = *r != new;
                *r = new;
                changed
            }
        }
    }

    /// Keep the panes whose session passes `keep` (and only the first pane
    /// of a session shown twice), collapsing splits left with one child;
    /// ratios are clamped. `None` when no pane is left. Used on a layout
    /// read from disk and when sessions go away.
    pub fn prune(self, keep: impl Fn(SessionId) -> bool) -> Option<PaneTree> {
        let mut seen = HashSet::new();
        self.prune_with(&keep, &mut seen)
    }

    fn prune_with(
        self,
        keep: &dyn Fn(SessionId) -> bool,
        seen: &mut HashSet<SessionId>,
    ) -> Option<PaneTree> {
        match self {
            PaneTree::Leaf(sid) => (keep(sid) && seen.insert(sid)).then_some(PaneTree::Leaf(sid)),
            PaneTree::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let first = first.prune_with(keep, seen);
                let second = second.prune_with(keep, seen);
                match (first, second) {
                    (Some(first), Some(second)) => Some(PaneTree::Split {
                        axis,
                        ratio: clamp_ratio(ratio),
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            }
        }
    }

    /// Panes and dividers laid out in `area`.
    pub fn layout(&self, area: Rect, divider: f32) -> Layout {
        let mut out = Layout::default();
        self.layout_into(area, divider, &mut Vec::new(), &mut out);
        out
    }

    fn layout_into(&self, area: Rect, divider: f32, path: &mut Vec<Branch>, out: &mut Layout) {
        match self {
            PaneTree::Leaf(sid) => out.panes.push(PaneRect {
                session: *sid,
                rect: area,
            }),
            PaneTree::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, d, b) = split_rect(area, *axis, *ratio, divider);
                path.push(Branch::First);
                first.layout_into(a, divider, path, out);
                path.pop();
                out.dividers.push(Divider {
                    rect: d,
                    path: SplitPath(path.clone()),
                    axis: *axis,
                    span: area,
                });
                path.push(Branch::Second);
                second.layout_into(b, divider, path, out);
                path.pop();
            }
        }
    }

    /// The pane to focus when moving from `from` towards `dir`: among the
    /// panes entirely on that side, those overlapping `from` across the
    /// movement come first, then the nearest, then the one whose middle is
    /// closest to `from`'s.
    pub fn neighbor(
        &self,
        area: Rect,
        divider: f32,
        from: SessionId,
        dir: Direction,
    ) -> Option<SessionId> {
        const EPS: f32 = 0.5;
        let layout = self.layout(area, divider);
        let f = layout.rect_of(from)?;
        let mut best: Option<((bool, f32, f32), SessionId)> = None;
        for p in &layout.panes {
            if p.session == from {
                continue;
            }
            let r = p.rect;
            // (gap along the movement, the candidate's and `from`'s extent across it)
            let (gap, lo, hi, flo, fhi) = match dir {
                Direction::Left => (f.x - r.right(), r.y, r.bottom(), f.y, f.bottom()),
                Direction::Right => (r.x - f.right(), r.y, r.bottom(), f.y, f.bottom()),
                Direction::Up => (f.y - r.bottom(), r.x, r.right(), f.x, f.right()),
                Direction::Down => (r.y - f.bottom(), r.x, r.right(), f.x, f.right()),
            };
            if gap < -EPS {
                continue;
            }
            let overlaps = hi.min(fhi) - lo.max(flo) > 0.0;
            let off_center = ((lo + hi) - (flo + fhi)).abs() / 2.0;
            let score = (!overlaps, gap.max(0.0), off_center);
            let better = match &best {
                None => true,
                Some((b, _)) => cmp_score(score, *b) == Ordering::Less,
            };
            if better {
                best = Some((score, p.session));
            }
        }
        best.map(|(_, sid)| sid)
    }
}

fn cmp_score(a: (bool, f32, f32), b: (bool, f32, f32)) -> Ordering {
    a.0.cmp(&b.0)
        .then(a.1.total_cmp(&b.1))
        .then(a.2.total_cmp(&b.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<SessionId> {
        (0..n).map(|_| SessionId::new()).collect()
    }

    const AREA: Rect = Rect::new(100.0, 0.0, 1006.0, 606.0);
    const DIV: f32 = 6.0;

    /// [a | [b / c]]
    fn three() -> (PaneTree, Vec<SessionId>) {
        let s = ids(3);
        let mut t = PaneTree::Leaf(s[0]);
        t.split(s[0], s[1], SplitDir::Right).unwrap();
        t.split(s[1], s[2], SplitDir::Down).unwrap();
        (t, s)
    }

    #[test]
    fn split_puts_the_new_pane_right_or_below_and_refuses_duplicates() {
        let (t, s) = three();
        assert_eq!(t.leaves(), s);
        assert_eq!(t.pane_count(), 3);
        let PaneTree::Split {
            axis,
            ratio,
            first,
            second,
        } = &t
        else {
            panic!("{t:?}")
        };
        assert_eq!((*axis, *ratio), (Axis::Horizontal, 0.5));
        assert_eq!(**first, PaneTree::Leaf(s[0]));
        assert!(matches!(
            **second,
            PaneTree::Split {
                axis: Axis::Vertical,
                ..
            }
        ));
        let mut t2 = t.clone();
        assert_eq!(
            t2.split(s[0], s[2], SplitDir::Down),
            Err(PaneError::AlreadyShown)
        );
        assert_eq!(
            t2.split(SessionId::new(), SessionId::new(), SplitDir::Down),
            Err(PaneError::NotShown)
        );
        assert_eq!(t2, t, "refused splits change nothing");
    }

    #[test]
    fn layout_divides_the_area_and_places_dividers_between_panes() {
        let (t, s) = three();
        let l = t.layout(AREA, DIV);
        let r: Vec<Rect> = l.panes.iter().map(|p| p.rect).collect();
        assert_eq!(l.panes.iter().map(|p| p.session).collect::<Vec<_>>(), s);
        assert_eq!(r[0], Rect::new(100.0, 0.0, 500.0, 606.0));
        assert_eq!(r[1], Rect::new(606.0, 0.0, 500.0, 300.0));
        assert_eq!(r[2], Rect::new(606.0, 306.0, 500.0, 300.0));
        assert_eq!(l.dividers.len(), 2);
        assert_eq!(l.dividers[0].rect, Rect::new(600.0, 0.0, 6.0, 606.0));
        assert_eq!(l.dividers[0].path, SplitPath(vec![]));
        assert_eq!(l.dividers[1].rect, Rect::new(606.0, 300.0, 500.0, 6.0));
        assert_eq!(l.dividers[1].path, SplitPath(vec![Branch::Second]));
        // Nothing overlaps and everything is covered.
        let covered: f32 = r.iter().map(|r| r.w * r.h).sum::<f32>()
            + l.dividers.iter().map(|d| d.rect.w * d.rect.h).sum::<f32>();
        assert_eq!(covered, AREA.w * AREA.h);
        assert_eq!(l.pane_at(700.0, 400.0).map(|p| p.session), Some(s[2]));
        assert!(l.pane_at(602.0, 10.0).is_none(), "the divider is no pane");
        assert_eq!(
            l.divider_at(602.0, 10.0, 0.0).map(|d| d.axis),
            Some(Axis::Horizontal)
        );
        assert_eq!(l.dividers[0].center(), 603.0);
        assert_eq!(l.dividers[1].center(), 303.0);
        // A single pane is the whole area.
        let one = PaneTree::Leaf(s[0]).layout(AREA, DIV);
        assert_eq!(one.panes[0].rect, AREA);
        assert!(one.dividers.is_empty());
    }

    #[test]
    fn a_thin_divider_is_grabbed_beyond_its_line() {
        let (t, s) = three();
        // A 2 px line, grabbed within 12 px (1 pt / 6 pt at 2x).
        let l = t.layout(AREA, 2.0);
        let v = &l.dividers[0];
        assert_eq!(v.rect, Rect::new(602.0, 0.0, 2.0, 606.0));
        assert_eq!(v.grab_rect(12.0), Rect::new(597.0, 0.0, 12.0, 606.0));
        let h = &l.dividers[1];
        assert_eq!(h.grab_rect(12.0), Rect::new(604.0, 297.0, 502.0, 12.0));
        assert_eq!(v.grab_rect(1.0), v.rect, "never narrower than the line");
        // Over a pane's edge: the divider is grabbed; the pane is still
        // the pane there (for a right-click, the wheel).
        assert_eq!(l.pane_at(599.0, 10.0).map(|p| p.session), Some(s[0]));
        let at = |x, y| l.divider_at(x, y, 12.0).map(|d| d.path.clone());
        assert_eq!(at(599.0, 10.0), Some(SplitPath(vec![])));
        assert_eq!(at(608.0, 10.0), Some(SplitPath(vec![])));
        assert_eq!(at(596.0, 10.0), None);
        assert_eq!(at(700.0, 298.0), Some(SplitPath(vec![Branch::Second])));
        // On one's line inside the other's grab area: the line wins.
        assert_eq!(at(606.0, 303.0), Some(SplitPath(vec![Branch::Second])));
        assert_eq!(at(700.0, 250.0), None);
    }

    #[test]
    fn ratios_follow_the_divider_drag_and_stay_clamped() {
        let (mut t, s) = three();
        let l = t.layout(AREA, DIV);
        let d = &l.dividers[0];
        // Drag the vertical divider to x = 400 (its center).
        let ratio = d.ratio_at(400.0, DIV);
        assert!(t.set_ratio(&d.path, ratio));
        let r = t.layout(AREA, DIV).rect_of(s[0]).unwrap();
        assert_eq!(r.right() + DIV / 2.0, 400.0);
        // Far left: clamped to MIN_RATIO.
        assert_eq!(d.ratio_at(0.0, DIV), MIN_RATIO);
        assert_eq!(d.ratio_at(5000.0, DIV), MAX_RATIO);
        assert!(!t.set_ratio(&SplitPath(vec![Branch::First]), 0.3), "a leaf");
        assert!(!t.set_ratio(&SplitPath(vec![Branch::First, Branch::First]), 0.3));
        t.set_ratio(&SplitPath(vec![Branch::Second]), 7.0);
        let PaneTree::Split { second, .. } = &t else {
            panic!()
        };
        assert!(matches!(**second, PaneTree::Split { ratio, .. } if ratio == MAX_RATIO));
        assert_eq!(clamp_ratio(f32::NAN), 0.5);
    }

    #[test]
    fn removing_a_pane_lets_its_sibling_take_the_place() {
        let (t, s) = three();
        // Remove b (first of the right column): c takes the column.
        let mut t1 = t.clone();
        assert_eq!(t1.remove(s[1]), Removal::Removed { focus: s[2] });
        assert_eq!(t1.leaves(), vec![s[0], s[2]]);
        let l = t1.layout(AREA, DIV);
        assert_eq!(l.rect_of(s[2]).unwrap().h, AREA.h);
        // Remove a: the right column becomes the root; focus its first leaf.
        let mut t2 = t.clone();
        assert_eq!(t2.remove(s[0]), Removal::Removed { focus: s[1] });
        assert_eq!(t2.leaves(), vec![s[1], s[2]]);
        assert!(matches!(
            t2,
            PaneTree::Split {
                axis: Axis::Vertical,
                ..
            }
        ));
        // Remove c (second): focus the nearest leaf of the sibling.
        let mut t3 = t.clone();
        assert_eq!(t3.remove(s[2]), Removal::Removed { focus: s[1] });
        // Not shown / the last pane.
        assert_eq!(t3.clone().remove(SessionId::new()), Removal::NotShown);
        let mut last = PaneTree::Leaf(s[0]);
        assert_eq!(last.remove(s[0]), Removal::LastPane);
        assert_eq!(last, PaneTree::Leaf(s[0]));
        // Down to one pane.
        assert!(matches!(t3.remove(s[1]), Removal::Removed { focus } if focus == s[0]));
        assert_eq!(t3, PaneTree::Leaf(s[0]));
    }

    #[test]
    fn replace_swaps_a_pane_session_but_never_duplicates_one() {
        let (mut t, s) = three();
        let x = SessionId::new();
        assert!(t.replace(s[1], x));
        assert_eq!(t.leaves(), vec![s[0], x, s[2]]);
        assert!(!t.replace(s[0], s[2]), "s[2] has a pane already");
        assert!(t.replace(s[0], s[0]), "same session: a no-op");
        assert!(!t.replace(SessionId::new(), SessionId::new()));
    }

    #[test]
    fn focus_moves_to_the_geometrically_nearest_pane() {
        // [a | [b / c]]: from a, right goes to b (top-most on overlap
        // tie? both overlap; b's middle is as far as c's: reading order).
        let (t, s) = three();
        let n = |from, dir| t.neighbor(AREA, DIV, from, dir);
        assert_eq!(n(s[0], Direction::Right), Some(s[1]));
        assert_eq!(n(s[0], Direction::Left), None);
        assert_eq!(n(s[0], Direction::Up), None);
        assert_eq!(n(s[1], Direction::Down), Some(s[2]));
        assert_eq!(n(s[2], Direction::Up), Some(s[1]));
        assert_eq!(n(s[2], Direction::Left), Some(s[0]));
        assert_eq!(n(s[1], Direction::Right), None);
        // 2×2: [[a / c] | [b / d]] — left/right keeps the row.
        let q = ids(4);
        let mut g = PaneTree::Leaf(q[0]);
        g.split(q[0], q[1], SplitDir::Right).unwrap();
        g.split(q[0], q[2], SplitDir::Down).unwrap();
        g.split(q[1], q[3], SplitDir::Down).unwrap();
        let n = |from, dir| g.neighbor(AREA, DIV, from, dir);
        assert_eq!(n(q[2], Direction::Right), Some(q[3]));
        assert_eq!(n(q[3], Direction::Left), Some(q[2]));
        assert_eq!(n(q[1], Direction::Left), Some(q[0]));
        assert_eq!(n(q[0], Direction::Down), Some(q[2]));
        assert_eq!(n(q[3], Direction::Up), Some(q[1]));
        // A pane beside two smaller ones prefers the one whose middle is
        // nearer: move the right column's split low so b is tall.
        let mut t2 = t.clone();
        t2.set_ratio(&SplitPath(vec![Branch::Second]), 0.8);
        let unknown = SessionId::new();
        assert_eq!(t2.neighbor(AREA, DIV, unknown, Direction::Left), None);
        assert_eq!(t2.neighbor(AREA, DIV, s[0], Direction::Right), Some(s[1]));
    }

    #[test]
    fn prune_drops_missing_and_duplicate_sessions_and_collapses() {
        let (t, s) = three();
        let keep_ac = t.clone().prune(|sid| sid != s[1]).unwrap();
        assert_eq!(keep_ac.leaves(), vec![s[0], s[2]]);
        assert!(matches!(
            keep_ac,
            PaneTree::Split {
                axis: Axis::Horizontal,
                ..
            }
        ));
        assert_eq!(
            t.clone().prune(|sid| sid == s[2]),
            Some(PaneTree::Leaf(s[2]))
        );
        assert_eq!(t.clone().prune(|_| false), None);
        // A hand-edited file: one session twice, a ratio out of range.
        let dup = PaneTree::Split {
            axis: Axis::Vertical,
            ratio: 3.0,
            first: Box::new(PaneTree::Leaf(s[0])),
            second: Box::new(PaneTree::Split {
                axis: Axis::Horizontal,
                ratio: 0.1,
                first: Box::new(PaneTree::Leaf(s[0])),
                second: Box::new(PaneTree::Leaf(s[1])),
            }),
        };
        let fixed = dup.prune(|_| true).unwrap();
        assert_eq!(fixed.leaves(), vec![s[0], s[1]]);
        assert!(matches!(fixed, PaneTree::Split { ratio, .. } if ratio == MAX_RATIO));
    }

    #[test]
    fn serde_roundtrip_is_readable_json() {
        let (mut t, s) = three();
        t.set_ratio(&SplitPath(vec![]), 0.3);
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("\"split\""), "{json}");
        assert!(json.contains("\"horizontal\""), "{json}");
        assert!(
            json.contains(&format!("{{\"leaf\":\"{}\"}}", s[0])),
            "{json}"
        );
        let back: PaneTree = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn geometry_fits_whole_cells_inside_the_padding() {
        let g = Geometry {
            area: AREA,
            cell_w: 10.0,
            cell_h: 20.0,
            pad: 4.0,
            divider: DIV,
        };
        let full = g.cells(AREA);
        assert_eq!((full.cols, full.rows), (99, 29));
        assert_eq!(g.origin(AREA), [104.0, 4.0]);
        let (t, s) = three();
        let l = t.layout(g.area, g.divider);
        let c = g.cells(l.rect_of(s[2]).unwrap());
        assert_eq!((c.cols, c.rows), (49, 14));
        assert_eq!(
            g.cells(Rect::new(0.0, 0.0, 3.0, 3.0)),
            Dims { cols: 0, rows: 0 }
        );
        let zero = Geometry { cell_w: 0.0, ..g };
        assert_eq!(zero.cells(AREA).cols, 0);
    }
}
