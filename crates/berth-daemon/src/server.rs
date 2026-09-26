//! Unix socket server. One task per connection: the reader splits frames
//! (`FrameReader`), insists on `Hello` first, and dispatches requests to the
//! `Manager` in order; a writer task drains the connection's `Outbox`.

use std::sync::Arc;
use std::time::Duration;

use berth_core::{
    decode_payload, encode_frame, ClientMsg, ClientRole, DaemonMsg, Event, FrameReader, Request,
    PROTOCOL_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::manager::Manager;
use crate::outbox::Outbox;
use crate::view::ConnId;
use crate::wait_true;

const READ_BUF: usize = 64 * 1024;
/// Grace period for writers to flush after shutdown before being aborted.
const DRAIN_WAIT: Duration = Duration::from_secs(2);

/// Accept until `stop` turns true, then save all sessions and let every
/// connection flush its queue.
pub async fn serve(listener: UnixListener, mgr: Arc<Manager>, stop: watch::Receiver<bool>) {
    let mut tasks = JoinSet::new();
    let mut stop_rx = stop.clone();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tasks.spawn(handle_conn(stream, mgr.clone(), stop.clone()));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            _ = wait_true(&mut stop_rx) => break,
        }
        while tasks.try_join_next().is_some() {}
    }
    drop(listener);
    mgr.shutdown().await;
    mgr.close_all_conns();
    let drained = tokio::time::timeout(DRAIN_WAIT, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tasks.abort_all();
    }
}

async fn handle_conn(stream: UnixStream, mgr: Arc<Manager>, mut stop: watch::Receiver<bool>) {
    let (mut rd, wr) = stream.into_split();
    let outbox = Outbox::new();
    let writer = tokio::spawn(write_loop(wr, outbox.clone()));
    let mut frames = FrameReader::new();
    let mut buf = vec![0u8; READ_BUF];
    let mut conn: Option<(ConnId, ClientRole)> = None;
    'read: loop {
        let n = tokio::select! {
            r = rd.read(&mut buf) => match r {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    tracing::debug!(error = %e, "read failed");
                    break;
                }
            },
            _ = wait_true(&mut stop) => break,
        };
        frames.push(&buf[..n]);
        loop {
            let payload = match frames.next_frame() {
                Ok(Some(p)) => p,
                Ok(None) => break,
                Err(e) => {
                    outbox.push(error(None, format!("bad frame: {e}")));
                    break 'read;
                }
            };
            let msg: ClientMsg = match decode_payload(&payload) {
                Ok(m) => m,
                Err(e) => {
                    outbox.push(error(None, format!("undecodable message: {e}")));
                    continue;
                }
            };
            match conn {
                Some((_, role)) if !role_allows(role, &msg.req) => {
                    tracing::debug!(?role, "request not allowed for this role; closing");
                    let message = if matches!(msg.req, Request::Hook(_)) {
                        format!("{role:?} connections may not send Hook events")
                    } else {
                        format!("{role:?} connections may only send Hello and Hook")
                    };
                    outbox.push(error(Some(msg.id), message));
                    break 'read;
                }
                Some((id, _)) => handle_request(&mgr, id, &outbox, msg).await,
                None => match msg.req {
                    Request::Hello { role, protocol, .. } if protocol == PROTOCOL_VERSION => {
                        conn = Some((mgr.register_conn(role, outbox.clone()), role));
                        outbox.push(reply(msg.id, hello()));
                    }
                    Request::Hello { .. } => {
                        outbox.push(reply(
                            msg.id,
                            Event::Incompatible {
                                daemon_protocol: PROTOCOL_VERSION,
                            },
                        ));
                        break 'read;
                    }
                    _ => {
                        outbox.push(error(Some(msg.id), "first message must be Hello".into()));
                        break 'read;
                    }
                },
            }
        }
    }
    if let Some((id, _)) = conn {
        mgr.conn_closed(id);
    }
    outbox.close();
    let _ = writer.await;
}

async fn write_loop(mut wr: OwnedWriteHalf, outbox: Arc<Outbox>) {
    let mut bytes = Vec::new();
    while let Some(batch) = outbox.next_batch().await {
        bytes.clear();
        for msg in &batch {
            match encode_frame(msg) {
                Ok(frame) => bytes.extend_from_slice(&frame),
                Err(e) => tracing::warn!(error = %e, "cannot encode message"),
            }
        }
        if let Err(e) = wr.write_all(&bytes).await {
            tracing::debug!(error = %e, "client went away");
            outbox.close();
            break;
        }
    }
    let _ = wr.shutdown().await;
}

