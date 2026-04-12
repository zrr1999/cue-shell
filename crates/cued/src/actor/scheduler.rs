//! Scheduler actor — command routing, ID assignment, chain execution.

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use cue_core::ipc::{OkPayload, ResponsePayload, error_code};
use cue_core::{AgentId, JobId};

use crate::parser::resolver::ResolvedCommand;

use super::{ActorSystem, GatewayMsg, ProcessMgrMsg, SchedulerMsg, ScopeStoreMsg};

/// Spawn the Scheduler actor task.
pub fn spawn(mut rx: mpsc::Receiver<SchedulerMsg>, sys: ActorSystem) {
    tokio::spawn(async move {
        let mut next_job: u32 = 1;
        let mut next_agent: u32 = 1;
        let mut _next_cron: u32 = 1;

        debug!("scheduler: started");

        while let Some(msg) = rx.recv().await {
            match msg {
                SchedulerMsg::Eval {
                    client_id,
                    request_id,
                    command,
                } => {
                    let response = handle_command(
                        command,
                        &mut next_job,
                        &mut next_agent,
                        &mut _next_cron,
                        &sys,
                    )
                    .await;
                    let _ = sys
                        .gateway
                        .send(GatewayMsg::SendResponse {
                            client_id,
                            request_id,
                            payload: response,
                        })
                        .await;
                }

                SchedulerMsg::JobFinished { job_id, exit_code } => {
                    info!(%job_id, exit_code, "scheduler: job finished");
                    // TODO: advance chain if applicable.
                }

                SchedulerMsg::Shutdown => {
                    debug!("scheduler: shutting down");
                    break;
                }
            }
        }

        debug!("scheduler: stopped");
    });
}

async fn handle_command(
    cmd: ResolvedCommand,
    next_job: &mut u32,
    next_agent: &mut u32,
    _next_cron: &mut u32,
    sys: &ActorSystem,
) -> ResponsePayload {
    match cmd {
        ResolvedCommand::Run { chain, .. } => {
            let jid = JobId(*next_job);
            *next_job += 1;

            // Collect the first leaf's command words for the stub process manager.
            let cmd_words = first_leaf_command(&chain);

            // Get current HEAD scope hash.
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = sys
                .scope_store
                .send(ScopeStoreMsg::GetHead { reply: tx })
                .await;
            let scope_hash = match rx.await {
                Ok(h) => h,
                Err(_) => {
                    return ResponsePayload::err(error_code::INTERNAL, "scope_store unreachable");
                }
            };

            info!(%jid, ?cmd_words, "scheduler: spawning job");

            let _ = sys
                .process_mgr
                .send(ProcessMgrMsg::SpawnJob {
                    job_id: jid,
                    command_line: cmd_words,
                    scope_hash,
                })
                .await;

            ResponsePayload::Ok(OkPayload::JobCreated {
                job_id: jid.to_string(),
            })
        }

        ResolvedCommand::Ask { text, .. } => {
            let aid = AgentId(*next_agent);
            *next_agent += 1;
            info!(%aid, %text, "scheduler: agent spawned (stub)");
            ResponsePayload::Ok(OkPayload::AgentSpawned {
                agent_id: aid.to_string(),
            })
        }

        ResolvedCommand::Jobs => {
            // Empty list for skeleton.
            ResponsePayload::Ok(OkPayload::JobList(vec![]))
        }

        ResolvedCommand::Agents => ResponsePayload::Ok(OkPayload::AgentList(vec![])),

        ResolvedCommand::Crons => ResponsePayload::Ok(OkPayload::CronList(vec![])),

        ResolvedCommand::Scopes => {
            // Return a minimal info about head scope.
            ResponsePayload::Ok(OkPayload::EvalText {
                text: "scope listing not yet implemented".into(),
            })
        }

        ResolvedCommand::Help { topic } => {
            let text = match topic.as_deref() {
                Some(t) => format!("help for '{t}' — not yet implemented"),
                None => "cue-shell help — not yet implemented".into(),
            };
            ResponsePayload::Ok(OkPayload::EvalText { text })
        }

        ResolvedCommand::Clear => ResponsePayload::ack(),

        ResolvedCommand::Quit => {
            info!("scheduler: quit requested, initiating shutdown");
            let _ = sys.gateway.send(GatewayMsg::Shutdown).await;
            ResponsePayload::ack()
        }

        ResolvedCommand::Cd { path } => {
            // Fork scope with new cwd.
            let delta = cue_core::scope::EnvDelta {
                set: std::collections::BTreeMap::new(),
                unset: vec![],
                cwd: Some(std::path::PathBuf::from(&path)),
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = sys
                .scope_store
                .send(ScopeStoreMsg::Fork { delta, reply: tx })
                .await;
            match rx.await {
                Ok(Ok(hash)) => ResponsePayload::Ok(OkPayload::ScopeCreated {
                    hash: hash.to_string(),
                    label: Some(format!("cd {path}")),
                }),
                Ok(Err(e)) => ResponsePayload::err(error_code::INTERNAL, e.to_string()),
                Err(_) => ResponsePayload::err(error_code::INTERNAL, "scope_store unreachable"),
            }
        }

        // Stubs for commands not yet implemented.
        ResolvedCommand::Kill { id }
        | ResolvedCommand::Retry { id }
        | ResolvedCommand::Out { id }
        | ResolvedCommand::Err { id }
        | ResolvedCommand::Fg { id }
        | ResolvedCommand::Wait { id }
        | ResolvedCommand::Send { id }
        | ResolvedCommand::Cancel { id }
        | ResolvedCommand::Pause { id }
        | ResolvedCommand::Resume { id }
        | ResolvedCommand::Probe { id } => {
            warn!(%id, "scheduler: command not yet implemented");
            ResponsePayload::err(error_code::NOT_SUPPORTED, "command not yet implemented")
        }

        _ => {
            warn!("scheduler: unhandled command variant");
            ResponsePayload::err(error_code::NOT_SUPPORTED, "command not yet implemented")
        }
    }
}

/// Extract the first leaf pipeline's command words (for the stub process manager).
fn first_leaf_command(node: &cue_core::pipeline::ChainNode) -> Vec<String> {
    match node {
        cue_core::pipeline::ChainNode::Leaf(p) => p
            .segments
            .first()
            .map(|s| s.command.clone())
            .unwrap_or_default(),
        cue_core::pipeline::ChainNode::Serial { left, .. }
        | cue_core::pipeline::ChainNode::Parallel { left, .. } => first_leaf_command(left),
    }
}
