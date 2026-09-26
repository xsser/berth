//! Side-channel scanner for OSC sequences that `alacritty_terminal` ignores.
//!
//! Recognised: OSC 7 (`file://host/path` cwd), OSC 133 A/B/C/D (shell
//! integration prompt marks; `D;<exit>` carries the exit code), OSC 9 (iTerm
//! / ConEmu notification text) and OSC 777 (`notify;title;body`).
//!
//! The scanner is stateful so sequences split across PTY read chunks are
//! reassembled. Sequence payloads are capped (4 KiB) to bound memory; a
//! longer sequence is discarded. Everything else passes through untouched —
//! the scanner never modifies the byte stream.
//!
//! Sequence boundaries deliberately mirror vte 0.15 (the parser inside
//! `alacritty_terminal`), so both sides agree on where every OSC starts and
//! ends:
//! - `ESC` enters the escape state from anywhere; `ESC ]` starts an OSC.
//! - An OSC ends at `BEL`, at `ESC` (normally the first half of `ESC \` = ST),
//!   or at `CAN`/`SUB`; vte dispatches the collected payload in all of these
//!   cases, and so does this scanner.
//! - Other C0 controls inside an OSC are dropped from the payload; 8-bit C1
//!   controls are not recognised (vte is 7-bit only).

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptMark {
    /// 133;A — prompt is about to be drawn (shell is idle).
    PromptStart,
    /// 133;B — user finished typing, command about to run.
    CommandStart,
    /// 133;C — command output begins.
    OutputStart,
    /// 133;D[;exit] — command finished.
    CommandEnd { exit_code: Option<i32> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OscEvent {
    Cwd(PathBuf),
    Prompt(PromptMark),
    Notify { title: Option<String>, body: String },
}

pub const MAX_OSC_LEN: usize = 4096;

const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const ESC: u8 = 0x1b;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    /// Plain text or the body of a non-OSC sequence: only `ESC` matters.
    #[default]
    Ground,
    /// Saw `ESC`.
    Escape,
    /// `ESC` followed by intermediate bytes (e.g. `ESC ( B`); the next final
    /// byte ends it, so a `]` here is *not* an OSC introducer.
    EscapeIntermediate,
    /// Inside `ESC ]`, collecting the payload.
    Osc,
    /// Inside an OSC whose payload exceeded [`MAX_OSC_LEN`]; skipping to its
    /// terminator without buffering.
    OscDiscard,
}

/// Incremental OSC scanner. See module docs.
#[derive(Debug, Default)]
pub struct OscPrescanner {
    state: State,
    payload: Vec<u8>,
}

impl OscPrescanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan a chunk; returns events found (possibly completing a sequence
    /// started in a previous chunk).
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<OscEvent> {
        let mut events = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if self.state == State::Ground {
                match bytes[i..].iter().position(|&b| b == ESC) {
                    Some(offset) => {
                        i += offset + 1;
                        self.state = State::Escape;
                        continue;
                    }
                    None => break,
                }
            }

            let byte = bytes[i];
            i += 1;
            self.state = match self.state {
                State::Ground => unreachable!("handled above"),
                State::Escape => match byte {
                    b']' => {
                        self.payload.clear();
                        State::Osc
                    }
                    CAN | SUB => State::Ground,
                    0x20..=0x2f => State::EscapeIntermediate,
                    // Final byte of a two-byte escape, or the introducer of
                    // CSI / DCS / SOS / PM / APC: none of them can contain an
                    // OSC (they all end at the next ESC at the latest).
                    0x30..=0x7e => State::Ground,
                    // C0 controls execute without leaving the escape state,
                    // ESC restarts it, DEL and non-ASCII bytes are ignored.
                    _ => State::Escape,
                },
                State::EscapeIntermediate => match byte {
                    ESC => State::Escape,
                    CAN | SUB => State::Ground,
                    0x30..=0x7e => State::Ground,
                    _ => State::EscapeIntermediate,
                },
                State::Osc => match byte {
                    BEL | CAN | SUB => {
                        self.dispatch(&mut events);
                        State::Ground
                    }
                    ESC => {
                        self.dispatch(&mut events);
                        State::Escape
                    }
                    0x00..=0x1f => State::Osc,
                    _ if self.payload.len() >= MAX_OSC_LEN => {
                        self.payload.clear();
                        State::OscDiscard
                    }
                    _ => {
                        self.payload.push(byte);
                        State::Osc
                    }
                },
                State::OscDiscard => match byte {
                    BEL | CAN | SUB => State::Ground,
                    ESC => State::Escape,
                    _ => State::OscDiscard,
                },
            };
        }
        events
    }

    fn dispatch(&mut self, events: &mut Vec<OscEvent>) {
        if let Some(event) = parse_payload(&self.payload) {
            events.push(event);
        }
        self.payload.clear();
    }
}