/// Hook connections (`berth-hook`) may only report agent events, and only
/// they may: control requests need a GUI / CLI connection, and a GUI / CLI
/// client cannot inject agent events for other sessions. (Not a security
/// boundary: the socket is owner-only and any local client can claim a role;
/// it keeps each client kind to its own job.)
fn role_allows(role: ClientRole, req: &Request) -> bool {
    match req {
        Request::Hello { .. } => true,
        Request::Hook(_) => role == ClientRole::Hook,
        _ => role != ClientRole::Hook,
    }
}

fn hello() -> Event {
    Event::Hello {
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol: PROTOCOL_VERSION,
    }
}

fn reply(id: u32, event: Event) -> DaemonMsg {
    DaemonMsg {
        reply_to: Some(id),
        event,
    }
}

fn error(reply_to: Option<u32>, message: String) -> DaemonMsg {
    DaemonMsg {
        reply_to,
        event: Event::Error { message },
    }
}

/// Dispatch one request. Most requests get exactly one reply (`reply_to` =
/// request id). `Attach` / `Subscribe` are answered by their first
/// `Screen` / `Preview`; `Input` is fire-and-forget (only errors reply).
async fn handle_request(mgr: &Arc<Manager>, conn: ConnId, outbox: &Arc<Outbox>, msg: ClientMsg) {
    let id = msg.id;
    let send = |event: Event| {
        outbox.push(reply(id, event));
    };
    let done = |r: Result<Event, String>| match r {
        Ok(event) => send(event),
        Err(message) => send(Event::Error { message }),
    };
    let ok = |r: Result<(), String>| match r {
        Ok(()) => send(Event::Ok),
        Err(message) => send(Event::Error { message }),
    };
    match msg.req {
        Request::Hello { .. } => send(hello()),
        Request::ListWorkspaces => send(Event::Workspaces(mgr.list_workspaces())),
        Request::CreateWorkspace { name, root } => done(
            mgr.create_workspace(name, root)
                .map(Event::WorkspaceUpdated),
        ),
        Request::RenameWorkspace { id: ws, name } => {
            done(mgr.rename_workspace(ws, name).map(Event::WorkspaceUpdated))
        }
        Request::DeleteWorkspace { id: ws } => ok(mgr.delete_workspace(ws)),
        Request::ListSessions => send(Event::Sessions(mgr.list_sessions())),
        Request::CreateSession {
            workspace,
            cwd,
            command,
            title,
            dims,
        } => done(
            mgr.create_session(workspace, cwd, command, title, dims)
                .await
                .map(Event::SessionUpdated),
        ),
        Request::Attach { session, dims } => {
            if let Err(message) = mgr.attach(conn, outbox.clone(), session, dims, id) {
                send(Event::Error { message });
            }
        }
        Request::Detach { session } => ok(mgr.detach(conn, session)),
        Request::Resize { session, dims } => ok(mgr.resize(conn, session, dims)),
        Request::Input { session, data } => {
            if let Err(message) = mgr.input(session, data) {
                send(Event::Error { message });
            }
        }
        Request::FetchLines {
            session,
            start,
            count,
        } => done(mgr.fetch_lines(session, start, count).await),
        Request::Subscribe { session, mode } => {
            if let Err(message) = mgr.subscribe(conn, outbox.clone(), session, mode, id) {
                send(Event::Error { message });
            }
        }
        Request::Unsubscribe { session } => ok(mgr.unsubscribe(conn, session)),
        Request::Kill { session } => ok(mgr.kill(session)),
        Request::Revive { session, mode } => {
            done(mgr.revive(session, mode).await.map(Event::SessionUpdated))
        }
        Request::Delete { session } => ok(mgr.delete_session(session).await),
        Request::MarkRead { session } => done(mgr.mark_read(session).map(Event::SessionUpdated)),
        Request::Rename { session, title } => done(
            mgr.rename_session(session, title)
                .map(Event::SessionUpdated),
        ),
        Request::MoveSession {
            session,
            workspace,
            order,
        } => done(
            mgr.move_session(session, workspace, order)
                .map(Event::SessionUpdated),
        ),
        Request::SetPersist { session, policy } => {
            done(mgr.set_persist(session, policy).map(Event::SessionUpdated))
        }
        Request::Hook(envelope) => {
            mgr.handle_hook(envelope);
            send(Event::Ok);
        }
        Request::DaemonStatus => send(Event::Status(mgr.status())),
        Request::Shutdown => {
            send(Event::Ok);
            mgr.request_shutdown();
        }
    }
}
