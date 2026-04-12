//! Gateway actor — Unix socket listener, per-client handlers, message framing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use cue_core::ipc::{
    EventPayload, MAX_MESSAGE_SIZE, Message, OkPayload, RequestPayload, ResponsePayload,
    encode_message, error_code,
};

use crate::parser::{Parser, Resolver};

use super::{ActorSystem, CLIENT_EVENT_CAP, EventBusMsg, GatewayMsg, SchedulerMsg};

/// Next client id counter (global, atomic).
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

// ── Message framing ──

/// Read one length-prefixed JSON message from the stream.
pub async fn read_message(stream: &mut UnixStream) -> Result<Message> {
    let len = stream.read_u32().await.context("read length prefix")?;
    if len as usize > MAX_MESSAGE_SIZE {
        bail!("message too large: {len} bytes");
    }
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await.context("read body")?;
    let msg: Message = serde_json::from_slice(&buf).context("deserialize message")?;
    Ok(msg)
}

/// Write one length-prefixed JSON message to the stream.
pub async fn write_message(stream: &mut UnixStream, msg: &Message) -> Result<()> {
    let encoded = encode_message(msg)?;
    stream.write_all(&encoded).await.context("write message")?;
    stream.flush().await.context("flush")?;
    Ok(())
}

/// Type alias for the shared client map to avoid clippy::type_complexity.
type ClientMap = Arc<tokio::sync::Mutex<HashMap<u64, mpsc::Sender<(u32, ResponsePayload)>>>>;

/// Spawn the Gateway actor.
///
/// This creates a Unix socket listener and spawns a task that accepts connections.
/// Per-client handler tasks are spawned for each connection.
pub async fn spawn(
    mut rx: mpsc::Receiver<GatewayMsg>,
    socket_path: PathBuf,
    sys: ActorSystem,
) -> Result<()> {
    // Remove stale socket file.
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("remove stale socket {}", socket_path.display()))?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind socket {}", socket_path.display()))?;

    info!(path = %socket_path.display(), "gateway: listening");

    // Shared state: client_id → response sender.
    let clients: ClientMap = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    let clients_for_dispatch = Arc::clone(&clients);

    // Accept loop — runs in its own task.
    let sys_accept = sys.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
                    info!(%client_id, "gateway: client connected");
                    let sys_clone = sys_accept.clone();
                    let clients_clone = Arc::clone(&clients_for_dispatch);
                    tokio::spawn(handle_client(client_id, stream, sys_clone, clients_clone));
                }
                Err(e) => {
                    error!("gateway: accept error: {e}");
                }
            }
        }
    });

    // Dispatch loop — routes responses/events back to clients.
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                GatewayMsg::SendResponse {
                    client_id,
                    request_id,
                    payload,
                } => {
                    let guard = clients.lock().await;
                    if let Some(sender) = guard.get(&client_id) {
                        if sender.try_send((request_id, payload)).is_err() {
                            warn!(%client_id, "gateway: response channel full/closed");
                        }
                    } else {
                        warn!(%client_id, "gateway: no such client for response");
                    }
                }

                GatewayMsg::PushEvent { payload, channel } => {
                    // Delegate to event bus.
                    let _ = sys
                        .event_bus
                        .send(EventBusMsg::Publish { payload, channel })
                        .await;
                }

                GatewayMsg::Shutdown => {
                    info!("gateway: shutdown signal received");
                    // We don't break here — the accept loop is in another task.
                    // The main shutdown sequence will drop all senders, which
                    // closes channels and causes tasks to exit.
                    break;
                }
            }
        }

        debug!("gateway: dispatch loop stopped");
    });

    Ok(())
}

