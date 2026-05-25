# Transport Modes Design — cue-shell

> **Status**: Draft / Pre-implementation
> **Scope**: HTTP transport, SSH transport completion, chain/relay mode
> **Audience**: Contributors implementing or reviewing these features

---

## Background and Existing Architecture

### Wire Protocol

All cue-shell IPC uses **length-prefixed JSON framing** defined in `cue-core/src/ipc.rs`:

```
[ 4 bytes: u32 big-endian body length ][ UTF-8 JSON body ]
```

The `Message` envelope is tagged JSON with three variants:

| Variant    | Direction       | Purpose                                    |
|------------|-----------------|--------------------------------------------|
| `Request`  | Client → cued   | User commands, pings, subscriptions        |
| `Response` | cued → Client   | Correlated replies (matched by `id: u32`)  |
| `Event`    | cued → Client   | Server-pushed async notifications          |

Max message size: 16 MiB (`MAX_MESSAGE_SIZE`).

### Transport Abstraction

The key insight is that the entire client stack is already transport-agnostic:

```rust
// cue-client/src/client.rs
pub trait ClientStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T> ClientStream for T where T: AsyncRead + AsyncWrite + Send + Unpin {}
```

`CuedClient::from_stream(stream)` accepts any `S: ClientStream + 'static`. The
reconnect manager (`cue-client/src/reconnect.rs`) is parameterized by a
`ClientConnector` closure that returns `CuedClient` — it does not care what
transport produced the stream.

```rust
pub struct ClientConnector {
    connect: Arc<dyn Fn() -> ConnectFuture + Send + Sync>,
}
```

**Any new transport only needs to produce an `AsyncRead + AsyncWrite` stream.**
The framing, reconnect, and event routing layers reuse verbatim.

### Current State

| Transport | Server side          | Client side                           | Reconnect |
|-----------|----------------------|---------------------------------------|-----------|
| Unix      | `actor/gateway.rs`   | `CuedClient::connect(socket_path)`    | ✅ full   |
| SSH stdio | `gateway_stdio.rs`   | `SshChildStream` in `cue-cli/main.rs` | ⚠️ partial |
| HTTP      | —                    | —                                     | ❌ none   |
| Chain     | —                    | —                                     | ❌ none   |

---

## Mode 1: HTTP Transport

### 1.1 Current State

No HTTP transport exists. This would allow clients to connect to `cued` over a
network port instead of a Unix socket — useful for remote development, container
environments, IDE plugins, and web UIs.

### 1.2 Alternative Approaches

#### Option A: REST polling + SSE for events

HTTP `POST /request` for commands; HTTP `GET /events` (Server-Sent Events) for
pushed events.

**Pros**:
- Human-readable, debuggable with `curl`
- SSE is firewall-friendly (unidirectional, plain HTTP/1.1)
- Easy to implement with `axum`

**Cons**:
- Inherently two connections per client (request channel + event channel)
- Request/response correlation is handled by the HTTP request/response cycle —
  but SSE events are not correlated with individual requests, requiring a
  session-ID header or cookie
- Polling adds latency for any operation that needs a quick response
- `FgInput`/`FgOutput` (PTY data) requires binary framing or base64 over SSE;
  SSE is text-only
- **Breaks the `ClientStream` model** — would need a completely new client
  abstraction layer

#### Option B: HTTP/2 with server push

Use `h2` or `hyper` + HTTP/2 server push frames for daemon-to-client messages.

**Pros**:
- Multiplexed streams over a single TCP connection
- Server push is standardized

