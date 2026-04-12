# IPC Protocol Design — cued ↔ Client

## 1. Transport

- **Unix domain socket**: `$XDG_RUNTIME_DIR/cue-shell/cued.sock`
- Fallback: `$HOME/.cue-shell/cued.sock`
- Single bidirectional connection per client

## 2. Framing

**Length-prefixed JSON**: 4-byte big-endian u32 length header + UTF-8 JSON body.

```
[4 bytes: body length (u32 BE)] [body: UTF-8 JSON]
```

- Max message size: 16 MiB (configurable)
- JSON body is always a valid `Message` envelope

## 3. Message Envelope

All messages share a unified envelope structure:

```rust
#[serde(tag = "type")]
enum Message {
    Request  { id: u32, payload: RequestPayload },
    Response { id: u32, payload: ResponsePayload },
    Event    { payload: EventPayload },
}
```

- `id` present only on Request/Response, used for correlation
- Client assigns monotonically increasing `id` per connection
- Events have no `id` — they are server-pushed

### JSON examples

```json
// Request: Eval (user command)
{"type": "request", "id": 1, "payload": {"Eval": {"input": ":run(retry=3) cargo test", "mode": "Job"}}}

// Response (success — Eval resolved to RunJob)
{"type": "response", "id": 1, "payload": {"Ok": {"JobCreated": {"job_id": "J1"}}}}

// Response (error)
{"type": "response", "id": 1, "payload": {"Err": {"code": "INVALID_SYNTAX", "message": "unexpected token '||?' at position 15"}}}

// Request: Subscribe (protocol command)
{"type": "request", "id": 2, "payload": {"Subscribe": {"channels": ["jobs", "agents", "output:J1"]}}}

// Event
{"type": "event", "payload": {"JobStateChanged": {"job_id": "J1", "old_state": "Pending", "new_state": "Running"}}}

// Request: Complete (editor service)
{"type": "request", "id": 3, "payload": {"Complete": {"input": ":ru", "cursor": 3, "mode": "Job"}}}
{"type": "response", "id": 3, "payload": {"Ok": {"CompletionList": {"items": [{"label": "run", "insert_text": "run", "kind": "Command", "detail": "Run a command as a job"}]}}}}
```

## 4. Communication Model

**Request-Response + Event Stream**, multiplexed on a single connection.

Flow:
1. Client connects to Unix socket
2. Client sends `Subscribe` request to register interest channels
3. cued responds with `Ok`
4. Bidirectional: client sends Requests, cued sends Responses (matched by `id`) + Events (no `id`)

Client must be prepared to receive interleaved Response and Event messages.

## 5. Event Subscription (Channel Model)

```rust
struct SubscribeRequest {
    channels: Vec<String>,
}
```

Channel types:
- `"jobs"` — all job state changes (created, state transitions, removed)
- `"agents"` — all agent state changes
- `"crons"` — all cron state changes
- `"output:<id>"` — stdout/stderr chunks for specific job/agent (e.g., `"output:J1"`, `"output:A2"`)
- `"scopes"` — scope creation, HEAD changes
- `"system"` — cued status, shutdown notices

Operations:
- `Subscribe { channels }` — add channels (additive, no duplicates)
- `Unsubscribe { channels }` — remove channels

TUI default subscription on connect: `["jobs", "agents", "crons", "system"]`
`:out J1` triggers additional: `Subscribe { channels: ["output:J1"] }`

## 6. Request Types (Client → cued)

### Design: Eval-centric

All user commands go through a single `Eval` request. cued owns the full parser
(Tokenizer → Parser → Resolver). Structured requests are only used for
protocol-level operations that don't correspond to user-typed commands.

```rust
enum RequestPayload {
    // === User commands (raw string, parsed by cued) ===
    Eval { input: String, mode: Mode },
    // input: raw user input, e.g. ":run(retry=3) cargo test -> cargo build"
    //        or bare input "cargo test" (cued applies mode default)
    // mode: current TUI mode (JOB/AGENT/CRON) for bare input resolution

    // === Protocol commands (structured, not user-typed) ===

    // Connection / subscription
    Subscribe { channels: Vec<String> },
    Unsubscribe { channels: Vec<String> },

    // :fg mode (raw I/O proxy)
    FgAttach { id: String },  // J1 or A1 (CLI Agent only)
    FgDetach {},
    FgInput { data: Vec<u8> },  // raw bytes from TUI keyboard
    FgResize { cols: u16, rows: u16 },  // terminal resize

    // Editor services (completion & highlighting)
    Complete { input: String, cursor: usize, mode: Mode },
    Highlight { input: String },

    // System
    Ping {},
    Shutdown {},
}
```

## 7. Response Types (cued → Client)

