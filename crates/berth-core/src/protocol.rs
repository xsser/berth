//! Client ⟷ daemon wire protocol over a Unix socket.
//!
//! Framing: `u32` little-endian payload length followed by a `postcard`
//! encoded `ClientMsg` (client → daemon) or `DaemonMsg` (daemon → client).
//! The first client frame must be `Request::Hello`; the daemon answers with
//! `Event::Hello`. Requests carry a client-chosen `id`; replies that answer a
//! specific request set `reply_to`. Unsolicited events have `reply_to: None`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::agent::{AgentInfo, HookEnvelope};
use crate::ids::{SessionId, WorkspaceId};
use crate::session::{PersistPolicy, SessionMeta, Workspace};
use crate::snapshot::{CursorState, LineSnapshot, TermModes};
use crate::style::{Style, StyleId};

/// Refuse frames larger than this (defensive; a full 100k-line screen fetch is
/// far below it).
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;
pub const FRAME_HEADER_LEN: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientRole {
    Gui,
    Hook,
    Cli,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dims {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubscribeMode {
    /// Full-resolution screen deltas (the focused session).
    Full,
    /// Throttled tail for the sidebar.
    Preview { rows: u8, max_hz: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviveMode {
    /// New shell in the session's cwd.
    Shell,
    /// `agents.<kind>.resume_command` with the stored external id.
    ResumeAgent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Hello {
        role: ClientRole,
        protocol: u32,
        client_version: String,
    },

    ListWorkspaces,
    CreateWorkspace {
        name: String,
        root: PathBuf,
    },
    RenameWorkspace {
        id: WorkspaceId,
        name: String,
    },
    DeleteWorkspace {
        id: WorkspaceId,
    },

    ListSessions,
    CreateSession {
        workspace: WorkspaceId,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
        title: Option<String>,
        dims: Dims,
    },
    Attach {
        session: SessionId,
        dims: Dims,
    },
    Detach {
        session: SessionId,
    },
    Resize {
        session: SessionId,
        dims: Dims,
    },
    Input {
        session: SessionId,
        data: Vec<u8>,
    },
    /// Fetch lines from the virtual history space: index 0 is the oldest
    /// restored line; the live scrollback follows the restored prefix.
    FetchLines {
        session: SessionId,
        start: u64,
        count: u32,
    },
    Subscribe {
        session: SessionId,
        mode: SubscribeMode,
    },
    Unsubscribe {
        session: SessionId,
    },
    Kill {
        session: SessionId,
    },
    Revive {
        session: SessionId,
        mode: ReviveMode,
    },
    /// Remove the session and purge its files.
    Delete {
        session: SessionId,
    },
    MarkRead {
        session: SessionId,
    },
    Rename {
        session: SessionId,
        title: Option<String>,
    },
    MoveSession {
        session: SessionId,
        workspace: WorkspaceId,
        order: u32,
    },
    SetPersist {
        session: SessionId,
        policy: PersistPolicy,
    },

    /// Sent by `berth-hook`.
    Hook(HookEnvelope),

    DaemonStatus,
    Shutdown,

    // Added in M3. New variants go last: postcard numbers variants by
    // position, so an older daemon still decodes every earlier request (and
    // answers these with an `Error` without `reply_to`).
    /// The session's most recent agent events (newest first), at most
    /// `limit`; answered by `Event::Events`.
    ListEvents {
        session: SessionId,
        limit: u32,
    },
    /// What `Revive { ResumeAgent }` would run for this session, without
    /// running it; answered by `Event::ResumeCommand`.
    ResumeCommand {
        session: SessionId,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientMsg {
    pub id: u32,
    pub req: Request,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScreenUpdate {
    pub session: SessionId,
    /// Monotonic per session; clients drop out-of-order updates.
    pub seq: u64,
    pub dims: Dims,
    /// When true `lines` contains every row.
    pub full: bool,
    pub lines: Vec<(u16, LineSnapshot)>,
    pub cursor: CursorState,
    pub modes: TermModes,
    pub display_offset: u32,
    /// Total virtual history lines (restored prefix + live scrollback).
    pub history_len: u64,
    /// Styles first used since the previous update (full table on attach).
    pub styles: Vec<(StyleId, Style)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub pid: u32,
    pub uptime_ms: i64,
    pub sessions_live: u32,
    pub sessions_total: u32,
}

/// One row of a session's agent event log (DESIGN §9 / §11: event kind and
/// a short detail such as a tool name or exit code, never prompt text).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEntry {
    pub at_ms: i64,
    /// e.g. `hook:PreToolUse`, `osc:133D`, `heuristic:idle`, `pty:exit`.
    pub kind: String,
    /// `AgentState::name()` after the event.
    pub state: String,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    Hello {
        daemon_version: String,
        protocol: u32,
    },
    Incompatible {
        daemon_protocol: u32,
    },
    /// Generic success for requests without a payload.
    Ok,
    Error {
        message: String,
    },

    Workspaces(Vec<Workspace>),
    WorkspaceUpdated(Workspace),
    WorkspaceRemoved(WorkspaceId),

    Sessions(Vec<SessionMeta>),
    SessionUpdated(SessionMeta),
    SessionRemoved(SessionId),

    Screen(ScreenUpdate),
    Lines {
        session: SessionId,
        start: u64,
        lines: Vec<LineSnapshot>,
        styles: Vec<(StyleId, Style)>,
    },
    Preview {
        session: SessionId,
        lines: Vec<LineSnapshot>,
        styles: Vec<(StyleId, Style)>,
    },

    Title {
        session: SessionId,
        title: String,
    },
    Cwd {
        session: SessionId,
        path: PathBuf,
    },
    Bell {
        session: SessionId,
    },
    Notify {
        session: SessionId,
        title: Option<String>,
        body: String,
    },
    AgentChanged {
        session: SessionId,
        agent: AgentInfo,
    },
    Exited {
        session: SessionId,
        code: Option<i32>,
    },

    Status(DaemonStatus),

    // Added in M3 (appended, see `Request::ListEvents`).
    /// Answer to `ListEvents`: newest first.
    Events {
        session: SessionId,
        events: Vec<EventEntry>,
    },
    /// Answer to `ResumeCommand`: the argv `Revive { ResumeAgent }` would
    /// execute directly (no shell) in `cwd`, or why it would refuse.
    ResumeCommand {
        session: SessionId,
        cwd: PathBuf,
        command: Result<Vec<String>, String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonMsg {
    pub reply_to: Option<u32>,
    pub event: Event,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame too large: {0} bytes")]
    TooLarge(usize),
    #[error("encode: {0}")]
    Encode(postcard::Error),
    #[error("decode: {0}")]
    Decode(postcard::Error),
}

/// Encode one message with its length prefix.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let payload = postcard::to_stdvec(msg).map_err(FrameError::Encode)?;
    if payload.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(payload.len()));
    }
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn decode_payload<'a, T: Deserialize<'a>>(payload: &'a [u8]) -> Result<T, FrameError> {
    postcard::from_bytes(payload).map_err(FrameError::Decode)
}

/// Incremental frame splitter for a byte stream.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete payload, if any.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if self.buf.len() < FRAME_HEADER_LEN {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge(len));
        }
        if self.buf.len() < FRAME_HEADER_LEN + len {
            return Ok(None);
        }
        let payload = self.buf[FRAME_HEADER_LEN..FRAME_HEADER_LEN + len].to_vec();
        self.buf.drain(..FRAME_HEADER_LEN + len);
        Ok(Some(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_split_across_pushes() {
        let m1 = ClientMsg {
            id: 1,
            req: Request::ListSessions,
        };
        let m2 = ClientMsg {
            id: 2,
            req: Request::Input {
                session: SessionId::new(),
                data: b"ls\n".to_vec(),
            },
        };
        let mut stream = encode_frame(&m1).unwrap();
        stream.extend(encode_frame(&m2).unwrap());

        let mut r = FrameReader::new();
        let (a, b) = stream.split_at(stream.len() / 2 + 1);
        r.push(a);
        let first = r
            .next_frame()
            .unwrap()
            .map(|p| decode_payload::<ClientMsg>(&p).unwrap());
        r.push(b);
        let first = first.or_else(|| r.next_frame().unwrap().map(|p| decode_payload(&p).unwrap()));
        let second: ClientMsg = decode_payload(&r.next_frame().unwrap().unwrap()).unwrap();
        assert_eq!(first, Some(m1));
        assert_eq!(second, m2);
        assert!(r.next_frame().unwrap().is_none());
    }

    #[test]
    fn oversized_header_rejected() {
        let mut r = FrameReader::new();
        r.push(&(MAX_FRAME_LEN as u32 + 1).to_le_bytes());
        assert!(matches!(r.next_frame(), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn daemon_msg_roundtrip() {
        let msg = DaemonMsg {
            reply_to: Some(7),
            event: Event::Screen(ScreenUpdate {
                session: SessionId::new(),
                seq: 1,
                dims: Dims { cols: 80, rows: 24 },
                full: true,
                lines: vec![(0, LineSnapshot::blank())],
                cursor: CursorState::default(),
                modes: TermModes::SHOW_CURSOR,
                display_offset: 0,
                history_len: 0,
                styles: vec![(StyleId(1), Style::default())],
            }),
        };
        let bytes = encode_frame(&msg).unwrap();
        let mut r = FrameReader::new();
        r.push(&bytes);
        let back: DaemonMsg = decode_payload(&r.next_frame().unwrap().unwrap()).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn m3_messages_roundtrip() {
        let session = SessionId::new();
        for req in [
            Request::ListEvents { session, limit: 5 },
            Request::ResumeCommand { session },
        ] {
            let msg = ClientMsg { id: 3, req };
            let back: ClientMsg = decode_payload(&encode_frame(&msg).unwrap()[4..]).unwrap();
            assert_eq!(msg, back);
        }
        for event in [
            Event::Events {
                session,
                events: vec![EventEntry {
                    at_ms: 1,
                    kind: "hook:PreToolUse".into(),
                    state: "tool_running".into(),
                    detail: Some("Bash".into()),
                }],
            },
            Event::ResumeCommand {
                session,
                cwd: PathBuf::from("/tmp/p"),
                command: Ok(vec!["claude".into(), "--resume".into(), "x".into()]),
            },
            Event::ResumeCommand {
                session,
                cwd: PathBuf::from("/tmp/p"),
                command: Err("no external id".into()),
            },
        ] {
            let msg = DaemonMsg {
                reply_to: Some(3),
                event,
            };
            let back: DaemonMsg = decode_payload(&encode_frame(&msg).unwrap()[4..]).unwrap();
            assert_eq!(msg, back);
        }
    }

    /// A client stops a daemon of another protocol version by speaking that
    /// version (`Hello`, `Shutdown`, answered by `Hello`, `Incompatible`,
    /// `Ok` or `Error`), so these frames must keep their bytes in every
    /// protocol version.
    #[test]
    fn frames_spoken_across_protocol_versions_keep_their_bytes() {
        let hello = ClientMsg {
            id: 0,
            req: Request::Hello {
                role: ClientRole::Cli,
                protocol: 1,
                client_version: "v".into(),
            },
        };
        assert_eq!(postcard::to_stdvec(&hello).unwrap(), [0, 0, 2, 1, 1, b'v']);
        let shutdown = ClientMsg {
            id: 1,
            req: Request::Shutdown,
        };
        assert_eq!(postcard::to_stdvec(&shutdown).unwrap(), [1, 23]);
        let answers: [(Event, &[u8]); 4] = [
            (
                Event::Hello {
                    daemon_version: "v".into(),
                    protocol: 1,
                },
                &[0, 1, b'v', 1],
            ),
            (Event::Incompatible { daemon_protocol: 1 }, &[1, 1]),
            (Event::Ok, &[2]),
            (
                Event::Error {
                    message: "e".into(),
                },
                &[3, 1, b'e'],
            ),
        ];
        for (event, bytes) in answers {
            let msg = DaemonMsg {
                reply_to: Some(1),
                event,
            };
            assert_eq!(
                postcard::to_stdvec(&msg).unwrap(),
                [&[1, 1], bytes].concat()
            );
        }
    }

    /// The M3 variants are appended: every pre-M3 variant keeps its postcard
    /// discriminant, so M2 peers still decode each other's old messages.
    #[test]
    fn m3_variants_do_not_renumber_existing_ones() {
        let first_byte = |r: Request| postcard::to_stdvec(&r).unwrap()[0];
        assert_eq!(first_byte(Request::DaemonStatus), 22);
        assert_eq!(first_byte(Request::Shutdown), 23);
        let session = SessionId::new();
        assert_eq!(first_byte(Request::ListEvents { session, limit: 1 }), 24);
        assert_eq!(first_byte(Request::ResumeCommand { session }), 25);
        let status = Event::Status(DaemonStatus {
            version: String::new(),
            pid: 0,
            uptime_ms: 0,
            sessions_live: 0,
            sessions_total: 0,
        });
        let events = Event::Events {
            session,
            events: vec![],
        };
        let status_tag = postcard::to_stdvec(&status).unwrap()[0];
        assert_eq!(postcard::to_stdvec(&events).unwrap()[0], status_tag + 1);
    }

    /// A protocol 1 berthd (M1 / M2) decodes requests up to `Shutdown`, and
    /// `AgentInfo` without `last_agent`. Sent as protocol 1, the M3 messages
    /// would pass its `Hello` check and fail on first use; sent as a newer
    /// one, they are refused up front (`Incompatible`: the GUI's restart
    /// banner, `berth doctor`). Catches a merge that takes the bump back.
    #[test]
    fn m3_messages_are_not_sent_as_protocol_1() {
        const PROTOCOL_1_LAST_REQUEST: u8 = 23;
        let resume = Request::ResumeCommand {
            session: SessionId::new(),
        };
        let tag = postcard::to_stdvec(&resume).unwrap()[0];
        assert!(
            tag <= PROTOCOL_1_LAST_REQUEST || crate::PROTOCOL_VERSION > 1,
            "request {tag} is beyond protocol 1 but sent as protocol {}",
            crate::PROTOCOL_VERSION
        );
    }
}
