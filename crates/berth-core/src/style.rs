//! Cell styling: colors, attribute flags, and the per-session style table.
//!
//! Styles are interned per session in the daemon (`StyleInterner`) and
//! mirrored by clients (`StyleTable`). Lines only carry `StyleId`s, which keeps
//! the wire and disk formats compact. `StyleId(0)` is always the default style.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Color {
    /// Terminal default foreground / background (theme decides).
    #[default]
    Default,
    /// 0..=255 palette index (0..=15 are the named ANSI colors).
    Indexed(u8),
    /// 24-bit color.
    Rgb(u8, u8, u8),
}

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct CellFlags: u16 {
        const BOLD             = 1 << 0;
        const ITALIC           = 1 << 1;
        const DIM              = 1 << 2;
        const UNDERLINE        = 1 << 3;
        const DOUBLE_UNDERLINE = 1 << 4;
        const UNDERCURL        = 1 << 5;
        const DOTTED_UNDERLINE = 1 << 6;
        const DASHED_UNDERLINE = 1 << 7;
        const STRIKEOUT        = 1 << 8;
        const INVERSE          = 1 << 9;
        const HIDDEN           = 1 << 10;
    }
}

impl CellFlags {
    pub const ANY_UNDERLINE: CellFlags = CellFlags::UNDERLINE
        .union(CellFlags::DOUBLE_UNDERLINE)
        .union(CellFlags::UNDERCURL)
        .union(CellFlags::DOTTED_UNDERLINE)
        .union(CellFlags::DASHED_UNDERLINE);
}

/// Complete visual style of a cell. Hyperlinks are intentionally not part of
/// v1 (reserved for a later protocol version).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    /// Underline color; `Color::Default` means "same as fg".
    pub underline: Color,
    pub flags: CellFlags,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct StyleId(pub u32);

impl StyleId {
    pub const DEFAULT: StyleId = StyleId(0);
}

/// Client-side (or on-disk) mirror of a session's interned styles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleTable {
    styles: Vec<Style>,
}

impl Default for StyleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl StyleTable {
    pub fn new() -> Self {
        Self {
            styles: vec![Style::default()],
        }
    }

    /// Unknown ids resolve to the default style rather than panicking, so a
    /// client that missed a delta degrades gracefully.
    pub fn get(&self, id: StyleId) -> Style {
        self.styles.get(id.0 as usize).copied().unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.styles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.styles.is_empty()
    }

    /// Insert or overwrite; grows the table with default styles if needed.
    pub fn insert(&mut self, id: StyleId, style: Style) {
        let idx = id.0 as usize;
        if idx >= self.styles.len() {
            self.styles.resize(idx + 1, Style::default());
        }
        self.styles[idx] = style;
    }

    pub fn apply(&mut self, delta: &[(StyleId, Style)]) {
        for (id, style) in delta {
            self.insert(*id, *style);
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = (StyleId, Style)> + '_ {
        self.styles
            .iter()
            .enumerate()
            .map(|(i, s)| (StyleId(i as u32), *s))
    }
}

/// Daemon-side interner: assigns ids and tracks which ids are new since the
/// last `take_pending`, so screen updates can carry only style deltas.
#[derive(Clone, Debug, Default)]
pub struct StyleInterner {
    table: StyleTable,
    index: HashMap<Style, StyleId>,
    pending: Vec<StyleId>,
}

impl StyleInterner {
    pub fn new() -> Self {
        let table = StyleTable::new();
        let mut index = HashMap::new();
        index.insert(Style::default(), StyleId::DEFAULT);
        Self {
            table,
            index,
            pending: Vec::new(),
        }
    }

    pub fn intern(&mut self, style: Style) -> StyleId {
        if let Some(id) = self.index.get(&style) {
            return *id;
        }
        let id = StyleId(self.table.len() as u32);
        self.table.insert(id, style);
        self.index.insert(style, id);
        self.pending.push(id);
        id
    }

    pub fn table(&self) -> &StyleTable {
        &self.table
    }

    /// Styles added since the previous call (for incremental sync).
    pub fn take_pending(&mut self) -> Vec<(StyleId, Style)> {
        let pending = std::mem::take(&mut self.pending);
        pending
            .into_iter()
            .map(|id| (id, self.table.get(id)))
            .collect()
    }

    /// Every style (for a full sync on attach).
    pub fn all(&self) -> Vec<(StyleId, Style)> {
        self.table.entries().collect()
    }

    /// Rebuild an interner from a persisted table (after daemon restart).
    pub fn from_table(table: StyleTable) -> Self {
        let index = table.entries().map(|(id, s)| (s, id)).collect();
        Self {
            table,
            index,
            pending: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interner_dedups_and_tracks_pending() {
        let mut i = StyleInterner::new();
        assert_eq!(i.intern(Style::default()), StyleId::DEFAULT);
        let red = Style {
            fg: Color::Indexed(1),
            ..Default::default()
        };
        let a = i.intern(red);
        let b = i.intern(red);
        assert_eq!(a, b);
        assert_eq!(a, StyleId(1));
        let pending = i.take_pending();
        assert_eq!(pending, vec![(StyleId(1), red)]);
        assert!(i.take_pending().is_empty());
        let rebuilt = StyleInterner::from_table(i.table().clone());
        assert_eq!(rebuilt.index.get(&red), Some(&StyleId(1)));
    }

    #[test]
    fn table_grows_on_sparse_insert() {
        let mut t = StyleTable::new();
        let s = Style {
            flags: CellFlags::BOLD,
            ..Default::default()
        };
        t.insert(StyleId(5), s);
        assert_eq!(t.len(), 6);
        assert_eq!(t.get(StyleId(5)), s);
        assert_eq!(t.get(StyleId(3)), Style::default());
        assert_eq!(t.get(StyleId(99)), Style::default());
    }

    #[test]
    fn flags_serialize_compact_in_postcard() {
        let f = CellFlags::BOLD | CellFlags::UNDERCURL;
        let bytes = postcard::to_stdvec(&f).unwrap();
        assert!(bytes.len() <= 2);
        let back: CellFlags = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(f, back);
    }
}
