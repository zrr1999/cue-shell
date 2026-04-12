//! ProcessManager actor — OS child process lifecycle.
//!
//! Spawns real child processes via `tokio::process::Command`, reads their
//! stdout/stderr into a [`RingBuffer`], writes a persistent log file, and
//! publishes output chunks + state-change events.

use std::collections::HashMap;
use std::io::Write;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use cue_core::JobId;
use cue_core::ipc::{EventPayload, Stream as OutputStream};
use cue_core::job::JobStatus;
use cue_core::scope::EnvSnapshot;

use super::{ActorSystem, EventBusMsg, ProcessMgrMsg, SchedulerMsg, ScopeStoreMsg};
use crate::ring_buffer::RingBuffer;

// ── Per-child bookkeeping ──

struct ProcessEntry {
    #[allow(dead_code)]
    job_id: JobId,
    status: JobStatus,
    /// Handle for the background reader/waiter task.
    _reader_handle: tokio::task::JoinHandle<()>,
    /// Send on this channel to request a kill.
    kill_tx: mpsc::Sender<()>,
    /// Shared ring buffer holding the latest output bytes (FIX 7).
    ring_buffer: Arc<Mutex<RingBuffer>>,
}

// ── Actor entry point ──

/// Spawn the ProcessManager actor task.
pub fn spawn(mut rx: mpsc::Receiver<ProcessMgrMsg>, sys: ActorSystem) {
    tokio::spawn(async move {
        debug!("process_mgr: started");

        let mut children: HashMap<u32, ProcessEntry> = HashMap::new();

        // FIX 2: internal channel for reader tasks to request cleanup.
        let (cleanup_tx, mut cleanup_rx) = mpsc::channel::<JobId>(super::ACTOR_CHANNEL_CAP);

        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break; };
                    match msg {
                ProcessMgrMsg::SpawnJob {
                    job_id,
                    command_line,
                    scope_hash,
                } => {
                    info!(%job_id, cmd = ?command_line, %scope_hash, "process_mgr: spawn");

                    // 1. Query ScopeStore for the environment snapshot.
                    let snapshot = {
                        let (tx, rx) = oneshot::channel();
                        if sys
                            .scope_store
                            .send(ScopeStoreMsg::GetScope {
                                hash: scope_hash,
                                reply: tx,
                            })
                            .await
                            .is_err()
                        {
                            error!(%job_id, "process_mgr: scope_store channel closed");
                            // FIX 1: fail the job instead of continuing with daemon env.
                            emit_state_change(&sys, job_id, JobStatus::Pending, JobStatus::Failed).await;
                            let _ = sys.scheduler.send(SchedulerMsg::JobFinished { job_id, exit_code: -1 }).await;
                            continue;
                        }
                        match rx.await {
                            Ok(Some(scope)) => scope.snapshot,
                            Ok(None) => {
                                // FIX 1: scope not found — fail the job.
                                error!(%job_id, %scope_hash, "process_mgr: scope not found");
                                emit_state_change(&sys, job_id, JobStatus::Pending, JobStatus::Failed).await;
                                let _ = sys.scheduler.send(SchedulerMsg::JobFinished { job_id, exit_code: -1 }).await;
                                continue;
                            }
                            Err(_) => {
                                // FIX 1: oneshot dropped — fail the job.
                                error!(%job_id, "process_mgr: scope_store reply dropped");
                                emit_state_change(&sys, job_id, JobStatus::Pending, JobStatus::Failed).await;
                                let _ = sys.scheduler.send(SchedulerMsg::JobFinished { job_id, exit_code: -1 }).await;
                                continue;
                            }
                        }
                    };

                    // 2. Build the tokio Command.
                    let Some(program) = command_line.first() else {
                        error!(%job_id, "process_mgr: empty command_line");
                        continue;
                    };

                    let mut cmd = tokio::process::Command::new(program);
                    if command_line.len() > 1 {
                        cmd.args(&command_line[1..]);
                    }
                    cmd.stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .kill_on_drop(true);

                    if let Some(ref snap) = snapshot {
                        apply_env(&mut cmd, snap);
                    }

                    // 3. Spawn the child process.
                    let mut child = match cmd.spawn() {
                        Ok(c) => c,
                        Err(e) => {
                            error!(%job_id, err = %e, "process_mgr: spawn failed");
                            // Transition directly to Failed.
                            emit_state_change(&sys, job_id, JobStatus::Pending, JobStatus::Failed).await;
                            let _ = sys
                                .scheduler
                                .send(SchedulerMsg::JobFinished {
                                    job_id,
                                    exit_code: -1,
                                })
                                .await;
                            continue;
                        }
                    };

                    let pid = child.id().unwrap_or(0);
                    info!(%job_id, pid, "process_mgr: child spawned");

                    // 4. Emit Pending → Running.
                    emit_state_change(&sys, job_id, JobStatus::Pending, JobStatus::Running).await;

                    // 5. Open log file (FIX 5: offloaded to blocking thread).
                    let log_file = open_log_file(job_id).await;

                    // 6. Take stdout/stderr handles.
                    let stdout = child.stdout.take();
                    let stderr = child.stderr.take();

                    // 7. Spawn reader/waiter background task.
                    let (kill_tx, kill_rx) = mpsc::channel::<()>(1);
                    // FIX 7: shared ring buffer accessible from ProcessEntry.
                    let ring_buffer = Arc::new(Mutex::new(RingBuffer::default()));
                    let ring_clone = ring_buffer.clone();
                    let sys_clone = sys.clone();
                    let cleanup_tx_clone = cleanup_tx.clone();
                    let reader_handle = tokio::spawn(reader_task(
                        job_id, child, stdout, stderr, log_file, kill_rx, sys_clone,
                        ring_clone, cleanup_tx_clone,
                    ));

                    children.insert(
                        job_id.0,
                        ProcessEntry {
                            job_id,
                            status: JobStatus::Running,
                            _reader_handle: reader_handle,
                            kill_tx,
                            ring_buffer,
                        },
                    );
                }

                ProcessMgrMsg::KillJob { job_id } => {
                    info!(%job_id, "process_mgr: kill requested");
                    if let Some(entry) = children.get_mut(&job_id.0) {
                        if !entry.status.is_terminal() {
                            entry.status = JobStatus::Killed;
                            // Signal the reader task to perform the kill sequence.
                            let _ = entry.kill_tx.send(()).await;
                        }
                    } else {
                        warn!(%job_id, "process_mgr: kill — job not found");
                    }
                }

                // FIX 7: expose ring-buffer contents for live-tail queries.
                ProcessMgrMsg::GetOutput { job_id, tail_bytes, reply } => {
                    let result = children
                        .get(&job_id.0)
                        .map(|entry| entry.ring_buffer.lock().unwrap().tail(tail_bytes));
                    let _ = reply.send(result);
                }

                ProcessMgrMsg::Shutdown => {
                    debug!("process_mgr: shutting down — killing all children");
                    for entry in children.values() {
                        if !entry.status.is_terminal() {
                            // FIX 4: non-blocking send so shutdown cannot stall.
                            let _ = entry.kill_tx.try_send(());
                        }
                    }
                    // Give children a moment to exit.
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    break;
                }
                    }
                }

                // FIX 2: reader task finished — remove the stale entry.
                Some(job_id) = cleanup_rx.recv() => {
                    debug!(%job_id, "process_mgr: cleaning up finished child");
                    children.remove(&job_id.0);
                }
            }
        }

        debug!("process_mgr: stopped");
    });
}