/// Handle one client connection.
async fn handle_client(
    client_id: u64,
    mut stream: UnixStream,
    sys: ActorSystem,
    clients: ClientMap,
) {
    // Per-client response channel.
    let (resp_tx, mut resp_rx) = mpsc::channel::<(u32, ResponsePayload)>(64);
    // Per-client event channel.
    let (evt_tx, mut evt_rx) = mpsc::channel::<EventPayload>(CLIENT_EVENT_CAP);

    // Register.
    {
        let mut guard = clients.lock().await;
        guard.insert(client_id, resp_tx);
    }

    // Auto-subscribe to default channels.
    for channel in ["jobs", "output:*"] {
        let _ = sys
            .event_bus
            .send(EventBusMsg::Subscribe {
                client_id,
                channel: channel.to_string(),
                sender: evt_tx.clone(),
            })
            .await;
    }

    // Split stream.
    let (read_half, write_half) = stream.split();
    let read_half = tokio::io::BufReader::new(read_half);
    let write_half = tokio::io::BufWriter::new(write_half);

    // We need owned halves for separate tasks — use a shared approach with a single loop.
    drop(read_half);
    drop(write_half);

    // Use a single-task select loop for simplicity.
    loop {
        tokio::select! {
            // Read a message from the client.
            msg_result = read_message(&mut stream) => {
                match msg_result {
                    Ok(Message::Request { id, payload }) => {
                        if let Err(e) = route_request(
                            client_id, id, payload, &sys, &evt_tx,
                        ).await {
                            warn!(%client_id, "gateway: route error: {e}");
                            let err_resp = Message::Response {
                                id,
                                payload: ResponsePayload::err(error_code::INTERNAL, e.to_string()),
                            };
                            if write_message(&mut stream, &err_resp).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(_) => {
                        // Clients should only send Request messages.
                        warn!(%client_id, "gateway: unexpected non-request message");
                    }
                    Err(e) => {
                        debug!(%client_id, "gateway: read error (disconnect?): {e}");
                        break;
                    }
                }
            }

            // Deliver response back to client.
            Some((request_id, payload)) = resp_rx.recv() => {
                let msg = Message::Response { id: request_id, payload };
                if write_message(&mut stream, &msg).await.is_err() {
                    break;
                }
            }

            // Deliver pushed event to client.
            Some(event) = evt_rx.recv() => {
                let msg = Message::Event { payload: event };
                if write_message(&mut stream, &msg).await.is_err() {
                    break;
                }
            }
        }
    }

    // Cleanup.
    info!(%client_id, "gateway: client disconnected");
    {
        let mut guard = clients.lock().await;
        guard.remove(&client_id);
    }
    let _ = sys
        .event_bus
        .send(EventBusMsg::UnsubscribeAll { client_id })
        .await;
}

/// Route an incoming request to the appropriate actor.
async fn route_request(
    client_id: u64,
    request_id: u32,
    payload: RequestPayload,
    sys: &ActorSystem,
    evt_tx: &mpsc::Sender<EventPayload>,
) -> Result<()> {
    match payload {
        RequestPayload::Eval { input, mode } => {
            // Parse → resolve → send to scheduler.
            match Parser::parse(&input) {
                Ok(ast) => match Resolver::resolve(ast, mode) {
                    Ok(command) => {
                        sys.scheduler
                            .send(SchedulerMsg::Eval {
                                client_id,
                                request_id,
                                command,
                            })
                            .await
                            .context("send to scheduler")?;
                    }
                    Err(e) => {
                        // Parse/resolve error → respond immediately.
                        sys.gateway
                            .send(GatewayMsg::SendResponse {
                                client_id,
                                request_id,
                                payload: ResponsePayload::err(
                                    error_code::INVALID_SYNTAX,
                                    e.to_string(),
                                ),
                            })
                            .await
                            .context("send error response")?;
                    }
                },
                Err(e) => {
                    sys.gateway
                        .send(GatewayMsg::SendResponse {
                            client_id,
                            request_id,
                            payload: ResponsePayload::err(
                                error_code::INVALID_SYNTAX,
                                e.to_string(),
                            ),
                        })
                        .await
                        .context("send error response")?;
                }
            }
        }

        RequestPayload::Subscribe { channels } => {
            for ch in channels {
                sys.event_bus
                    .send(EventBusMsg::Subscribe {
                        client_id,
                        channel: ch,
                        sender: evt_tx.clone(),
                    })
                    .await?;
            }
            sys.gateway
                .send(GatewayMsg::SendResponse {
                    client_id,
                    request_id,
                    payload: ResponsePayload::ack(),
                })
                .await?;
        }

        RequestPayload::Unsubscribe { channels } => {
            for ch in channels {
                sys.event_bus
                    .send(EventBusMsg::Unsubscribe {
                        client_id,
                        channel: ch,
                    })
                    .await?;
            }
            sys.gateway
                .send(GatewayMsg::SendResponse {
                    client_id,
                    request_id,
                    payload: ResponsePayload::ack(),
                })
                .await?;
        }

        RequestPayload::Ping {} => {
            sys.gateway
                .send(GatewayMsg::SendResponse {
                    client_id,
                    request_id,
                    payload: ResponsePayload::Ok(OkPayload::Pong {}),
                })
                .await?;
        }

        RequestPayload::Shutdown {} => {
            info!("gateway: shutdown request from client {client_id}");
            sys.gateway
                .send(GatewayMsg::SendResponse {
                    client_id,
                    request_id,
                    payload: ResponsePayload::ack(),
                })
                .await?;
            // Trigger full shutdown.
            sys.gateway.send(GatewayMsg::Shutdown).await?;
        }

        // Stubs for unimplemented request types.
        _ => {
            sys.gateway
                .send(GatewayMsg::SendResponse {
                    client_id,
                    request_id,
                    payload: ResponsePayload::err(
                        error_code::NOT_SUPPORTED,
                        "request type not yet implemented",
                    ),
                })
                .await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn message_framing_roundtrip() {
        // Create a connected pair.
        let (mut client, mut server) = UnixStream::pair().unwrap();

        let msg = Message::Request {
            id: 42,
            payload: RequestPayload::Ping {},
        };

        write_message(&mut client, &msg).await.unwrap();
        let decoded = read_message(&mut server).await.unwrap();

        if let Message::Request {
            id,
            payload: RequestPayload::Ping {},
        } = decoded
        {
            assert_eq!(id, 42);
        } else {
            panic!("wrong message variant");
        }
    }

    #[tokio::test]
    async fn response_roundtrip() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let msg = Message::Response {
            id: 1,
            payload: ResponsePayload::Ok(OkPayload::Pong {}),
        };
        write_message(&mut a, &msg).await.unwrap();
        let decoded = read_message(&mut b).await.unwrap();
        assert!(matches!(
            decoded,
            Message::Response {
                id: 1,
                payload: ResponsePayload::Ok(OkPayload::Pong {}),
            }
        ));
    }
}
