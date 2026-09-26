//! Per-connection outgoing queue.
//!
//! Bounded and lossy only where loss is harmless: a queued `Screen` or
//! `Preview` for a session is *merged* with a newer one instead of queueing
//! both, so a slow client always receives the latest state and never an
//! inconsistent one (row deltas are overlaid, style deltas concatenated).
//! Other messages are never dropped; if they alone exceed the hard cap the
//! client is not reading and the connection is closed.

use std::collections::VecDeque;
use std::sync::Arc;

use berth_core::{DaemonMsg, Event, ScreenUpdate};
use parking_lot::Mutex;
use tokio::sync::Notify;

/// Queue length at which a non-reading client is disconnected.
pub const HARD_CAP: usize = 4096;

#[derive(Debug, Default)]
pub struct Outbox {
    inner: Mutex<Inner>,
    notify: Notify,
}

#[derive(Debug, Default)]
struct Inner {
    queue: VecDeque<DaemonMsg>,
    closed: bool,
}

impl Outbox {
    pub fn new() -> Arc<Outbox> {
        Arc::new(Outbox::default())
    }

    /// Queue a message; `false` if the outbox is closed (or just overflowed).
    pub fn push(&self, msg: DaemonMsg) -> bool {
        let mut inner = self.inner.lock();
        if inner.closed {
            return false;
        }
        if let Some(msg) = coalesce(&mut inner.queue, msg) {
            if inner.queue.len() >= HARD_CAP {
                tracing::warn!(
                    queued = inner.queue.len(),
                    "client not reading; disconnecting"
                );
                inner.closed = true;
                inner.queue.clear();
                drop(inner);
                self.notify.notify_one();
                return false;
            }
            inner.queue.push_back(msg);
        }
        drop(inner);
        self.notify.notify_one();
        true
    }

    /// Stop accepting messages; already queued ones are still delivered.
    pub fn close(&self) {
        self.inner.lock().closed = true;
        self.notify.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().closed
    }

