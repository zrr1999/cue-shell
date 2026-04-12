//! Socket client for communicating with the cued daemon.
//!
//! Uses length-prefixed JSON framing over a Unix domain socket, matching
//! the gateway protocol defined in `cue-core::ipc`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use cue_core::Mode;
use cue_core::ipc::{MAX_MESSAGE_SIZE, Message, RequestPayload, encode_message};

/// Client handle for a single connection to the cued daemon.
pub struct CuedClient {
    stream: UnixStream,
    next_id: u32,
}

impl CuedClient {
    /// Connect to the daemon at `socket_path`.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("connect to {}", socket_path.display()))?;
        Ok(Self { stream, next_id: 1 })
    }

    /// Send a request and return the assigned request ID.
    pub async fn send(&mut self, payload: RequestPayload) -> Result<u32> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = Message::Request { id, payload };
        let buf = encode_message(&msg).context("encode request")?;
        self.stream
            .write_all(&buf)
            .await
            .context("write to socket")?;
        Ok(id)
    }

    /// Read the next message from the daemon.
    pub async fn recv(&mut self) -> Result<Message> {
        // Read 4-byte BE length prefix.
        let mut len_buf = [0u8; 4];
        self.stream
            .read_exact(&mut len_buf)
            .await
            .context("read length prefix")?;
        let len = u32::from_be_bytes(len_buf) as usize;

        if len > MAX_MESSAGE_SIZE {
            bail!("message too large: {len} bytes (max {MAX_MESSAGE_SIZE})");
        }

        // Read JSON body.
        let mut body = vec![0u8; len];
        self.stream
            .read_exact(&mut body)
            .await
            .context("read message body")?;

        let msg: Message = serde_json::from_slice(&body).context("deserialize message")?;
        Ok(msg)
    }

    /// Convenience: send an Eval request.
    pub async fn eval(&mut self, input: &str, mode: Mode) -> Result<u32> {
        self.send(RequestPayload::Eval {
            input: input.to_string(),
            mode,
        })
        .await
    }

    /// Convenience: subscribe to event channels.
    pub async fn subscribe(&mut self, channels: &[&str]) -> Result<()> {
        self.send(RequestPayload::Subscribe {
            channels: channels.iter().map(|s| (*s).to_string()).collect(),
        })
        .await?;
        Ok(())
    }

    /// Convenience: send a Ping request.
    pub async fn ping(&mut self) -> Result<u32> {
        self.send(RequestPayload::Ping {}).await
    }

    /// Split the client into read/write halves for concurrent use.
    ///
    /// Returns `(reader, writer_stream)` where the reader can call `recv()`
    /// and the writer keeps the `next_id` counter.
    pub fn into_split(self) -> (ClientReader, ClientWriter) {
        let (read_half, write_half) = self.stream.into_split();
        (
            ClientReader { stream: read_half },
            ClientWriter {
                stream: write_half,
                next_id: self.next_id,
            },
        )
    }
}

/// Read half of a split client connection.
pub struct ClientReader {
    stream: tokio::net::unix::OwnedReadHalf,
}

impl ClientReader {
    /// Read the next message from the daemon.
    pub async fn recv(&mut self) -> Result<Message> {
        let mut len_buf = [0u8; 4];
        self.stream
            .read_exact(&mut len_buf)
            .await
            .context("read length prefix")?;
        let len = u32::from_be_bytes(len_buf) as usize;

        if len > MAX_MESSAGE_SIZE {
            bail!("message too large: {len} bytes (max {MAX_MESSAGE_SIZE})");
        }

        let mut body = vec![0u8; len];
        self.stream
            .read_exact(&mut body)
            .await
            .context("read message body")?;

        let msg: Message = serde_json::from_slice(&body).context("deserialize message")?;
        Ok(msg)
    }
}

/// Write half of a split client connection.
pub struct ClientWriter {
    stream: tokio::net::unix::OwnedWriteHalf,
    next_id: u32,
}

impl ClientWriter {
    /// Send a request and return the assigned request ID.
    pub async fn send(&mut self, payload: RequestPayload) -> Result<u32> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = Message::Request { id, payload };
        let buf = encode_message(&msg).context("encode request")?;
        self.stream
            .write_all(&buf)
            .await
            .context("write to socket")?;
        Ok(id)
    }
}

// ── Writer handle (channel-based command pattern) ──

/// A cloneable handle for sending requests to the daemon.
///
/// Internally holds an [`mpsc::Sender`] that feeds a dedicated writer task
/// which owns the actual [`ClientWriter`].
#[derive(Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<RequestPayload>,
}

impl WriterHandle {
    /// Enqueue a request payload to be sent to the daemon (non-blocking).
    ///
    /// Returns `Ok(())` if the message was enqueued, or `Err` if the
    /// writer task has exited or the channel is full.
    pub fn try_send(
        &self,
        payload: RequestPayload,
    ) -> Result<(), mpsc::error::TrySendError<RequestPayload>> {
        self.tx.try_send(payload)
    }
}

/// Spawn a dedicated writer task that owns the [`ClientWriter`] and receives
/// messages from a bounded channel.  Returns a [`WriterHandle`] for sending
/// requests.
///
/// The task exits when all [`WriterHandle`] clones are dropped.
pub fn spawn_writer_task(mut writer: ClientWriter) -> WriterHandle {
    let (tx, mut rx) = mpsc::channel::<RequestPayload>(64);
    tokio::spawn(async move {
        while let Some(payload) = rx.recv().await {
            if let Err(e) = writer.send(payload).await {
                tracing::error!("writer task send error: {e}");
                break;
            }
        }
        tracing::debug!("writer task exiting");
    });
    WriterHandle { tx }
}

// ── Socket path resolution (mirrors cued::dirs) ──

const APP_DIR: &str = "cue-shell";

/// Resolve the default socket path: `$XDG_RUNTIME_DIR/cue-shell/cued.sock`.
pub fn default_socket_path() -> PathBuf {
    let runtime_dir = if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join(APP_DIR)
    } else {
        std::env::temp_dir().join(APP_DIR)
    };
    runtime_dir.join("cued.sock")
}
