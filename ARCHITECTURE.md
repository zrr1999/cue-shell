# cue-shell Architecture

cue-shell is a durable process substrate. The daemon owns process lifecycle,
scope persistence, schedules, and event streams; clients are views and control
surfaces over that same substrate.

Start with the design index:

- [Design overview](docs/design/README.md)
- [Conceptual model](docs/design/conceptual-model.md)
- [Daemon architecture](docs/design/daemon-architecture.md)
- [IPC protocol](docs/design/ipc-protocol.md)
- [Commands and modes](docs/design/commands-and-modes.md)

The short boundary is:

```text
argv/cwd/env/stdin -> job_id, exit code, stdout/stderr, structured events, scope hash
```

Agent runtime policy, workflow orchestration, backend lifecycle, and model
selection belong above cue-shell. cue-shell keeps the process layer small,
observable, and durable.