    pub fn len(&self) -> usize {
        self.inner.lock().queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Everything queued so far; waits while empty. `None` once closed and
    /// drained.
    pub async fn next_batch(&self) -> Option<Vec<DaemonMsg>> {
        loop {
            {
                let mut inner = self.inner.lock();
                if !inner.queue.is_empty() {
                    return Some(inner.queue.drain(..).collect());
                }
                if inner.closed {
                    return None;
                }
            }
            // `notify_one` stores a permit when nobody waits, so a push between
            // the check above and this await is not lost.
            self.notify.notified().await;
        }
    }
}

/// Merge `msg` into a queued message of the same kind and session; returns
/// the message back when it has to be queued separately.
fn coalesce(queue: &mut VecDeque<DaemonMsg>, msg: DaemonMsg) -> Option<DaemonMsg> {
    match msg.event {
        Event::Screen(new) => {
            let slot = queue.iter_mut().rev().find_map(|m| match &mut m.event {
                Event::Screen(old) if old.session == new.session => Some(old),
                _ => None,
            });
            match slot {
                Some(old) => {
                    merge_screen(old, new);
                    None
                }
                None => Some(DaemonMsg {
                    reply_to: msg.reply_to,
                    event: Event::Screen(new),
                }),
            }
        }
        Event::Preview {
            session,
            lines,
            styles,
        } => {
            let slot = queue.iter_mut().rev().find_map(|m| match &mut m.event {
                Event::Preview {
                    session: s,
                    lines: l,
                    styles: st,
                } if *s == session => Some((l, st)),
                _ => None,
            });
            match slot {
                Some((old_lines, old_styles)) => {
                    *old_lines = lines;
                    old_styles.extend(styles);
                    None
                }
                None => Some(DaemonMsg {
                    reply_to: msg.reply_to,
                    event: Event::Preview {
                        session,
                        lines,
                        styles,
                    },
                }),
            }
        }
        event => Some(DaemonMsg {
            reply_to: msg.reply_to,
            event,
        }),
    }
}

/// `old` then `new` ≡ merged: rows not repainted by `new` keep the content
/// `old` carried; style deltas accumulate; everything else is `new`'s.
pub(crate) fn merge_screen(old: &mut ScreenUpdate, new: ScreenUpdate) {
    let mut styles = std::mem::take(&mut old.styles);
    styles.extend(new.styles);
    if new.full || new.dims != old.dims {
        let full = new.full;
        *old = ScreenUpdate { styles, ..new };
        old.full = full;
        return;
    }
    for (row, line) in new.lines {
        match old.lines.iter_mut().find(|(r, _)| *r == row) {
            Some(slot) => slot.1 = line,
            None => old.lines.push((row, line)),
        }
    }
    old.seq = new.seq;
    old.cursor = new.cursor;
    old.modes = new.modes;
    old.display_offset = new.display_offset;
    old.history_len = new.history_len;
    old.styles = styles;
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_core::{Dims, LineSnapshot, SessionId, Style, StyleId};

    fn line(t: &str) -> LineSnapshot {
        let mut l = LineSnapshot::blank();
        l.push_str(t, StyleId::DEFAULT);
        l
    }

    fn screen(
        session: SessionId,
        seq: u64,
        full: bool,
        lines: &[(u16, &str)],
        style: u32,
    ) -> DaemonMsg {
        DaemonMsg {
            reply_to: None,
            event: Event::Screen(ScreenUpdate {
                session,
                seq,
                dims: Dims { cols: 80, rows: 3 },
                full,
                lines: lines.iter().map(|(r, t)| (*r, line(t))).collect(),
                cursor: Default::default(),
                modes: Default::default(),
                display_offset: 0,
                history_len: seq,
                styles: vec![(StyleId(style), Style::default())],
            }),
        }
    }

    fn only_screen(ob: &Outbox) -> ScreenUpdate {
        let mut q = ob.inner.lock();
        assert_eq!(q.queue.len(), 1);
        match q.queue.pop_front().unwrap().event {
            Event::Screen(s) => s,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn screen_deltas_merge_into_consistent_update() {
        let ob = Outbox::new();
        let s = SessionId::new();
        ob.push(screen(s, 1, true, &[(0, "a"), (1, "b"), (2, "c")], 1));
        ob.push(screen(s, 2, false, &[(1, "B")], 2));
        ob.push(screen(s, 3, false, &[(2, "C"), (1, "BB")], 3));
        let merged = only_screen(&ob);
        assert!(merged.full);
        assert_eq!(merged.seq, 3);
        assert_eq!(merged.history_len, 3);
        let mut rows: Vec<(u16, String)> =
            merged.lines.iter().map(|(r, l)| (*r, l.text())).collect();
        rows.sort();
        assert_eq!(
            rows,
            vec![(0, "a".into()), (1, "BB".into()), (2, "C".into())]
        );
        let ids: Vec<u32> = merged.styles.iter().map(|(id, _)| id.0).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn full_update_supersedes_but_keeps_style_deltas() {
        let ob = Outbox::new();
        let s = SessionId::new();
        ob.push(screen(s, 1, false, &[(0, "x")], 5));
        ob.push(screen(s, 2, true, &[(0, "y"), (1, "z")], 6));
        let merged = only_screen(&ob);
        assert!(merged.full);
        assert_eq!(merged.lines.len(), 2);
        assert_eq!(merged.styles.len(), 2);
    }

    #[test]
    fn previews_keep_latest_and_other_sessions_are_separate() {
        let ob = Outbox::new();
        let (a, b) = (SessionId::new(), SessionId::new());
        let preview = |s, t: &str| DaemonMsg {
            reply_to: None,
            event: Event::Preview {
                session: s,
                lines: vec![line(t)],
                styles: vec![],
            },
        };
        ob.push(preview(a, "1"));
        ob.push(DaemonMsg {
            reply_to: Some(9),
            event: Event::Ok,
        });
        ob.push(preview(a, "2"));
        ob.push(preview(b, "x"));
        ob.push(screen(a, 1, false, &[(0, "s")], 1));
        let q: Vec<DaemonMsg> = ob.inner.lock().queue.drain(..).collect();
        assert_eq!(q.len(), 4);
        match &q[0].event {
            Event::Preview { lines, .. } => assert_eq!(lines[0].text(), "2"),
            other => panic!("{other:?}"),
        }
        assert_eq!(q[1].reply_to, Some(9));
    }

    #[test]
    fn overflow_of_control_messages_closes() {
        let ob = Outbox::new();
        for i in 0..HARD_CAP {
            assert!(ob.push(DaemonMsg {
                reply_to: Some(i as u32),
                event: Event::Ok
            }));
        }
        assert!(!ob.push(DaemonMsg {
            reply_to: None,
            event: Event::Ok
        }));
        assert!(ob.is_closed());
        assert!(ob.is_empty());
    }

    #[tokio::test]
    async fn close_drains_then_ends() {
        let ob = Outbox::new();
        ob.push(DaemonMsg {
            reply_to: Some(1),
            event: Event::Ok,
        });
        ob.close();
        assert!(!ob.push(DaemonMsg {
            reply_to: Some(2),
            event: Event::Ok
        }));
        assert_eq!(ob.next_batch().await.unwrap().len(), 1);
        assert!(ob.next_batch().await.is_none());
    }
}