// ── Helpers ──

/// Apply the scope's environment snapshot to a Command.
fn apply_env(cmd: &mut tokio::process::Command, snap: &EnvSnapshot) {
    // Clear inherited env and set from snapshot.
    cmd.env_clear();
    cmd.envs(snap.env.iter());
    cmd.current_dir(&snap.cwd);
}

/// Open (or create) the append-only log file for a job's output.
///
/// FIX 5: runs on the blocking thread-pool so the tokio runtime thread is
/// never stalled by filesystem syscalls.
async fn open_log_file(job_id: JobId) -> Option<std::fs::File> {
    tokio::task::spawn_blocking(move || {
        let dir = crate::dirs::output_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            error!(%job_id, err = %e, "process_mgr: cannot create output dir");
            return None;
        }
        let path = dir.join(format!("{job_id}.log"));
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => Some(f),
            Err(e) => {
                error!(%job_id, path = %path.display(), err = %e, "process_mgr: open log file");
                None
            }
        }
    })
    .await
    .unwrap_or(None)
}

/// Background task that reads stdout/stderr, populates the ring buffer,
/// writes to the log file, emits events, and waits for the child to exit.
#[allow(clippy::too_many_arguments)]
async fn reader_task(
    job_id: JobId,
    mut child: tokio::process::Child,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    log_file: Option<std::fs::File>,
    mut kill_rx: mpsc::Receiver<()>,
    sys: ActorSystem,
    ring: Arc<Mutex<RingBuffer>>,
    cleanup_tx: mpsc::Sender<JobId>,
) {
    // FIX 5: wrap the log file so it can be shared with spawn_blocking.
    let log_file = Arc::new(Mutex::new(log_file));

    // Wrap stdout/stderr in Option<BufReader>-like async readers.
    let mut stdout = stdout;
    let mut stderr = stderr;

    let mut stdout_buf = vec![0u8; 8192];
    let mut stderr_buf = vec![0u8; 8192];
    let mut stdout_done = stdout.is_none();
    let mut stderr_done = stderr.is_none();

    loop {
        tokio::select! {
            // Kill signal from the main actor loop.
            _ = kill_rx.recv() => {
                info!(%job_id, "process_mgr: sending SIGTERM");
                let _ = child.start_kill();

                // Wait up to 10 s for graceful exit, then SIGKILL (kill_on_drop).
                let timeout = tokio::time::sleep(std::time::Duration::from_secs(10));
                tokio::select! {
                    status = child.wait() => {
                        let code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                        debug!(%job_id, code, "process_mgr: child exited after SIGTERM");
                    }
                    () = timeout => {
                        warn!(%job_id, "process_mgr: child did not exit in 10 s — dropping (SIGKILL)");
                        // child is dropped here → kill_on_drop sends SIGKILL
                        drop(child);
                    }
                }

                emit_state_change(&sys, job_id, JobStatus::Running, JobStatus::Killed).await;
                let _ = sys.scheduler.send(SchedulerMsg::JobFinished { job_id, exit_code: -1 }).await;
                // FIX 2: tell the main loop to remove our entry.
                let _ = cleanup_tx.send(job_id).await;
                return;
            }

            // Read stdout.
            result = async {
                match stdout.as_mut() {
                    Some(s) => s.read(&mut stdout_buf).await,
                    None => std::future::pending().await,
                }
            }, if !stdout_done => {
                match result {
                    Ok(0) => { stdout_done = true; }
                    Ok(n) => {
                        let chunk = &stdout_buf[..n];
                        ring.lock().unwrap().push(chunk);
                        write_log(&log_file, chunk).await;
                        emit_output(&sys, job_id, OutputStream::Stdout, chunk).await;
                    }
                    Err(e) => {
                        debug!(%job_id, err = %e, "process_mgr: stdout read error");
                        stdout_done = true;
                    }
                }
            }

            // Read stderr.
            result = async {
                match stderr.as_mut() {
                    Some(s) => s.read(&mut stderr_buf).await,
                    None => std::future::pending().await,
                }
            }, if !stderr_done => {
                match result {
                    Ok(0) => { stderr_done = true; }
                    Ok(n) => {
                        let chunk = &stderr_buf[..n];
                        ring.lock().unwrap().push(chunk);
                        write_log(&log_file, chunk).await;
                        emit_output(&sys, job_id, OutputStream::Stderr, chunk).await;
                    }
                    Err(e) => {
                        debug!(%job_id, err = %e, "process_mgr: stderr read error");
                        stderr_done = true;
                    }
                }
            }
        }

        // If both streams are closed, wait for the child to exit.
        if stdout_done && stderr_done {
            break;
        }
    }

    // FIX 3: wait for exit status while still honouring late kill requests.
    let (exit_code, was_killed) = tokio::select! {
        status = child.wait() => {
            let code = match status {
                Ok(s) => s.code().unwrap_or(-1),
                Err(e) => {
                    error!(%job_id, err = %e, "process_mgr: wait failed");
                    -1
                }
            };
            (code, false)
        }
        _ = kill_rx.recv() => {
            child.start_kill().ok();
            let code = match child.wait().await {
                Ok(s) => s.code().unwrap_or(-1),
                Err(_) => -1,
            };
            (code, true)
        }
    };

    let ring_len = ring.lock().unwrap().len();
    info!(%job_id, exit_code, bytes = ring_len, "process_mgr: child exited");

    // Emit OutputEof.
    let _ = sys
        .event_bus
        .send(EventBusMsg::Publish {
            payload: EventPayload::OutputEof {
                id: job_id.to_string(),
            },
            channel: format!("output:{job_id}"),
        })
        .await;

    if was_killed {
        emit_state_change(&sys, job_id, JobStatus::Running, JobStatus::Killed).await;
        let _ = sys
            .scheduler
            .send(SchedulerMsg::JobFinished {
                job_id,
                exit_code: -1,
            })
            .await;
    } else {
        // Determine final state.
        let new_state = if exit_code == 0 {
            JobStatus::Done
        } else {
            JobStatus::Failed
        };

        emit_state_change(&sys, job_id, JobStatus::Running, new_state).await;

        let _ = sys
            .scheduler
            .send(SchedulerMsg::JobFinished { job_id, exit_code })
            .await;
    }

    // FIX 2: tell the main loop to remove our entry.
    let _ = cleanup_tx.send(job_id).await;
}