```rust
enum ResponsePayload {
    Ok(OkPayload),
    Err { code: String, message: String },
}

enum OkPayload {
    Ack {},  // generic success (Subscribe, Kill, FgDetach, etc.)
    JobCreated { job_id: String },
    ChainCreated { chain_id: String, job_ids: Vec<String> },
    AgentSpawned { agent_id: String },
    CronAdded { cron_id: String },
    ScopeCreated { hash: String, label: Option<String> },

    JobInfo(JobInfo),
    JobList(Vec<JobInfo>),
    AgentList(Vec<AgentInfo>),
    CronList(Vec<CronInfo>),
    ScopeInfo(ScopeInfo),
    Output { id: String, data: String, truncated: bool },

    // Eval can return any of the above depending on the parsed command.
    // Additionally, some commands produce text output:
    EvalText { text: String },  // for :help, :env list, etc.

    // Editor services
    CompletionList { items: Vec<CompletionItem> },
    HighlightResult { spans: Vec<HighlightSpan> },

    FgAttached { id: String },
    Pong {},
}

// Completion item (for Complete request)
struct CompletionItem {
    label: String,
    insert_text: String,
    kind: CompletionKind,  // Command, Param, Id, Path, Operator
    detail: Option<String>,
}

// Highlight span (for Highlight request)
struct HighlightSpan {
    range: (usize, usize),  // byte offset (start, end)
    kind: HighlightKind,    // CommandPrefix, CommandName, Operator, IdRef, Error, ...
}
```

## 8. Event Types (cued → Client, pushed)

```rust
enum EventPayload {
    // Job events (channel: "jobs")
    JobStateChanged { job_id: String, old_state: JobState, new_state: JobState },
    JobCreated { job_id: String, pipeline: String },
    JobRemoved { job_id: String },

    // Agent events (channel: "agents")
    AgentStateChanged { agent_id: String, old_state: AgentState, new_state: AgentState },
    AgentMessage { agent_id: String, role: String, content: String },  // conversation stream

    // Cron events (channel: "crons")
    CronTriggered { cron_id: String, job_id: String },  // cron fired, spawned job
    CronRemoved { cron_id: String },

    // Output events (channel: "output:<id>")
    OutputChunk { id: String, stream: Stream, data: String },
    // stream: "stdout" | "stderr"
    // data: UTF-8 text; for non-UTF-8 bytes, base64-encoded with a sibling field:
    // OutputChunkBinary { id: String, stream: Stream, base64: String }
    OutputEof { id: String },  // process closed its output

    // Scope events (channel: "scopes")
    ScopeCreated { hash: String, label: Option<String> },
    HeadChanged { old_hash: String, new_hash: String },

    // :fg events (no channel — only sent to fg-attached client)
    FgOutput { data: Vec<u8> },  // raw pty output
    FgExited { id: String, reason: String },  // process exited while in :fg

    // System events (channel: "system")
    ShuttingDown { reason: String },
    DaemonReady {},
}
```

## 9. :fg Full-Duplex Proxy Mode

When client sends `FgAttach { id: "J1" }`:
1. cued responds `FgAttached { id: "J1" }`
2. Connection enters **fg proxy mode** for this job/agent:
   - Client → cued: `FgInput { data }` messages (raw keystrokes)
   - cued → client: `FgOutput { data }` events (raw pty output)
   - Client → cued: `FgResize { cols, rows }` on terminal resize
3. Client sends `FgDetach {}` to exit (triggered by Ctrl+Z)
4. cued responds `Ack {}`, connection returns to normal mode

During fg mode, other Request/Response and Event messages continue normally on the same connection.

## 10. Error Codes

Standard error codes returned in `Err { code, message }`:

| Code | Meaning |
|---|---|
| `NOT_FOUND` | Job/Agent/Cron/Scope not found |
| `INVALID_STATE` | Operation not valid in current state (e.g., :fg on Done job) |
| `INVALID_SCOPE` | Referenced scope hash not found |
| `INVALID_SYNTAX` | Malformed pipeline/chain/cron expression |
| `ALREADY_EXISTS` | Duplicate operation (e.g., already fg-attached) |
| `NOT_SUPPORTED` | Operation not supported (e.g., :fg on API Agent) |
| `PERMISSION_DENIED` | Planner-only command from non-planner client |
| `INTERNAL` | Unexpected cued error |

## 11. Connection Lifecycle

```
Client                              cued
  |                                   |
  |--- connect (Unix socket) -------->|
  |                                   |
  |--- Subscribe {channels} --------->|
  |<-- Response {Ok: Ack} ------------|
  |                                   |
  |--- RunJob {pipeline} ------------>|
  |<-- Response {Ok: JobCreated} -----|
  |<-- Event {JobStateChanged} -------|  (async)
  |<-- Event {OutputChunk} -----------|  (if subscribed)
  |                                   |
  |--- FgAttach {id: "J1"} ---------->|
  |<-- Response {Ok: FgAttached} -----|
  |<-- FgOutput {data} ---------------|  (streaming)
  |--- FgInput {data} --------------->|  (keystrokes)
  |--- FgDetach {} ------------------->|
  |<-- Response {Ok: Ack} ------------|
  |                                   |
  |--- close connection -------------->|
```

## Design Notes

- All string IDs (J1, A2, C3) are used consistently across Request/Response/Event
- ModeParams is a `HashMap<String, Value>` matching the `()` syntax
- cued must buffer recent output per job for `GetOutput` (tail query) — configurable ring buffer
- Multiple clients can connect simultaneously; each has independent subscriptions
- Future: WebSocket bridge for remote access (same JSON protocol, different transport)