/// Interpret one complete OSC payload (the bytes between `ESC ]` and the
/// terminator).
fn parse_payload(payload: &[u8]) -> Option<OscEvent> {
    let (code, rest) = split_once(payload, b';')?;
    match code {
        b"7" => parse_cwd(rest).map(OscEvent::Cwd),
        b"133" => parse_prompt_mark(rest).map(OscEvent::Prompt),
        b"9" => parse_osc9(rest),
        b"777" => parse_osc777(rest),
        _ => None,
    }
}

fn split_once(bytes: &[u8], separator: u8) -> Option<(&[u8], &[u8])> {
    let pos = bytes.iter().position(|&b| b == separator)?;
    Some((&bytes[..pos], &bytes[pos + 1..]))
}

/// `7;file://<host>/<percent-encoded path>`. Any host is accepted (remote
/// hosts included): the daemon only needs the path. kitty's
/// `kitty-shell-cwd://` variant carries the path unencoded.
fn parse_cwd(url: &[u8]) -> Option<PathBuf> {
    let (rest, percent_encoded) = if let Some(rest) = url.strip_prefix(b"file://") {
        (rest, true)
    } else {
        (url.strip_prefix(b"kitty-shell-cwd://")?, false)
    };
    let path_start = rest.iter().position(|&b| b == b'/')?;
    let path = &rest[path_start..];
    let bytes = if percent_encoded {
        percent_decode(path)
    } else {
        path.to_vec()
    };
    Some(path_from_bytes(bytes))
}

/// `133;<kind>[;<params>...]`; only `D` interprets a parameter (the exit
/// code). Other kinds (`P`, `L`, `N`, ...) and key=value options are ignored.
fn parse_prompt_mark(rest: &[u8]) -> Option<PromptMark> {
    let mut params = rest.split(|&b| b == b';');
    match params.next()? {
        b"A" => Some(PromptMark::PromptStart),
        b"B" => Some(PromptMark::CommandStart),
        b"C" => Some(PromptMark::OutputStart),
        b"D" => {
            let exit_code = params
                .next()
                .and_then(|p| std::str::from_utf8(p).ok()?.trim().parse().ok());
            Some(PromptMark::CommandEnd { exit_code })
        }
        _ => None,
    }
}

/// `9;<body>` (iTerm2 / ConEmu notification). ConEmu's `9;4;<state>[;<pct>]`
/// progress report shares the code but is not a notification, so it is
/// ignored, as is an empty body.
fn parse_osc9(body: &[u8]) -> Option<OscEvent> {
    if body.is_empty() || is_conemu_progress(body) {
        return None;
    }
    Some(OscEvent::Notify {
        title: None,
        body: lossy(body),
    })
}

fn is_conemu_progress(body: &[u8]) -> bool {
    body.strip_prefix(b"4;").is_some_and(|params| {
        params
            .split(|&b| b == b';')
            .all(|p| p.iter().all(u8::is_ascii_digit))
    })
}