**Cons**:
- HTTP/2 server push was [removed from browsers](https://crbug.com/1201674) and
  is deprecated for general use
- `h2` crate is low-level; setting up a push-based server is non-trivial
- Still requires TLS in most contexts
- Does not map naturally to the existing `AsyncRead + AsyncWrite` stream model

#### Option C: HTTP/3 (QUIC)

Use `quinn` crate to speak QUIC natively.

**Pros**:
- Multiplexed, 0-RTT reconnect, no head-of-line blocking
- Built-in TLS 1.3

**Cons**:
- QUIC is UDP-based; many corporate firewalls block UDP port 443
- `quinn` API is more complex than WebSocket
- Overkill for local-network or loopback use
- No existing `quinn` dependency in the workspace

#### Option D: WebSocket (tokio-tungstenite / axum) — **RECOMMENDED**

Upgrade an HTTP/1.1 connection to WebSocket. Each WebSocket message carries
exactly one length-prefixed JSON IPC message (i.e., the same 4-byte header +
JSON body that Unix clients send). The `SinkExt`/`StreamExt` WebSocket API is
wrapped in a thin `AsyncRead + AsyncWrite` adapter, giving back a `ClientStream`
with **zero changes to the upper layers**.

```
Client                         cued (HTTP gateway)
  │── HTTP GET /ws (Upgrade) ──►│
  │◄── 101 Switching Protocols ─│
  │                              │
  │── WS binary frame ──────────►│  (4-byte prefix + JSON)
  │◄── WS binary frame ──────────│  (4-byte prefix + JSON)
```

**Pros**:
- Full duplex, single connection, works through load balancers
- Binary frames preserve the existing framing intact — `read_message` /
  `encode_message` in `cue-core` need no changes
- `axum` is the natural fit: `axum::extract::ws::WebSocket` is already
  `AsyncRead + AsyncWrite`-compatible via a small adapter
- `tokio-tungstenite` provides a standalone client without requiring axum
- Firewall-friendly (standard port 80/443, HTTP upgrade handshake)
- Auth: `Authorization: Bearer <token>` header on the upgrade request
- TLS: standard `tokio-rustls` / `axum-server` with TLS acceptor

**Cons**:
- Slightly more setup than raw TCP (HTTP upgrade round-trip)
- WebSocket framing overhead (~2–14 bytes per frame) — negligible vs. JSON body

**Implementation sketch**:

```rust
// Server side — add to cued alongside gateway.rs
// crates/cue-daemon/src/actor/gateway_ws.rs

use axum::{Router, extract::ws::{WebSocket, WebSocketUpgrade}, routing::get};
use tokio::io::duplex;

pub fn router(sys: ActorSystem) -> Router {
    Router::new()
        .route("/ws", get(move |ws: WebSocketUpgrade| {
            let sys = sys.clone();
            async move { ws.on_upgrade(|socket| handle_ws_client(socket, sys)) }
        }))
}

async fn handle_ws_client(ws: WebSocket, sys: ActorSystem) {
    // Wrap ws into AsyncRead + AsyncWrite, then call the existing
    // handle_client logic (currently UnixStream-specific, to be refactored).
    let stream = ws_to_stream(ws);
    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    handle_client_generic(client_id, stream, sys, ...).await;
}
```

The `handle_client` function in `actor/gateway.rs` currently takes a
`UnixStream`. Refactoring it to accept `impl AsyncRead + AsyncWrite + Unpin`
(or the boxed `BoxedClientStream`) enables reuse for both Unix and WebSocket
clients.

```rust
// Client side — cue-client/src/reconnect.rs
impl ClientConnector {
    pub fn websocket(url: String, token: Option<String>) -> Self {
        Self::new(move || {
            let url = url.clone();
            let token = token.clone();
            async move {
                let (ws, _) = tokio_tungstenite::connect_async(
                    tungstenite::client::IntoClientRequest::into_client_request(url)?
                ).await?;
                // wrap ws in WsStream adapter (AsyncRead + AsyncWrite)
                Ok(CuedClient::from_stream(WsStream::new(ws)))
            }
        })
    }
}
```

### 1.3 Recommended Approach

**WebSocket over HTTP/1.1 using `axum`** (Option D), reusing the existing
length-prefixed JSON framing over binary WebSocket frames.

Rationale:
- Zero protocol changes — same wire format as Unix socket
- Single `ClientStream` adapter bridges the two transports at the leaf level
- `axum` is the right-fit HTTP framework for Tokio (tower middleware for auth,
  TLS via `axum-server`)
- `tokio-tungstenite` for the client side; `axum` for the server side

### 1.4 TOML Configuration Schema

```toml
# ~/.config/cue-shell/client.toml

[transport]
default_profile = "remote-http"

[transport.profiles.remote-http]
transport = "http"
url = "ws://192.168.1.10:8765/ws"   # or wss:// for TLS
# token = "secret-bearer-token"     # optional; sent as Authorization: Bearer
# tls_ca_cert = "/path/to/ca.pem"  # optional; for self-signed certs
```

Daemon side (`cued` config):

```toml
# ~/.config/cue-shell/server.toml

[http_gateway]
enabled = true
bind = "0.0.0.0:8765"
# tls_cert = "/path/to/cert.pem"
# tls_key  = "/path/to/key.pem"
# auth_token = "secret-bearer-token"   # if set, all WS upgrade requests must
#                                       # include Authorization: Bearer <token>
```

### 1.5 New Rust Dependencies

Add to `Cargo.toml` (workspace):

```toml
axum            = { version = "0.8", features = ["ws"] }        # server WS
tokio-tungstenite = { version = "0.24", features = ["native-tls"] } # client WS
```

TLS (optional, behind a feature flag):

```toml
axum-server     = { version = "0.7", features = ["tls-rustls"] }
tokio-rustls    = "0.26"
```

### 1.6 Implementation Steps

1. **Refactor `handle_client`** in `actor/gateway.rs` to be generic over
   `AsyncRead + AsyncWrite + Unpin` (extract stream type, keep all routing
   logic). This removes the hard `UnixStream` dependency.

2. **Add `gateway_ws.rs`** crate in `cued`: spawn a `tokio::net::TcpListener`,
   hand connections to `axum` (or raw `hyper` upgrade). On upgrade, wrap the
   WebSocket in a `WsStream` adapter and call the generic `handle_client`.

3. **Add `WsStream` adapter** in `cue-client` (or a shared location): wraps
   `tokio_tungstenite::WebSocketStream` and implements `AsyncRead + AsyncWrite`
   by translating binary frames to/from a byte buffer.

4. **Add `ClientConnector::websocket`** constructor in `cue-client/src/reconnect.rs`.

5. **Extend `TransportProfile`** enum in `cue-cli/src/config.rs` with
   `Http(HttpProfile)` variant. Extend `connector_for_profile` and
   `target_config.rs` to build the WebSocket connector.

6. **Add auth middleware** using `axum` tower layer: extract
   `Authorization: Bearer <token>` from the upgrade request headers.

7. **Wire up in `cued start`**: if `[http_gateway] enabled = true`, spawn the
   HTTP listener task alongside the Unix listener.

---

## Mode 2: SSH Transport (Completion)

### 2.1 Current State

The SSH transport is **more complete than it might appear**:

- **`cue-cli/src/main.rs`** (`connect_ssh_transport`, `spawn_ssh_transport`):
  fully implements subprocess SSH with `ssh <destination> <gateway_command>`,
  wraps the child's stdin/stdout as `SshChildStream: AsyncRead + AsyncWrite`,
  performs a `Ping/Pong` handshake to verify the tunnel, and constructs a
  `CuedClient` from the stream. The `ClientConnector` is also set up to
  re-invoke `connect_ssh_transport` on reconnect.

- **The real gap** is in `cue-tui/src/target_config.rs::connector_for_profile`:

```rust
// crates/cue-tui/src/target_config.rs:122
"ssh" => anyhow::bail!(
    "live reconnect for ssh transport is not yet supported; \
     restart cue-tui to apply the profile change"
),
```

This function is called when the user switches the active target profile *inside
a running cue-tui session*. It cannot reconstruct an SSH connector because it
only has access to the TOML value (no access to `SshProfile` or the connector
factory in `cue-cli`).

- **Auto-start**: the SSH transport has no mechanism to run `start_command` on
  the remote when the gateway connection fails (unlike Unix where
  `ensure_daemon_running` auto-starts `cued`). This is a UX gap but not a
  correctness problem.

### 2.2 Alternative Approaches for the SSH Client Layer

#### Option A: Subprocess `ssh` (current approach) — **RECOMMENDED**

Spawn `ssh <destination> "cued gateway --stdio"` as a Tokio child process, wrap
stdin/stdout as `AsyncRead + AsyncWrite`.

**Pros**:
- Already implemented and working in `cue-cli`
- Inherits all OpenSSH features: agent forwarding (`-A`), jump hosts (`-J`),
  `~/.ssh/config`, ControlMaster multiplexing, known_hosts verification
- Zero Rust SSH implementation to maintain
- `kill_on_drop(true)` on the child ensures cleanup

**Cons**:
- Requires `ssh` binary on PATH (not an issue for developer machines)
- Subprocess adds ~50–100 ms handshake latency on first connect
- Reconnect spawns a new process (acceptable — same overhead as manual reconnect)

#### Option B: `openssh` crate

The [`openssh`](https://crates.io/crates/openssh) crate wraps the `ssh` binary
more ergonomically (typed API, ControlMaster session reuse, remote command
execution).

**Pros**:
- `openssh::Session::connect()` returns a reusable session handle
- `session.raw_command("cued gateway --stdio")` gives stdin/stdout streams
- ControlMaster means the second connect uses the existing TCP connection →
  near-zero reconnect latency
- `session.check()` lets the reconnect loop detect a dead connection

**Cons**:
- New dependency (`openssh = "0.10"`)
- Still shells out to `ssh`; no benefit for the basic use case
- ControlMaster path configuration can conflict with the user's own settings

#### Option C: `russh` crate (pure Rust SSH)

[`russh`](https://crates.io/crates/russh) is a pure-Rust async SSH
implementation (client + server).

**Pros**:
- No dependency on `ssh` binary
- Full control over reconnect, known_hosts, and key negotiation
- Could enable SSH server mode inside `cued` itself (accept incoming SSH and
  route to Unix socket), removing the need for `cued gateway --stdio`

**Cons**:
- Substantial new dependency (`russh`, `russh-keys`) with complex async API
- Known_hosts and host key verification must be implemented manually
- Agent forwarding via `russh` requires additional work
- Maintenance burden — crypto library upgrades, CVE tracking
- Overkill: the subprocess approach already works well

#### Option D: `ssh2` crate (libssh2 bindings)

[`ssh2`](https://crates.io/crates/ssh2) wraps `libssh2` (C library).

**Pros**:
- Mature, widely used
- Good known_hosts and key agent support

**Cons**:
- Blocking / callback API — awkward with Tokio (needs `spawn_blocking`)
- Requires `libssh2` C library at link time (complicates static builds)
- Worse than subprocess in every practical dimension for this use case

### 2.3 Recommended Approach

**Keep the subprocess `ssh` approach** (Option A) and fix the two gaps:

1. **Fix `connector_for_profile` for SSH** so that live target-switching in
   `cue-tui` works without restarting.
2. **Add remote auto-start** as a best-effort step before the first connection.

The fix for gap #1 is architectural: `connector_for_profile` lives in
`cue-tui` and only has a TOML value. It needs the same connector factory logic
that exists in `cue-cli`. The simplest fix is to move the SSH connector factory
into `cue-client` (or a new `cue-client/src/ssh.rs`) so both `cue-cli` and
`cue-tui` can call it.

### 2.4 TOML Configuration Schema

No changes needed. The existing schema already captures everything:

```toml
[transport.profiles.devbox]
transport = "ssh"
destination = "devbox"                       # ssh(1) destination
gateway_command = "cued gateway --stdio"     # executed on remote
start_command = "cued start"                 # run once if daemon not found
# Extra SSH options (passed via -o or prepended to command):
# ssh_options = ["-A", "-o", "ServerAliveInterval=10"]
```

Proposed new optional field:

```toml
[transport.profiles.devbox]
# ...existing fields...
auto_start = true       # attempt start_command on first ping failure (default: true)
ssh_options = ["-A"]    # additional ssh(1) flags prepended to the invocation
```

### 2.5 Implementation Steps

1. **Extract SSH connector factory** into `cue-client/src/ssh.rs`:

```rust
// crates/cue-client/src/ssh.rs

pub struct SshConnectorConfig {
    pub destination: String,
    pub gateway_command: String,
    pub start_command: String,
    pub ssh_options: Vec<String>,
    pub auto_start: bool,
}

impl ClientConnector {
    pub fn ssh(config: SshConnectorConfig) -> Self {
        Self::new(move || {
            let config = config.clone();
            async move {
                // 1. spawn ssh process
                // 2. wrap stdin/stdout as SshChildStream (AsyncRead + AsyncWrite)
                // 3. send Ping, if timeout → try start_command, retry
                // 4. return CuedClient::from_stream(stream)
            }
        })
    }
}
```

Move `SshChildStream`, `spawn_ssh_transport`, and `connect_ssh_transport` from
`cue-cli/src/main.rs` into this module. `cue-cli` calls `ClientConnector::ssh`.

2. **Fix `connector_for_profile`** in `cue-tui/src/target_config.rs`:

```rust
"ssh" => {
    let config = SshConnectorConfig {
        destination: table.get("destination")...
        gateway_command: ...
        start_command: ...
        ssh_options: ...
        auto_start: ...
    };
    Ok(cue_client::ClientConnector::ssh(config))
}
```

3. **Add `ssh_options` to `SshProfile`** in `cue-cli/src/config.rs`.

4. **Add auto-start logic**: before the first `Ping`, try
   `ssh <destination> <start_command>` once and retry the gateway connection.
   Wrap in a timeout (e.g., 10 s) to avoid blocking reconnect loops.

5. **Add `ServerAliveInterval` default**: append `-o ServerAliveInterval=10 -o
   ServerAliveCountMax=3` to the SSH invocation so dead connections are detected
   without waiting for TCP timeout.

6. **Add SSH live-reconnect integration test** in `cue-client` using in-process
   duplex streams (no real `ssh`).

---

## Mode 3: Chain Transport / Proxy Mode

### 3.1 Current State

No chain/relay mode exists. This mode would allow a local `cued` to act as a
transparent proxy to a remote `cued`, enabling topologies like:

```
cue-tui ──Unix──► local-cued ──SSH──► remote-cued
```

Use cases:
- Audit logging / session recording at the local hop
- Credential isolation (the client never touches remote credentials; local cued
  holds the SSH key)
- Multi-hop: `laptop → vpn-gateway-cued → target-cued`
- Offline resilience: local cued can queue commands while remote is unreachable

### 3.2 Alternative Approaches

#### Option A: Transparent proxy (all-forward)

Local `cued` has an "upstream" configuration. Every incoming `Request` from any
local client is forwarded verbatim to the upstream `cued`, and every `Response`
/ `Event` from upstream is fanned out to subscribed local clients.

```
local client A ──► local-cued ─────► remote-cued
local client B ──► local-cued ─╯
```

Local cued maintains one persistent connection to upstream, shares responses and
events across all local clients.

**Pros**:
- Simplest model — local cued is a dumb multiplexer
- No logic duplication, no state divergence between hops
- Clients get exactly the same semantics as a direct connection

**Cons**:
- Local cued cannot run any commands locally (no local job execution)
- Client IDs from local clients must be remapped to avoid collisions on the
  upstream (`client_id` is an opaque u64 on the wire)
- If upstream disconnects, all local clients must be notified

**Mapping concern**: the current IPC protocol carries `client_id` only in
internal actor messages (it is not in the wire `Message` envelope). The wire
protocol carries only `id: u32` (request correlation). So transparent proxying
at the wire level is straightforward: forward the raw bytes.

```
local client ──[raw bytes]──► proxy ──[raw bytes]──► remote-cued
              ◄─────────────────────◄─────────────────────
```

Since `gateway_stdio.rs` already implements exactly this (`io::copy` in both
directions), a Unix-to-Unix proxy just needs to:
1. Accept a Unix connection from the local client
2. Open its own connection to the upstream (Unix or SSH)
3. `io::copy` in both directions

#### Option B: Selective proxy (local + remote commands)

Some commands run on local `cued` (`:ls`, `:scope`, local jobs), others are
forwarded to remote `cued` based on routing rules.

**Pros**:
- Local commands remain fast even when remote is slow/disconnected
- Can mix local and remote jobs in the same TUI session

**Cons**:
- Requires the proxy to parse and route individual IPC messages — breaks the
  simple byte-relay model
- Client would need to distinguish local vs. remote job IDs
- High implementation complexity; scope creep

#### Option C: Mesh topology (multiple cued with shared state)

Multiple `cued` instances synchronize job state over a gossip or consensus
protocol.

**Pros**:
- High availability

**Cons**:
- Requires distributed systems primitives (consensus, conflict resolution)
- Completely out of scope for the current single-daemon model
- No plausible use case that justifies the complexity

### 3.3 Recommended Approach

**Option A — transparent proxy** using raw byte relay, with upstream configured
as either a Unix socket or an SSH transport. Start with a cue-tui-level relay
that does not require any changes to cued's internal actor system.

The simplest useful design requires only:
1. A new `"chain"` transport profile in `client.toml`
2. A `cued relay` subcommand (analogous to `cued gateway --stdio`) that accepts
   a Unix connection and relays it to an upstream socket (Unix or SSH)
3. Or, more powerfully: configure `cued` itself to forward all client
   connections to an upstream

The byte-relay approach re-uses `gateway_stdio::relay` nearly verbatim.

### 3.4 TOML Configuration Schema

**Client-side**: the chain is invisible — the client just configures its
transport to point at the intermediate `cued`. No new client config needed.

**Daemon-side** — `cued` in relay mode:

```toml
# ~/.config/cue-shell/server.toml  (on the relay node)

[relay]
enabled = true

# upstream is another cued; described as any standard transport profile
[relay.upstream]
transport = "ssh"
destination = "prod-server"
gateway_command = "cued gateway --stdio"
start_command = "cued start"
```

Or simpler: a one-shot relay via CLI:

```sh
# On the relay machine: accept unix connections and forward them over SSH
cued relay --upstream ssh://prod-server
```

### 3.5 Implementation Steps

1. **Add `cued relay` subcommand** in `crates/cue-cli/src/bin/cued.rs` (analogous to
   `cued gateway --stdio`):
   - Accept a Unix socket connection
   - Connect to upstream (Unix or SSH using the SSH connector from Mode 2)
   - Run `gateway_stdio::relay(client_unix, stdin, upstream_stream)` — the
     three-way relay function already works for any `AsyncRead + AsyncWrite`

2. **Extend `cued gateway`** to support an `--upstream` flag:

```sh
cued gateway --stdio --upstream ssh://devbox
```

This re-uses the existing `gateway_stdio.rs::relay` function:

```rust
pub async fn run_relay(stdin: impl AsyncRead, stdout: impl AsyncWrite, upstream: impl AsyncRead + AsyncWrite) -> Result<()> {
    relay(stdin, stdout, upstream).await
}
```

3. **Multi-client multiplexing** (optional, Phase 2): instead of one relay
   per client connection, have the relay actor maintain a single upstream
   connection and multiplex multiple local clients. This requires the proxy to
   parse and route individual `Message` envelopes (breaking pure byte relay),
   but the same `read_message`/`write_message` functions from `cue-core` can be
   reused.

4. **Reconnect**: the relay should reconnect to the upstream using the same
   `spawn_connection_manager` infrastructure from `cue-client`. When upstream
   reconnects, notify all active local clients with a `DaemonReady` event.

---

## Cross-Cutting Concerns

### Authentication

| Transport | Mechanism                                      | Notes                                      |
|-----------|------------------------------------------------|--------------------------------------------|
| Unix      | Filesystem permissions on socket (`0600`)      | Implicit — same user only                  |
| SSH       | SSH authentication (key / agent / password)    | Delegated to OpenSSH                       |
| HTTP/WS   | `Authorization: Bearer <token>` on WS upgrade  | Token in daemon config; TLS strongly advised |
| Chain     | Auth at each hop                               | SSH on the relay-to-upstream leg           |

For HTTP, the token should be a random 256-bit hex string. The daemon stores
accepted token material in `~/.config/cue-shell/server.toml` with `chmod 600`;
clients reference the matching token from their `client.toml` transport profile.

### TLS

- **Unix**: N/A (kernel-level isolation)
- **SSH**: TLS not needed (SSH provides encryption)
- **HTTP/WS**: optional for LAN; required for WAN. Use `rustls` (no C
  dependencies). `axum-server` with a `RustlsConfig` acceptor.
- TLS is a feature flag: `cued` starts in plaintext mode unless a cert/key are
  configured. Clients use `ws://` vs `wss://` accordingly.

### Connection Health

All transports should implement keepalives to detect dead connections quickly:

| Transport | Mechanism                                           |
|-----------|-----------------------------------------------------|
| Unix      | OS detects dead socket; read returns `BrokenPipe`   |
| SSH       | `ServerAliveInterval=10` in SSH flags               |
| HTTP/WS   | WebSocket ping/pong frames (via `axum` / tungstenite) every 30 s |
| Chain     | Inherited from the upstream transport               |

The reconnect manager (`run_connection_manager_with_delay`) already handles
disconnect detection and retry. Adding keepalive timers only reduces the *time
to detect* a dead connection.

### Error Handling

- **Transport errors** (connection refused, timeout): propagated as
  `ConnectionEvent::Disconnected` → reconnect loop.
- **Auth errors** (HTTP 401, SSH auth failure): should surface as a fatal error
  with a clear message, not silently retried.
- **Protocol mismatch**: the `Ping/Pong` handshake after every new connection
  (already used by SSH transport) should be the standard health check for all
  transports.

---

## Priority Order

### Priority 1: SSH Transport Completion

**Why first**: It is 80% done and the missing 20% (`connector_for_profile` for
SSH in `cue-tui`) blocks a real, already-configured user-facing feature. The
change is surgical:

1. Extract `SshConnectorConfig` + `ClientConnector::ssh` into `cue-client`
2. Fix `connector_for_profile` to return a working SSH connector
3. Add `ssh_options` + `auto_start` fields

**Effort estimate**: ~1–2 days. No new dependencies.

### Priority 2: HTTP Transport

**Why second**: Enables web UIs, IDE plugins, Docker/container environments
where Unix sockets are not accessible. The `ClientStream` abstraction means the
client side is ~100 lines of adapter code.

**Effort estimate**: ~3–5 days. New dependencies: `axum`, `tokio-tungstenite`.

### Priority 3: Chain Transport

**Why third**: The simplest `cued relay` form is trivially built on top of the
SSH transport work (Priority 1) and `gateway_stdio::relay`. Full multiplexing
(one upstream connection for N local clients) is a Phase 2 enhancement.

**Effort estimate**: ~1–2 days for simple relay; ~1 week for multiplexed relay.

---

## Appendix: `ClientStream` Adapter Patterns

### WebSocket → `AsyncRead + AsyncWrite`

```rust
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::WebSocketStream;
use tungstenite::Message as WsMsg;
use std::{collections::VecDeque, pin::Pin, task::{Context, Poll}};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct WsStream<S> {
    inner: WebSocketStream<S>,
    read_buf: VecDeque<u8>,
}

impl<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> AsyncRead for WsStream<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        // drain read_buf first, then poll inner for more binary frames
        loop {
            if !self.read_buf.is_empty() {
                let n = std::cmp::min(buf.remaining(), self.read_buf.len());
                let chunk: Vec<u8> = self.read_buf.drain(..n).collect();
                buf.put_slice(&chunk);
                return Poll::Ready(Ok(()));
            }
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(WsMsg::Binary(data)))) => self.read_buf.extend(data),
                Poll::Ready(Some(Ok(_))) => continue, // skip text/ping/pong
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))),
                Poll::Ready(None) => return Poll::Ready(Ok(())), // EOF
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> AsyncWrite for WsStream<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_ready(cx) {
            Poll::Ready(Ok(())) => {
                Pin::new(&mut self.inner).start_send(WsMsg::Binary(buf.to_vec().into()))
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;
                Poll::Ready(Ok(buf.len()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))),
            Poll::Pending => Poll::Pending,
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx).map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx).map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))
    }
}
```

### SSH Child Stdin/Stdout → `AsyncRead + AsyncWrite`

Already implemented in `cue-cli/src/main.rs` as `SshChildStream`. Moving this
type to `cue-client/src/ssh.rs` makes it reusable.
