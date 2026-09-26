//! Paste (integrate.md §2, §6): bracketed-paste wrapping and flow control.
//!
//! The daemon refuses an `Input` whole when the session already has 1 MiB
//! of unread input ("backpressure: …") and does not acknowledge accepted
//! input at all. A paste is therefore sent in chunks of at most
//! [`PASTE_CHUNK`]; each chunk is followed by a `DaemonStatus` request whose
//! reply is the barrier: requests on one connection are handled in order, so
//! when the barrier's reply arrives the chunk's own error (if any) has
//! already been seen. A refused chunk is retried with backoff; the next
//! chunk goes out only after the previous one was accepted, which keeps the
//! bytes in order.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use berth_core::TermModes;

pub const PASTE_CHUNK: usize = 64 * 1024;
const RETRY_MIN: Duration = Duration::from_millis(50);
const RETRY_MAX: Duration = Duration::from_millis(500);
/// Give up after the session refused input for this long without a break.
const GIVE_UP: Duration = Duration::from_secs(20);

/// The bytes a paste sends (Alacritty's rules): in bracketed-paste mode the
/// text is wrapped in `ESC[200~ … ESC[201~` with ESC and ^C removed so the
/// paste cannot terminate the bracket early; otherwise line breaks become
/// `\r` (what Enter sends).
pub fn encode(text: &str, modes: TermModes) -> Vec<u8> {
    if modes.contains(TermModes::BRACKETED_PASTE) {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend(text.bytes().filter(|b| *b != 0x1b && *b != 0x03));
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Split `bytes` into chunks of at most [`PASTE_CHUNK`], preferring UTF-8
/// character boundaries.
pub fn chunks(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let mut n = rest.len().min(PASTE_CHUNK);
        if n < rest.len() {
            // Back up over UTF-8 continuation bytes (at most 3).
            let mut k = n;
            while k > n.saturating_sub(3) && k > 0 && (rest[k] & 0xC0) == 0x80 {
                k -= 1;
            }
            if k > 0 {
                n = k;
            }
        }
        out.push(rest[..n].to_vec());
        rest = &rest[n..];
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sent {
    input: u32,
    barrier: u32,
    refused: bool,
}

/// Outcome of a reply that belonged to the job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobReply {
    /// Not ours.
    Unrelated,
    /// Consumed; the job goes on.
    Progress,
    /// The job was abandoned; show the message.
    Failed(String),
}

/// One in-order stream of input chunks for a session.
#[derive(Debug)]
pub struct PasteJob {
    queue: VecDeque<Vec<u8>>,
    sent: Option<Sent>,
    retry_at: Option<Instant>,
    backoff: Duration,
    refused_since: Option<Instant>,
}

impl PasteJob {
    pub fn new(bytes: &[u8]) -> PasteJob {
        PasteJob {
            queue: chunks(bytes).into(),
            sent: None,
            retry_at: None,
            backoff: RETRY_MIN,
            refused_since: None,
        }
    }

    /// Append input that must follow the paste (keys typed meanwhile).
    pub fn push(&mut self, bytes: &[u8]) {
        self.queue.extend(chunks(bytes));
    }

    pub fn is_done(&self) -> bool {
        self.queue.is_empty() && self.sent.is_none()
    }

    pub fn queued_bytes(&self) -> usize {
        self.queue.iter().map(Vec::len).sum()
    }

    /// The chunk to send now, if the previous one is settled and no retry
    /// delay is pending. Call [`PasteJob::sent`] with the request ids.
    pub fn next_chunk(&mut self, now: Instant) -> Option<&[u8]> {
        if self.sent.is_some() || self.retry_at.is_some_and(|t| now < t) {
            return None;
        }
        self.retry_at = None;
        self.queue.front().map(Vec::as_slice)
    }

    pub fn sent(&mut self, input: u32, barrier: u32) {
        self.sent = Some(Sent {
            input,
            barrier,
            refused: false,
        });
    }

    /// When a retry is due (for the event loop's timer).
    pub fn deadline(&self) -> Option<Instant> {
        self.retry_at
    }

    /// Route an `Error` reply.
    pub fn on_error(&mut self, reply_to: u32, message: &str, now: Instant) -> JobReply {
        match self.sent {
            Some(ref mut s) if s.input == reply_to => {
                if message.starts_with("backpressure:") {
                    s.refused = true;
                    let since = *self.refused_since.get_or_insert(now);
                    if now.duration_since(since) >= GIVE_UP {
                        let left = self.queued_bytes();
                        self.abandon();
                        return JobReply::Failed(format!(
                            "{message} — gave up after {} s, {left} bytes not sent",
                            GIVE_UP.as_secs()
                        ));
                    }
                    JobReply::Progress
                } else {
                    let left = self.queued_bytes();
                    self.abandon();
                    JobReply::Failed(format!("{message} ({left} bytes of input not sent)"))
                }
            }
            Some(s) if s.barrier == reply_to => {
                // The barrier itself failed: the chunk's fate is unknown.
                let left = self.queued_bytes();
                self.abandon();
                JobReply::Failed(format!("{message} ({left} bytes of input not sent)"))
            }
            _ => JobReply::Unrelated,
        }
    }

    /// Route a non-error reply (the barrier's `Status`).
    pub fn on_reply(&mut self, reply_to: u32, now: Instant) -> JobReply {
        match self.sent {
            Some(s) if s.barrier == reply_to => {
                self.sent = None;
                if s.refused {
                    self.retry_at = Some(now + self.backoff);
                    self.backoff = (self.backoff * 2).min(RETRY_MAX);
                } else {
                    self.queue.pop_front();
                    self.backoff = RETRY_MIN;
                    self.refused_since = None;
                }
                JobReply::Progress
            }
            _ => JobReply::Unrelated,
        }
    }

    fn abandon(&mut self) {
        self.queue.clear();
        self.sent = None;
        self.retry_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_paste_wraps_and_strips_escape_and_ctrl_c() {
        let got = encode("a\x1b[201~b\x03c\n", TermModes::BRACKETED_PASTE);
        assert_eq!(got, b"\x1b[200~a[201~bc\n\x1b[201~");
    }

    #[test]
    fn plain_paste_turns_line_breaks_into_carriage_returns() {
        assert_eq!(encode("a\r\nb\nc\rd", TermModes::empty()), b"a\rb\rc\rd");
        assert_eq!(encode("中文", TermModes::empty()), "中文".as_bytes());
    }

    #[test]
    fn chunks_respect_the_limit_and_utf8_boundaries() {
        // 3-byte characters: the limit falls inside one of them.
        let text = "中".repeat(PASTE_CHUNK / 2);
        let bytes = text.as_bytes();
        let parts = chunks(bytes);
        assert_eq!(parts[0].len(), PASTE_CHUNK - 1);
        assert!(parts.iter().all(|p| p.len() <= PASTE_CHUNK));
        assert!(parts.iter().all(|p| std::str::from_utf8(p).is_ok()));
        assert_eq!(parts.concat(), bytes);
        assert!(chunks(b"").is_empty());
        assert_eq!(chunks(&vec![b'x'; PASTE_CHUNK + 1]).len(), 2);
    }

    #[test]
    fn chunks_go_one_at_a_time_and_refused_ones_are_retried() {
        let t0 = Instant::now();
        let mut job = PasteJob::new(&vec![b'x'; PASTE_CHUNK * 2 + 10]);
        assert_eq!(job.next_chunk(t0).map(<[u8]>::len), Some(PASTE_CHUNK));
        job.sent(1, 2);
        assert_eq!(job.next_chunk(t0), None, "wait for the barrier");
        assert_eq!(job.on_reply(99, t0), JobReply::Unrelated);
        assert_eq!(job.on_reply(2, t0), JobReply::Progress);
        // Second chunk refused.
        assert!(job.next_chunk(t0).is_some());
        job.sent(3, 4);
        assert_eq!(
            job.on_error(3, "backpressure: session x is not reading", t0),
            JobReply::Progress
        );
        assert_eq!(job.on_reply(4, t0), JobReply::Progress);
        assert_eq!(job.next_chunk(t0), None, "backing off");
        let due = job.deadline().expect("retry scheduled");
        assert_eq!(
            job.next_chunk(due).map(<[u8]>::len),
            Some(PASTE_CHUNK),
            "same chunk again"
        );
        job.sent(5, 6);
        job.on_reply(6, due);
        assert_eq!(job.next_chunk(due).map(<[u8]>::len), Some(10));
        job.push(b"typed");
        job.sent(7, 8);
        job.on_reply(8, due);
        assert_eq!(job.next_chunk(due), Some(&b"typed"[..]));
        job.sent(9, 10);
        job.on_reply(10, due);
        assert!(job.is_done());
    }

    #[test]
    fn other_errors_and_endless_refusal_abandon_the_job() {
        let t0 = Instant::now();
        let mut job = PasteJob::new(b"abc");
        job.next_chunk(t0);
        job.sent(1, 2);
        match job.on_error(1, "session is not live", t0) {
            JobReply::Failed(m) => assert!(m.contains("session is not live"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(job.is_done());

        let mut job = PasteJob::new(b"abc");
        let mut now = t0;
        let mut ids = 0;
        loop {
            now = job.deadline().unwrap_or(now).max(now);
            assert!(job.next_chunk(now).is_some());
            ids += 2;
            job.sent(ids, ids + 1);
            match job.on_error(ids, "backpressure: full", now) {
                JobReply::Progress => {}
                JobReply::Failed(m) => {
                    assert!(m.contains("gave up"), "{m}");
                    break;
                }
                JobReply::Unrelated => panic!("unrelated"),
            }
            job.on_reply(ids + 1, now);
            now += Duration::from_secs(1);
        }
        assert!(job.is_done());
    }
}