/// `777;notify;<title>;<body>` (urxvt / Ghostty / WezTerm). The body may
/// itself contain `;`. Other 777 sub-commands are ignored.
fn parse_osc777(rest: &[u8]) -> Option<OscEvent> {
    let mut parts = rest.splitn(3, |&b| b == b';');
    if parts.next()? != b"notify" {
        return None;
    }
    let title = parts.next().unwrap_or_default();
    let body = parts.next().unwrap_or_default();
    if title.is_empty() && body.is_empty() {
        return None;
    }
    let title = (!title.is_empty()).then(|| lossy(title));
    Some(OscEvent::Notify {
        title,
        body: lossy(body),
    })
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Decode `%XX` escapes; malformed escapes are kept literally.
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            if let (Some(hi), Some(lo)) = (hex_value(input[i + 1]), hex_value(input[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(input: &[u8]) -> Vec<OscEvent> {
        OscPrescanner::new().scan(input)
    }

    fn scan_bytewise(input: &[u8]) -> Vec<OscEvent> {
        let mut scanner = OscPrescanner::new();
        input
            .iter()
            .flat_map(|b| scanner.scan(std::slice::from_ref(b)))
            .collect()
    }

    #[test]
    fn bel_terminated() {
        assert_eq!(
            scan_all(b"\x1b]133;A\x07"),
            vec![OscEvent::Prompt(PromptMark::PromptStart)]
        );
    }

    #[test]
    fn st_terminated() {
        assert_eq!(
            scan_all(b"\x1b]133;B\x1b\\"),
            vec![OscEvent::Prompt(PromptMark::CommandStart)]
        );
        assert_eq!(
            scan_all(b"\x1b]133;C\x1b\\"),
            vec![OscEvent::Prompt(PromptMark::OutputStart)]
        );
    }

    #[test]
    fn command_end_exit_code() {
        assert_eq!(
            scan_all(b"\x1b]133;D;0\x07"),
            vec![OscEvent::Prompt(PromptMark::CommandEnd {
                exit_code: Some(0)
            })]
        );
        assert_eq!(
            scan_all(b"\x1b]133;D;130;aid=7\x07"),
            vec![OscEvent::Prompt(PromptMark::CommandEnd {
                exit_code: Some(130)
            })]
        );
        assert_eq!(
            scan_all(b"\x1b]133;D\x07\x1b]133;D;x\x07"),
            vec![
                OscEvent::Prompt(PromptMark::CommandEnd { exit_code: None }),
                OscEvent::Prompt(PromptMark::CommandEnd { exit_code: None }),
            ]
        );
    }

    #[test]
    fn prompt_mark_options_and_unknown_kinds() {
        assert_eq!(
            scan_all(b"\x1b]133;A;cl=m;aid=1\x07\x1b]133;P;k=i\x07\x1b]133\x07"),
            vec![OscEvent::Prompt(PromptMark::PromptStart)]
        );
    }

    #[test]
    fn cwd_is_percent_decoded_for_any_host() {
        assert_eq!(
            scan_all(b"\x1b]7;file://Mac/Users/x/a%20b\x07"),
            vec![OscEvent::Cwd(PathBuf::from("/Users/x/a b"))]
        );
        assert_eq!(
            scan_all(b"\x1b]7;file:///tmp/%E4%BD%A0%zz\x1b\\"),
            vec![OscEvent::Cwd(PathBuf::from("/tmp/你%zz"))]
        );
        assert_eq!(
            scan_all(b"\x1b]7;kitty-shell-cwd://h/a%20b\x07"),
            vec![OscEvent::Cwd(PathBuf::from("/a%20b"))]
        );
        assert!(scan_all(b"\x1b]7;file://host-without-path\x07\x1b]7;http://x/y\x07").is_empty());
    }

    #[test]
    fn notifications() {
        assert_eq!(
            scan_all(b"\x1b]9;build done; 3 warnings\x07"),
            vec![OscEvent::Notify {
                title: None,
                body: "build done; 3 warnings".into()
            }]
        );
        assert_eq!(
            scan_all("\x1b]777;notify;标题;正文;含分号\x1b\\".as_bytes()),
            vec![OscEvent::Notify {
                title: Some("标题".into()),
                body: "正文;含分号".into()
            }]
        );
        assert_eq!(
            scan_all(b"\x1b]777;notify;;only body\x07"),
            vec![OscEvent::Notify {
                title: None,
                body: "only body".into()
            }]
        );
        // ConEmu progress, empty bodies and other 777 sub-commands are not notifications.
        assert!(
            scan_all(b"\x1b]9;4;1;50\x07\x1b]9;4;0\x07\x1b]9;\x07\x1b]777;precmd\x07").is_empty()
        );
    }

    #[test]
    fn one_byte_chunks_reassemble() {
        let input = b"pre\x1b]133;D;2\x07mid\x1b]7;file://h/p%2Fq\x1b\\post\x1b]9;hi\x07";
        let expected = vec![
            OscEvent::Prompt(PromptMark::CommandEnd { exit_code: Some(2) }),
            OscEvent::Cwd(PathBuf::from("/p/q")),
            OscEvent::Notify {
                title: None,
                body: "hi".into(),
            },
        ];
        assert_eq!(scan_all(input), expected);
        assert_eq!(scan_bytewise(input), expected);
    }

    #[test]
    fn non_osc_escapes_do_not_trigger() {
        let input: &[u8] = b"\x1b[31mred\x1b[0m\x1b7\x1b8\x1b[?1049h\x1b(B\x1b)]\x1bPq]133;A\x1b\\\
            \x1b_]133;A\x1b\\text ]133;A\x07 \x1b[]133;A\x07";
        assert!(scan_all(input).is_empty());
        assert!(scan_bytewise(input).is_empty());
    }

    #[test]
    fn oversized_payload_is_discarded_and_scanner_recovers() {
        let mut input = b"\x1b]9;".to_vec();
        input.extend(std::iter::repeat_n(b'x', MAX_OSC_LEN + 10));
        input.extend_from_slice(b"\x07\x1b]133;A\x07");
        assert_eq!(
            scan_all(&input),
            vec![OscEvent::Prompt(PromptMark::PromptStart)]
        );
        assert_eq!(
            scan_bytewise(&input),
            vec![OscEvent::Prompt(PromptMark::PromptStart)]
        );

        // A payload of exactly MAX_OSC_LEN bytes is still accepted.
        let body = "y".repeat(MAX_OSC_LEN - 2);
        let exact = format!("\x1b]9;{body}\x07");
        assert_eq!(
            scan_all(exact.as_bytes()),
            vec![OscEvent::Notify { title: None, body }]
        );
    }

    #[test]
    fn terminators_follow_vte() {
        // ESC that is not part of ST still ends the OSC (vte dispatches it).
        assert_eq!(
            scan_all(b"\x1b]133;A\x1b[0m"),
            vec![OscEvent::Prompt(PromptMark::PromptStart)]
        );
        // CAN aborts but vte dispatches the collected payload anyway.
        assert_eq!(
            scan_all(b"\x1b]133;B\x18"),
            vec![OscEvent::Prompt(PromptMark::CommandStart)]
        );
        // C0 controls inside the payload are dropped, not terminators.
        assert_eq!(
            scan_all(b"\x1b]133;\nA\x07"),
            vec![OscEvent::Prompt(PromptMark::PromptStart)]
        );
    }

    #[test]
    fn split_across_chunk_boundaries_keeps_state() {
        let mut scanner = OscPrescanner::new();
        assert!(scanner.scan(b"abc\x1b").is_empty());
        assert!(scanner.scan(b"]13").is_empty());
        assert!(scanner.scan(b"3;D;4").is_empty());
        // Like vte, the OSC is dispatched at the ESC that starts ST.
        assert_eq!(
            scanner.scan(b"2\x1b"),
            vec![OscEvent::Prompt(PromptMark::CommandEnd {
                exit_code: Some(42)
            })]
        );
        assert!(scanner.scan(b"\\tail").is_empty());
    }
}
