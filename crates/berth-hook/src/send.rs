//! One-shot delivery to berthd: connect (bounded), write `Hello` + `Hook`,
//! close. Never reads replies, never retries.

use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use berth_core::{encode_frame, ClientMsg, ClientRole, HookEnvelope, Request, PROTOCOL_VERSION};

/// Total budget for connect + write (task spec: 50 ms).
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

pub fn send(envelope: HookEnvelope, socket: &Path, budget: Duration) -> Result<(), String> {
    let start = Instant::now();
    let bytes = frames(envelope)?;
    // std has no connect timeout for Unix sockets; a helper thread bounds it
    // (the thread is abandoned on timeout and dies with the process).
    let (tx, rx) = mpsc::channel();
    let path = socket.to_path_buf();
    std::thread::spawn(move || {
        let _ = tx.send(UnixStream::connect(path));
    });
    let mut stream = rx
        .recv_timeout(budget)
        .map_err(|_| "connect timed out".to_string())?
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let left = budget
        .saturating_sub(start.elapsed())
        .max(Duration::from_millis(1));
    stream
        .set_write_timeout(Some(left))
        .map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(left))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(&bytes)
        .map_err(|e| format!("write: {e}"))?;
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}
