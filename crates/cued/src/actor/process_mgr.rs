//! ProcessManager actor — OS child process lifecycle (STUB).
//!
//! The skeleton immediately transitions jobs through Pending → Running → Done
//! without actually spawning processes.  Real fork/exec/pty support comes later.

use tokio::sync::mpsc;
use tracing::{debug, info};

use cue_core::ipc::EventPayload;
use cue_core::job::JobStatus;

use super::{ActorSystem, EventBusMsg, ProcessMgrMsg, SchedulerMsg};

/// Spawn the ProcessManager actor task.
pub fn spawn(mut rx: mpsc::Receiver<ProcessMgrMsg>, sys: ActorSystem) {
    tokio::spawn(async move {
        debug!("process_mgr: started (stub mode)");

        while let Some(msg) = rx.recv().await {
            match msg {
                ProcessMgrMsg::SpawnJob {
                    job_id,
                    command_line,
                    scope_hash,
                } => {
                    info!(
                        %job_id,
                        cmd = ?command_line,
                        %scope_hash,
                        "process_mgr: spawn (stub)"
                    );

                    // Emit Pending → Running.
                    let _ = sys
                        .event_bus
                        .send(EventBusMsg::Publish {
                            payload: EventPayload::JobStateChanged {
                                job_id: job_id.to_string(),
                                old_state: JobStatus::Pending,
                                new_state: JobStatus::Running,
                            },
                            channel: "jobs".into(),
                        })
                        .await;

                    // Simulate brief execution.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

                    // Emit Running → Done.
                    let _ = sys
                        .event_bus
                        .send(EventBusMsg::Publish {
                            payload: EventPayload::JobStateChanged {
                                job_id: job_id.to_string(),
                                old_state: JobStatus::Running,
                                new_state: JobStatus::Done,
                            },
                            channel: "jobs".into(),
                        })
                        .await;

                    // Notify Scheduler.
                    let _ = sys
                        .scheduler
                        .send(SchedulerMsg::JobFinished {
                            job_id,
                            exit_code: 0,
                        })
                        .await;
                }

                ProcessMgrMsg::KillJob { job_id } => {
                    info!(%job_id, "process_mgr: kill (stub)");
                    let _ = sys
                        .event_bus
                        .send(EventBusMsg::Publish {
                            payload: EventPayload::JobStateChanged {
                                job_id: job_id.to_string(),
                                old_state: JobStatus::Running,
                                new_state: JobStatus::Killed,
                            },
                            channel: "jobs".into(),
                        })
                        .await;
                }

                ProcessMgrMsg::Shutdown => {
                    debug!("process_mgr: shutting down");
                    break;
                }
            }
        }

        debug!("process_mgr: stopped");
    });
}
