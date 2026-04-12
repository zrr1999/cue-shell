//! Actor system for cued.
//!
//! Five actors communicate via bounded `mpsc` channels:
//!
//! ```text
//! Gateway  ──→  Scheduler  ──→  ProcessMgr
//!    │              │
//!    │         ScopeStore
//!    │
//!    └────────  EventBus  ←── (all actors publish)
//! ```

pub mod event_bus;
pub mod gateway;
pub mod process_mgr;
pub mod scheduler;
pub mod scope_store;

use anyhow::Result;
use tokio::sync::mpsc;

use cue_core::ScopeHash;
use cue_core::ipc::{EventPayload, OkPayload, ResponsePayload};
use cue_core::scope::{EnvDelta, EnvSnapshot, Scope};

use crate::parser::resolver::ResolvedCommand;

/// Default bounded channel capacity for actor mailboxes.
pub const ACTOR_CHANNEL_CAP: usize = 256;

/// Per-client event channel capacity.
pub const CLIENT_EVENT_CAP: usize = 64;

// ── Per-actor message types ──

/// Messages handled by the Gateway actor.
pub enum GatewayMsg {
    /// Deliver a response to a specific client.
    SendResponse {
        client_id: u64,
        request_id: u32,
        payload: ResponsePayload,
    },
    /// Push an event to all subscribers of `channel`.
    PushEvent {
        payload: EventPayload,
        channel: String,
    },
    /// Graceful shutdown.
    Shutdown,
}

/// Messages handled by the Scheduler actor.
pub enum SchedulerMsg {
    /// Evaluate a resolved command on behalf of a client.
    Eval {
        client_id: u64,
        request_id: u32,
        command: ResolvedCommand,
    },
    /// A job has finished execution.
    JobFinished {
        job_id: cue_core::JobId,
        exit_code: i32,
    },
    /// Graceful shutdown.
    Shutdown,
}

/// Messages handled by the ProcessManager actor.
pub enum ProcessMgrMsg {
    /// Spawn a child process for the given job.
    SpawnJob {
        job_id: cue_core::JobId,
        command_line: Vec<String>,
        scope_hash: ScopeHash,
    },
    /// Kill a running job.
    KillJob { job_id: cue_core::JobId },
    /// Read the tail of a running job's output ring buffer.
    GetOutput {
        job_id: cue_core::JobId,
        tail_bytes: usize,
        reply: tokio::sync::oneshot::Sender<Option<Vec<u8>>>,
    },
    /// Graceful shutdown.
    Shutdown,
}

/// Messages handled by the ScopeStore actor.
pub enum ScopeStoreMsg {
    /// Get the current HEAD scope hash.
    GetHead {
        reply: tokio::sync::oneshot::Sender<ScopeHash>,
    },
    /// Get a scope by hash.
    GetScope {
        hash: ScopeHash,
        reply: tokio::sync::oneshot::Sender<Option<Scope>>,
    },
    /// Get the current HEAD snapshot.
    GetHeadSnapshot {
        reply: tokio::sync::oneshot::Sender<Option<EnvSnapshot>>,
    },
    /// Create a root scope from a full snapshot.
    CreateRoot {
        snapshot: EnvSnapshot,
        reply: tokio::sync::oneshot::Sender<ScopeHash>,
    },
    /// Fork a child scope from the current HEAD.
    Fork {
        delta: EnvDelta,
        reply: tokio::sync::oneshot::Sender<Result<ScopeHash>>,
    },
    /// Graceful shutdown.
    Shutdown,
}

/// Messages handled by the EventBus actor.
pub enum EventBusMsg {
    /// Register a client for a channel.
    Subscribe {
        client_id: u64,
        channel: String,
        sender: mpsc::Sender<EventPayload>,
    },
    /// Remove a client from a channel.
    Unsubscribe { client_id: u64, channel: String },
    /// Remove a client from ALL channels (on disconnect).
    UnsubscribeAll { client_id: u64 },
    /// Broadcast an event to all subscribers of a channel.
    Publish {
        payload: EventPayload,
        channel: String,
    },
    /// Graceful shutdown.
    Shutdown,
}

// ── Actor handle bundle ──

/// Holds all actor sender handles.  Cheaply cloneable.
#[derive(Clone)]
pub struct ActorSystem {
    pub gateway: mpsc::Sender<GatewayMsg>,
    pub scheduler: mpsc::Sender<SchedulerMsg>,
    pub process_mgr: mpsc::Sender<ProcessMgrMsg>,
    pub scope_store: mpsc::Sender<ScopeStoreMsg>,
    pub event_bus: mpsc::Sender<EventBusMsg>,
}

impl ActorSystem {
    /// Send `Shutdown` to every actor.
    pub async fn shutdown(&self) {
        // Ignore errors — the actor may already have stopped.
        let _ = self.gateway.send(GatewayMsg::Shutdown).await;
        let _ = self.scheduler.send(SchedulerMsg::Shutdown).await;
        let _ = self.process_mgr.send(ProcessMgrMsg::Shutdown).await;
        let _ = self.scope_store.send(ScopeStoreMsg::Shutdown).await;
        let _ = self.event_bus.send(EventBusMsg::Shutdown).await;
    }
}

/// Spawn all five actors, returning the [`ActorSystem`] handle bundle.
pub async fn spawn_all(
    socket_path: std::path::PathBuf,
    db: rusqlite::Connection,
) -> Result<ActorSystem> {
    // Create channels.
    let (gw_tx, gw_rx) = mpsc::channel::<GatewayMsg>(ACTOR_CHANNEL_CAP);
    let (sched_tx, sched_rx) = mpsc::channel::<SchedulerMsg>(ACTOR_CHANNEL_CAP);
    let (pm_tx, pm_rx) = mpsc::channel::<ProcessMgrMsg>(ACTOR_CHANNEL_CAP);
    let (ss_tx, ss_rx) = mpsc::channel::<ScopeStoreMsg>(ACTOR_CHANNEL_CAP);
    let (eb_tx, eb_rx) = mpsc::channel::<EventBusMsg>(ACTOR_CHANNEL_CAP);

    let sys = ActorSystem {
        gateway: gw_tx,
        scheduler: sched_tx,
        process_mgr: pm_tx,
        scope_store: ss_tx,
        event_bus: eb_tx,
    };

    // Spawn actors.
    event_bus::spawn(eb_rx);
    scope_store::spawn(ss_rx, db, sys.clone());
    process_mgr::spawn(pm_rx, sys.clone());
    scheduler::spawn(sched_rx, sys.clone());
    gateway::spawn(gw_rx, socket_path, sys.clone()).await?;

    // Publish DaemonReady event.
    let _ = sys
        .event_bus
        .send(EventBusMsg::Publish {
            payload: EventPayload::DaemonReady {},
            channel: "system".into(),
        })
        .await;

    Ok(sys)
}

/// Helper to build a success response.
pub fn ok_response(payload: OkPayload) -> ResponsePayload {
    ResponsePayload::Ok(payload)
}
