# cue-shell

An async process runtime with TUI for human-agent collaboration.

> ⚠️ **Early development** — design phase, not yet functional.

## Overview

cue-shell (`cue`) is a terminal-native runtime for orchestrating async processes, designed for seamless human-agent collaboration. It is **not** a traditional shell — it's a structured environment where jobs, sessions, and scopes are first-class primitives.

### Key Features

- **Three-layer architecture**: Execution Backend (`cued` daemon) → Session Runtime (core) → Frontend (TUI/MCP/API)
- **Three interaction modes**: JOB ⚡ · AGENT 🤖 · CRON ⏰ — switch with `Shift+Tab`
- **`:` prefix commands**: Vim-style builtin access (`:run`, `:kill`, `:jobs`, `:ask`, `:cron`, ...)
- **Planner/Executor model**: Structured AI collaboration with `:probe`, `:confirm`, `:escalate`
- **Scope persistence**: Environment snapshots with delta storage and lifecycle management
- **Chain syntax**: `->` serial · `~>` ignore-failure · `||` parallel · `||?` any-success

## Architecture

```
┌─────────────────────────────────────────┐
│  L3 Frontend: TUI / MCP / REST API      │
├─────────────────────────────────────────┤
│  L2 Session Runtime (cue-core)          │
│  Job · Session · Scope · Chain          │
├─────────────────────────────────────────┤
│  L1 Execution Backend (cued daemon)     │
│  Unix socket · SQLite · Process mgmt    │
└─────────────────────────────────────────┘
```

## Workspace Structure

```
crates/
├── cue-core/   — Core types and logic: Job, Session, Scope, Chain
├── cued/       — Background daemon: Unix socket server, job orchestration
├── cue-tui/    — TUI frontend: mode switching, command input, job display
└── cue-cli/    — CLI entry point: command parsing, mode dispatch
```

## Development

```bash
# Prerequisites: Rust 1.85+, just

# Build
cargo build

# Run checks
just check

# Run tests
just test

# Full CI locally
just ci

# Install pre-commit hooks
just pre-commit-install
```

## Design Documents

See [`docs/design/`](docs/design/) for the full design documentation:

- **DESIGN.md** — Architecture, core concepts, three-layer model
- **commands-and-modes.md** — Command reference, mode system, `:cron` syntax

## Project Status

| Component | Status |
|-----------|--------|
| Design docs | ✅ Complete (v2) |
| Cargo workspace | ✅ Scaffolded |
| CI/CD | ✅ Configured |
| cue-core | 🚧 Not started |
| cued daemon | 🚧 Not started |
| cue-tui | 🚧 Not started |
| cue-cli | 🚧 Not started |

## License

Licensed under the [MIT License](LICENSE).
