//! One-shot delivery to berthd: connect, write `Hello` + `Hook`, close —
//! all within a hard time budget. Never reads replies, never retries.

use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use berth_core::{encode_frame, ClientMsg, ClientRole, HookEnvelope, Request, PROTOCOL_VERSION};

/// Hard cap on a whole delivery, connect + write (task spec: 50 ms).
pub const BUDGET: Duration = Duration::from_millis(50);

pub fn frames(envelope: HookEnvelope) -> Result<Vec<u8>, String> {
    let hello = ClientMsg {
        id: 1,
        req: Request::Hello {
            role: ClientRole::Hook,
            protocol: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    };
    let hook = ClientMsg {
        id: 2,
        req: Request::Hook(envelope),
    };
    let mut bytes = encode_frame(&hello).map_err(|e| e.to_string())?;
    bytes.extend(encode_frame(&hook).map_err(|e| e.to_string())?);
    Ok(bytes)
}

/// Deliver `envelope`, returning within `budget` of the call however the
/// daemon behaves (absent, accepting but never reading, reading a trickle):
/// a helper thread connects and writes both frames while the caller waits
/// for it at most that long. On timeout the helper is abandoned mid-write
/// and dies with the process; the daemon sees a truncated frame and drops
/// the connection.
pub fn send(envelope: HookEnvelope, socket: &Path, budget: Duration) -> Result<(), String> {
    let deadline = Instant::now() + budget;
    let bytes = frames(envelope)?;
    let (tx, rx) = mpsc::channel();
    let path = socket.to_path_buf();
    std::thread::spawn(move || {
        let _ = tx.send(connect_and_write(&path, &bytes, budget));
    });
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap_or_else(|_| Err(format!("not delivered within {budget:?}")))
}

fn connect_and_write(socket: &Path, bytes: &[u8], budget: Duration) -> Result<(), String> {
    let mut stream =
        UnixStream::connect(socket).map_err(|e| format!("connect {}: {e}", socket.display()))?;
    // Not the cap (the caller's wait is): only bounds how long an abandoned
    // helper can sit in a write that makes no progress, e.g. while a chained
    // statusline command still runs.
    let _ = stream.set_write_timeout(Some(budget));
    stream.write_all(bytes).map_err(|e| format!("write: {e}"))?;
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    use berth_core::{AgentSignal, ClaudeHook, ClaudeHookEvent};

    use super::*;

    fn four_mib_envelope() -> HookEnvelope {
        HookEnvelope {
            berth_session: None,
            pid: 1,
            sent_at_ms: 0,
            signal: AgentSignal::Claude(ClaudeHook {
                session_id: "x".repeat(4 << 20),
                cwd: None,
                transcript_path: None,
                permission_mode: None,
                event: ClaudeHookEvent::Stop {
                    stop_hook_active: false,
                },
            }),
        }
    }

    /// A "daemon" that accepts one connection, then for 5 s reads `chunk`
    /// bytes every `every` (`chunk` 0: never reads).
    fn stalled_daemon(dir: &Path, chunk: usize, every: Duration) -> PathBuf {
        let socket = dir.join("berthd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let end = Instant::now() + Duration::from_secs(5);
            let mut buf = vec![0; chunk];
            while Instant::now() < end {
                std::thread::sleep(every);
                if chunk > 0 && stream.read(&mut buf).unwrap_or(0) == 0 {
                    break;
                }
            }
        });
        socket
    }

    /// Review #18: `BUDGET` caps the whole delivery (connect + both frames),
    /// not each syscall. Neither a daemon that accepts but never reads nor
    /// one that reads a trickle (every partial write would restart a
    /// per-write timeout) can hold the hook, and so the agent, past it.
    #[test]
    fn budget_is_a_hard_cap_on_the_whole_delivery() {
        let readers = [
            (0, Duration::from_millis(10)),
            (1024, Duration::from_millis(5)),
        ];
        for (chunk, every) in readers {
            let dir = tempfile::tempdir().unwrap();
            let socket = stalled_daemon(dir.path(), chunk, every);
            let envelope = four_mib_envelope();
            let start = Instant::now();
            let result = send(envelope, &socket, BUDGET);
            let took = start.elapsed();
            assert!(result.is_err(), "reader {chunk}B/{every:?}: {result:?}");
            // Loose on purpose: it only has to tell "capped" from "one
            // timeout per write". Without the cap the trickle reader takes
            // hours for 4 MiB, so any bound well under a second proves it —
            // while a loaded CI machine can still schedule the helper
            // thread tens of milliseconds late (seen: 113 ms for a 50 ms
            // budget). Tightening this to BUDGET + ε only buys flakes.
            assert!(
                took < Duration::from_millis(750),
                "reader {chunk}B/{every:?}: send took {took:?}"
            );
        }
    }
}
