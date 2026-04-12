# Cue Shell — Design Overview

> Architecture reference for cue-shell: an async process runtime with TUI
> for human-agent collaboration.

## Architecture

```
┌─────────────┐     Unix socket      ┌──────────────────────────┐
│  cue-tui    │◄────────────────────►│         cued             │
│  (ratatui)  │    length-prefixed   │  ┌─────────┐            │
└─────────────┘    JSON protocol     │  │ Gateway  │            │
                                     │  └────┬────┘            │
┌─────────────┐                      │  ┌────▼─────┐           │
│  cue-cli    │◄────────────────────►│  │Scheduler │           │
│  (headless) │                      │  └────┬─────┘           │
└─────────────┘                      │  ┌────▼────────┐        │
                                     │  │ ProcessMgr  │        │
                                     │  └─────────────┘        │
                                     │  ┌─────────────┐        │
                                     │  │ ScopeStore  │ SQLite │
                                     │  └─────────────┘        │
                                     │  ┌─────────────┐        │
                                     │  │  EventBus   │        │
                                     │  └─────────────┘        │
                                     └──────────────────────────┘
```

### Crate Layout

| Crate | Role |
|---|---|
| **cue-core** | Shared types, parser, protocol definitions |
| **cued** | Background daemon — scheduler, process manager, scope store |
| **cue-tui** | Interactive TUI client (ratatui + crossterm) |
| **cue-cli** | Headless CLI client for scripting |

## Core Primitives

| Primitive | ID | Description |
|---|---|---|
| **Job** | J1, J2, ... | OS child process (or pipeline of processes) |
| **Agent** | A1, A2, ... | AI assistant (CLI with pty, or API-only) |
| **Cron** | C1, C2, ... | Scheduled/delayed task that spawns Jobs |
| **Scope** | S0@a3f1 | Immutable environment snapshot (content-addressed, blake3) |
| **Chain** | — | Job orchestration graph (serial/parallel) |
| **Pipeline** | — | Process pipe chain within a single Job |

## Operator Model (Two Layers)

```
Pipeline (within a Job):  |>   |&>   |!>
Chain (between Jobs):     ->   ~>    ||   ||?
```

Priority: pipe (1) > parallel (2) > serial (3).

`a |> b -> c || d ~> e` parses as `Job1(a|>b) -> (Job2(c) || Job3(d)) ~> Job4(e)`.

## Modes

| Mode | Indicator | Bare input becomes |
|---|---|---|
| JOB ⚡ | `[JOB ⚡] > _` | `:run <input>` |
| AGENT 🤖 | `[AGENT 🤖] > _` | `:ask <input>` |
| CRON ⏰ | `[CRON ⏰] > _` | `:cron <input>` |

Shift+Tab cycles modes. `:` prefix always invokes a builtin command regardless of mode.

## Mode Params

```
:run(retry=3, timeout=30s) cargo test
:ask(model=gpt-4) explain this error
:cron(scope=S0@a3f1) every 5m cargo clippy
```

Parenthesized `key=value` pairs immediately after the command name configure
execution behavior. They override `config.toml` defaults. Only "launcher" commands
support mode params: `:run`, `:ask`, `:cron`, `:spawn`, `:scope new`.

## Scope Model

Scopes are **immutable, content-addressed environment snapshots**:

- ID = blake3(env + cwd + ...) → identical environments share the same hash
- Delta storage: `parent_hash` + `EnvDelta` (set/unset/cwd changes)
- Display: `S0@a3f1` (human label + short hash)
- Job holds `start_scope` and `end_scope` (None until complete)
- Default scope = movable HEAD pointer, modified via `:env set` / `:cd`

Analogy: Scope ≈ git commit, Job ≈ git diff, fork ≈ git branch, default scope ≈ HEAD.

## IPC Protocol

- **Transport**: Unix domain socket at `$XDG_RUNTIME_DIR/cue-shell/cued.sock`
- **Framing**: 4-byte big-endian u32 length + UTF-8 JSON body
- **Model**: Request/Response + Event push, multiplexed on a single connection
- **Eval-centric**: user commands sent as raw strings via `Eval { input, mode }`;
  cued owns the full parser (Tokenizer → Parser → Resolver)
- **Subscriptions**: channel model (`jobs`, `agents`, `crons`, `output:J1`, `scopes`, `system`)

## Storage (Three Layers)

| Layer | What | Where |
|---|---|---|
| In-memory | Ring buffers (per-Job, 1 MiB), active state | cued process |
| File system | Output logs (`J1.log`, `A1.log`) | `$XDG_DATA_HOME/cue-shell/output/` |
| SQLite | Scopes, Crons, job history, config | `$XDG_DATA_HOME/cue-shell/cued.db` |

## Design Documents

| Document | Contents |
|---|---|
| [commands-and-modes.md](commands-and-modes.md) | Complete command reference, modes, cron syntax |
| [core-types.md](core-types.md) | Rust type definitions — Scope, Job, Agent, Cron, Pipeline, Chain |
| [tui.md](tui.md) | TUI architecture, layout, interaction design |
| [ipc-protocol.md](ipc-protocol.md) | cued ↔ client protocol specification |
| [parser.md](parser.md) | Command parser — tokenizer, grammar, completion |
| [daemon-architecture.md](daemon-architecture.md) | cued internals — actors, storage, startup/shutdown |

## Research Documents

| Document | Contents |
|---|---|
| [competitive-landscape.md](../research/competitive-landscape.md) | Competitive analysis of shells, TUIs, process managers |
| [syntax-decisions.md](../research/syntax-decisions.md) | Prefix syntax selection process and cron grammar design |

## Tech Stack

| Dependency | Role |
|---|---|
| tokio | Async runtime (multi-threaded) |
| ratatui + crossterm | TUI rendering |
| bpaf | CLI argument parsing |
| rusqlite | SQLite persistence |
| serde + serde_json | Serialization (IPC protocol) |
| tracing | Structured logging |
| thiserror + anyhow | Error handling |

MSRV: 1.90 · Edition: 2024 · License: MIT