/// Emit a `JobStateChanged` event.
async fn emit_state_change(
    sys: &ActorSystem,
    job_id: JobId,
    old_state: JobStatus,
    new_state: JobStatus,
) {
    let _ = sys
        .event_bus
        .send(EventBusMsg::Publish {
            payload: EventPayload::JobStateChanged {
                job_id: job_id.to_string(),
                old_state,
                new_state,
            },
            channel: "jobs".into(),
        })
        .await;
}

/// Emit an `OutputChunk` event.
async fn emit_output(sys: &ActorSystem, job_id: JobId, stream: OutputStream, data: &[u8]) {
    let text = String::from_utf8_lossy(data).into_owned();
    let _ = sys
        .event_bus
        .send(EventBusMsg::Publish {
            payload: EventPayload::OutputChunk {
                id: job_id.to_string(),
                stream,
                data: text,
            },
            channel: format!("output:{job_id}"),
        })
        .await;
}

/// Write a chunk to the log file (best-effort).
///
/// FIX 5: offloaded to the blocking thread-pool so the async reader task
/// never stalls the tokio runtime with synchronous I/O.
async fn write_log(file: &Arc<Mutex<Option<std::fs::File>>>, data: &[u8]) {
    let file = file.clone();
    let data = data.to_vec();
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(mut guard) = file.lock()
            && let Some(f) = guard.as_mut()
        {
            let _ = f.write_all(&data);
        }
    })
    .await;
}
