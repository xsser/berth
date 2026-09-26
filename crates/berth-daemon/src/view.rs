//! Subscriber bookkeeping and the helpers behind the per-session virtual
//! line space: `[restored prefix 0..R) ++ [live scrollback) ++ [screen rows)`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use berth_core::{LineSnapshot, SessionSnapshotFile, Style, StyleId, StyleTable};

use crate::outbox::Outbox;

pub type ConnId = u64;

#[derive(Debug)]
pub(crate) enum SubKind {
    /// Full-resolution screen deltas. `styles_sent` = how many entries of the
    /// session's style table this subscriber already has (ids are dense and
    /// only grow, so this is enough to send exact per-subscriber deltas even
    /// with several clients).
    Full {
        needs_full: bool,
        styles_sent: usize,
    },
    /// Throttled tail of the last `rows` content lines.
    Preview {
        rows: usize,
        interval: Duration,
        last_sent: Option<Instant>,
        pending: bool,
        last_lines: Option<Vec<LineSnapshot>>,
    },
}

#[derive(Debug)]
pub(crate) struct Subscriber {
    pub conn: ConnId,
    pub outbox: Arc<Outbox>,
    pub kind: SubKind,
    /// Request id answered by the first update sent to this subscriber.
    pub reply_to: Option<u32>,
}

impl Subscriber {
    pub fn full(conn: ConnId, outbox: Arc<Outbox>, reply_to: Option<u32>) -> Subscriber {
        Subscriber {
            conn,
            outbox,
            kind: SubKind::Full {
                needs_full: true,
                styles_sent: 0,
            },
            reply_to,
        }
    }

    pub fn preview(
        conn: ConnId,
        outbox: Arc<Outbox>,
        rows: u8,
        max_hz: u8,
        reply_to: Option<u32>,
    ) -> Subscriber {
        let hz = u64::from(max_hz.max(1));
        Subscriber {
            conn,
            outbox,
            kind: SubKind::Preview {
                rows: usize::from(rows.max(1)),
                interval: Duration::from_millis(1000 / hz),
                last_sent: None,
                pending: true,
                last_lines: None,
            },
            reply_to,
        }
    }

    pub fn is_full(&self) -> bool {
        matches!(self.kind, SubKind::Full { .. })
    }

    /// When this preview subscriber may send next (None: nothing pending).
    pub fn preview_due(&self) -> Option<Instant> {
        match &self.kind {
            SubKind::Preview {
                pending: true,
                last_sent,
                interval,
                ..
            } => Some(last_sent.map_or_else(Instant::now, |t| t + *interval)),
            _ => None,
        }
    }
}

/// Drop trailing blank lines.
pub(crate) fn trim_trailing_blank(lines: &mut Vec<LineSnapshot>) {
    while lines.last().is_some_and(LineSnapshot::is_blank) {
        lines.pop();
    }
}

/// Keep only the newest `max` lines.
pub(crate) fn cap_front(lines: &mut Vec<LineSnapshot>, max: usize) {
    if lines.len() > max {
        lines.drain(..lines.len() - max);
    }
}

/// Restored prefix for a session loaded from disk: `history ++ screen`
/// (screen's trailing blank lines trimmed), capped to the newest `max`.
pub(crate) fn prefix_from_snapshot(
    snap: SessionSnapshotFile,
    max: usize,
) -> (Vec<LineSnapshot>, StyleTable) {
    let mut prefix = snap.history;
    if let Some(screen) = snap.screen {
        let mut lines = screen.lines;
        trim_trailing_blank(&mut lines);
        prefix.extend(lines);
    }
    cap_front(&mut prefix, max);
    (prefix, snap.styles)
}

/// Non-default styles used by `lines`, resolved in `table` (so `Lines` and
/// `Preview` messages are self-contained for any client).
pub(crate) fn referenced_styles(
    lines: &[LineSnapshot],
    table: &StyleTable,
) -> Vec<(StyleId, Style)> {
    let ids: BTreeSet<StyleId> = lines
        .iter()
        .flat_map(|l| l.runs.iter().map(|r| r.style))
        .filter(|id| *id != StyleId::DEFAULT)
        .collect();
    ids.into_iter().map(|id| (id, table.get(id))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_core::{Color, ScreenSnapshot, SessionMeta, StyleInterner, SNAPSHOT_FORMAT_VERSION};

    fn line(t: &str, s: StyleId) -> LineSnapshot {
        let mut l = LineSnapshot::blank();
        l.push_str(t, s);
        l
    }

    #[test]
    fn prefix_is_history_then_trimmed_screen_capped() {
        let screen = ScreenSnapshot {
            rows: 4,
            lines: vec![
                line("$ echo hi", StyleId(0)),
                line("hi", StyleId(0)),
                LineSnapshot::blank(),
                line("  ", StyleId(0)),
            ],
            ..Default::default()
        };
        let snap = SessionSnapshotFile {
            format_version: SNAPSHOT_FORMAT_VERSION,
            saved_at_ms: 0,
            session: SessionMeta::default(),
            styles: StyleTable::new(),
            history: vec![line("old1", StyleId(0)), line("old2", StyleId(0))],
            screen: Some(screen),
        };
        let (p, _) = prefix_from_snapshot(snap.clone(), 100);
        let texts: Vec<String> = p.iter().map(LineSnapshot::text).collect();
        assert_eq!(texts, ["old1", "old2", "$ echo hi", "hi"]);
        let (p, _) = prefix_from_snapshot(snap, 3);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].text(), "old2");
    }

    #[test]
    fn referenced_styles_are_unique_and_resolved() {
        let mut i = StyleInterner::new();
        let red = Style {
            fg: Color::Indexed(1),
            ..Default::default()
        };
        let rid = i.intern(red);
        let mut l = line("a", StyleId::DEFAULT);
        l.push('b', rid);
        let styles = referenced_styles(&[l.clone(), l], i.table());
        assert_eq!(styles, vec![(rid, red)]);
    }

    #[test]
    fn preview_interval_from_hz() {
        let s = Subscriber::preview(1, Outbox::new(), 3, 4, None);
        match s.kind {
            SubKind::Preview { rows, interval, .. } => {
                assert_eq!((rows, interval), (3, Duration::from_millis(250)))
            }
            _ => panic!(),
        }
        let s = Subscriber::preview(1, Outbox::new(), 0, 0, None);
        match s.kind {
            SubKind::Preview { rows, interval, .. } => {
                assert_eq!((rows, interval), (1, Duration::from_secs(1)))
            }
            _ => panic!(),
        }
    }
}
