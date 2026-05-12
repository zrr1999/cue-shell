//! ProcessManager actor — OS child process lifecycle.
//!
//! Spawns real child processes via `tokio::process::Command`, reads their
//! stdout/stderr into a [`RingBuffer`], writes a persistent log file, and
//! publishes output chunks + state-change events.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use cue_core::JobId;
use cue_core::ipc::{EventPayload, Stream as OutputStream};
use cue_core::job::JobStatus;
use cue_core::scope::EnvSnapshot;

use super::{
    ActorSystem, EventBusMsg, GatewayMsg, ProcessMgrMsg, SchedulerMsg, ScopeStoreMsg,
    StderrSnapshot,
};
use crate::ring_buffer::RingBuffer;
use crate::runtime_env::effective_snapshot;
use crate::word_expansion::expand_command_line;

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
    /// Separate stderr ring buffer.  `None` in PTY mode (streams are merged).
    stderr_ring: Option<Arc<Mutex<RingBuffer>>>,
    /// PTY master for job input.
    input: Arc<AsyncFd<std::fs::File>>,
    /// PTY master fd used for resize ioctls.
    resize: Arc<std::fs::File>,
    /// Which client, if any, owns the foreground stream.
    fg_owner: Arc<Mutex<Option<u64>>>,
}

const DEFAULT_PTY_COLS: u16 = 80;
const DEFAULT_PTY_ROWS: u16 = 24;

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
                    pipeline,
                    scope_hash,
                    cwd_override,
                    wrapper_enabled,
                } => {
                    info!(%job_id, segments = pipeline.segments.len(), %scope_hash, "process_mgr: spawn");

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

                    let effective_snapshot = snapshot.as_ref().map(effective_snapshot);

                    // Resolve effective cwd: explicit override wins, else scope cwd.
                    let effective_cwd = cwd_override.as_ref().or_else(|| {
                        effective_snapshot.as_ref().map(|s| &s.cwd)
                    });
                    if let Some(cwd) = effective_cwd
                        && !cwd.is_dir()
                    {
                        error!(
                            %job_id,
                            cwd = %cwd.display(),
                            "process_mgr: invalid cwd for job spawn"
                        );
                        emit_state_change(&sys, job_id, JobStatus::Pending, JobStatus::Failed)
                            .await;
                        let _ = sys
                            .scheduler
                            .send(SchedulerMsg::JobFinished {
                                job_id,
                                exit_code: -1,
                            })
                            .await;
                        continue;
                    }

                    clear_output_log(job_id).await;

                    // ── Build expanded commands for each segment ──
                    let mut segment_commands: Vec<(String, Vec<String>)> = Vec::new();
                    for segment in &pipeline.segments {
                        let expanded = expand_command_line(&segment.command, effective_snapshot.as_ref());
                        let Some(prog) = expanded.first().filter(|p| !p.is_empty()) else {
                            error!(%job_id, cmd = ?segment.command, "process_mgr: segment command is empty");
                            break;
                        };
                        // Apply wrapper prefix per-segment.
                        let (program, args) = if wrapper_enabled {
                            let wbin = &sys.config.wrapper.binary;
                            if !wbin.is_empty()
                                && sys.config.wrapper.should_wrap(prog, false, Some(true))
                            {
                                let mut wargs = Vec::with_capacity(expanded.len());
                                wargs.push(prog.clone());
                                wargs.extend_from_slice(&expanded[1..]);
                                (wbin.clone(), wargs)
                            } else {
                                (prog.clone(), expanded[1..].to_vec())
                            }
                        } else {
                            (prog.clone(), expanded[1..].to_vec())
                        };
                        segment_commands.push((program, args));
                    }

                    if segment_commands.is_empty() {
                        error!(%job_id, "process_mgr: all segment commands empty");
                        continue;
                    }

                    // ── Single segment: spawn via PTY (unchanged path) ──
                    if segment_commands.len() == 1 {
                        let (program, args) = segment_commands.into_iter().next().unwrap();
                        let mut cmd = tokio::process::Command::new(&program);
                        if !args.is_empty() {
                            cmd.args(&args);
                        }
                        if let Some(snap) = effective_snapshot {
                            apply_env(&mut cmd, &snap);
                        }
                        if let Some(ref cwd) = cwd_override {
                            cmd.current_dir(cwd);
                        }

                        let pty_pair = match crate::pty::open_pty() {
                            Ok(pair) => pair,
                            Err(error) => {
                                error!(%job_id, err = %error, "process_mgr: open pty failed");
                                emit_state_change(&sys, job_id, JobStatus::Pending, JobStatus::Failed)
                                    .await;
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
                        let master_file = std::fs::File::from(pty_pair.master);
                        let slave = pty_pair.slave;
                    if let Err(error) = set_nonblocking(master_file.as_raw_fd()) {
                        error!(%job_id, err = %error, "process_mgr: set pty nonblocking failed");
                        emit_state_change(&sys, job_id, JobStatus::Pending, JobStatus::Failed)
                            .await;
                        let _ = sys
                            .scheduler
                            .send(SchedulerMsg::JobFinished {
                                job_id,
                                exit_code: -1,
                            })
                            .await;
                        continue;
                    }
                    if let Err(error) =
                        set_winsize(slave.as_raw_fd(), DEFAULT_PTY_COLS, DEFAULT_PTY_ROWS)
                    {
                        warn!(%job_id, err = %error, "process_mgr: set initial pty size failed");
                    }
                    let reader_file = match master_file.try_clone() {
                        Ok(file) => file,
                        Err(error) => {
                            error!(%job_id, err = %error, "process_mgr: clone pty reader failed");
                            emit_state_change(&sys, job_id, JobStatus::Pending, JobStatus::Failed)
                                .await;
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
                    let input_file = match master_file.try_clone() {
                        Ok(file) => file,
                        Err(error) => {
                            error!(%job_id, err = %error, "process_mgr: clone pty input failed");
                            emit_state_change(&sys, job_id, JobStatus::Pending, JobStatus::Failed)
                                .await;
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
                    let resize_file = match master_file.try_clone() {
                        Ok(file) => Arc::new(file),
                        Err(error) => {
                            error!(%job_id, err = %error, "process_mgr: clone pty resize failed");
                            emit_state_change(&sys, job_id, JobStatus::Pending, JobStatus::Failed)
                                .await;
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
                    let slave_fd = slave.as_raw_fd();
                    let master_fd = master_file.as_raw_fd();
                    // SAFETY: the child process is single-threaded after fork here; the closure
                    // only performs async-signal-safe libc calls on valid inherited fds.
                    unsafe {
                        cmd.pre_exec(move || {
                            if libc::setsid() == -1 {
                                return Err(std::io::Error::last_os_error());
                            }
                            if libc::ioctl(slave_fd, libc::TIOCSCTTY.into(), 0) == -1 {
                                return Err(std::io::Error::last_os_error());
                            }
                            for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                                if libc::dup2(slave_fd, target) == -1 {
                                    return Err(std::io::Error::last_os_error());
                                }
                            }
                            if slave_fd > libc::STDERR_FILENO {
                                libc::close(slave_fd);
                            }
                            if master_fd > libc::STDERR_FILENO {
                                libc::close(master_fd);
                            }
                            Ok(())
                        });
                    }
                    cmd.stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .kill_on_drop(true);

                    // 3. Spawn the child process.
                    let mut child = match cmd.spawn() {
                        Ok(c) => c,
                        Err(e) => {
                            error!(
                                %job_id,
                                program = %program,
                                expanded_cmd = ?pipeline.segments.first().map(|s| &s.command),
                                cwd = %snapshot
                                    .as_ref()
                                    .map(|snap| snap.cwd.display().to_string())
                                    .unwrap_or_else(|| "<daemon cwd>".into()),
                                path = ?snapshot
                                    .as_ref()
                                    .and_then(|snap| snap.env.get("PATH").cloned()),
                                err = %e,
                                "process_mgr: spawn failed"
                            );
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
                    drop(slave);
                    drop(master_file);

                    let pid = child.id().unwrap_or(0);
                    info!(%job_id, pid, "process_mgr: child spawned");

                    // 4. Emit Pending → Running.
                    emit_state_change(&sys, job_id, JobStatus::Pending, JobStatus::Running).await;

                    // 5. Open log file (FIX 5: offloaded to blocking thread).
                    let log_file = open_log_file(job_id).await;
                    let input = match AsyncFd::new(input_file) {
                        Ok(file) => Arc::new(file),
                        Err(error) => {
                            error!(%job_id, err = %error, "process_mgr: async pty input failed");
                            let _ = child.start_kill();
                            let _ = child.wait().await;
                            emit_state_change(&sys, job_id, JobStatus::Running, JobStatus::Failed)
                                .await;
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
                    let reader = match AsyncFd::new(reader_file) {
                        Ok(file) => file,
                        Err(error) => {
                            error!(%job_id, err = %error, "process_mgr: async pty reader failed");
                            let _ = child.start_kill();
                            let _ = child.wait().await;
                            emit_state_change(&sys, job_id, JobStatus::Running, JobStatus::Failed)
                                .await;
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

                    // 7. Spawn reader/waiter background task.
                    let (kill_tx, kill_rx) = mpsc::channel::<()>(1);
                    // FIX 7: shared ring buffer accessible from ProcessEntry.
                    let ring_buffer = Arc::new(Mutex::new(RingBuffer::default()));
                    let ring_clone = ring_buffer.clone();
                    let fg_owner = Arc::new(Mutex::new(None));
                    let fg_owner_clone = fg_owner.clone();
                    let sys_clone = sys.clone();
                    let cleanup_tx_clone = cleanup_tx.clone();
                    let reader_handle = tokio::spawn(reader_task(
                        job_id, child, reader, log_file, kill_rx, sys_clone,
                        ring_clone, fg_owner_clone, cleanup_tx_clone,
                    ));

                    children.insert(
                        job_id.0,
                        ProcessEntry {
                            job_id,
                            status: JobStatus::Running,
                            _reader_handle: reader_handle,
                            kill_tx,
                            ring_buffer,
                            stderr_ring: None,
                            input,
                            resize: resize_file,
                            fg_owner,
                        },
                    );
                    } else {
                        // ── Multi-segment: native pipe spawn ──
                        spawn_native_pipeline(
                            job_id,
                            segment_commands,
                            pipeline,
                            effective_snapshot.as_ref(),
                            effective_cwd,
                            cwd_override.as_ref(),
                            &sys,
                            &mut children,
                            &cleanup_tx,
                        )
                        .await;
                    }
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

                ProcessMgrMsg::GetStderr { job_id, tail_bytes, reply } => {
                    let result = children.get(&job_id.0).map(|entry| match &entry.stderr_ring {
                        Some(ring) => StderrSnapshot {
                            pty_merged: false,
                            data: ring.lock().unwrap().tail(tail_bytes),
                        },
                        None => StderrSnapshot {
                            pty_merged: true,
                            data: Vec::new(),
                        },
                    });
                    let _ = reply.send(result);
                }

                ProcessMgrMsg::SendJobInput { job_id, data, reply } => {
                    let input = children.get(&job_id.0).map(|entry| entry.input.clone());
                    let handled = if let Some(input) = input {
                        match write_pty(&input, &data).await {
                            Ok(()) => Ok(()),
                            Err(error) => Err(format!("failed to write job input: {error}")),
                        }
                    } else {
                        Err(format!("job {job_id} does not accept stdin"))
                    };
                    let _ = reply.send(handled);
                }

                ProcessMgrMsg::AttachFg { client_id, job_id, reply } => {
                    let (result, snapshot) = if let Some(entry) = children.get_mut(&job_id.0) {
                        if entry.status != JobStatus::Running {
                            (Err(format!("job {job_id} is not running")), None)
                        } else if let Some(owner) = *entry.fg_owner.lock().unwrap()
                            && owner != client_id
                        {
                            (Err(format!("job {job_id} is already foreground-attached")), None)
                        } else {
                            *entry.fg_owner.lock().unwrap() = Some(client_id);
                            (
                                Ok(()),
                                Some(
                                    entry
                                        .ring_buffer
                                        .lock()
                                        .unwrap()
                                        .tail(crate::ring_buffer::DEFAULT_CAPACITY),
                                ),
                            )
                        }
                    } else {
                        (Err(format!("job {job_id} not found")), None)
                    };
                    let attached = result.is_ok();
                    let _ = reply.send(result);
                    if attached
                        && let Some(snapshot) = snapshot
                        && !snapshot.is_empty()
                    {
                        let _ = sys
                            .gateway
                            .send(GatewayMsg::SendEvent {
                                client_id,
                                payload: EventPayload::FgOutput { data: snapshot },
                            })
                            .await;
                    }
                }

                ProcessMgrMsg::DetachFg { client_id, reason } => {
                    let mut detached_jobs = Vec::new();
                    for entry in children.values_mut() {
                        if *entry.fg_owner.lock().unwrap() == Some(client_id) {
                            *entry.fg_owner.lock().unwrap() = None;
                            detached_jobs.push(entry.job_id.to_string());
                        }
                    }
                    for job_id in detached_jobs {
                        let _ = sys
                            .gateway
                            .send(GatewayMsg::SendEvent {
                                client_id,
                                payload: EventPayload::FgExited {
                                    id: job_id,
                                    reason: reason.clone(),
                                },
                            })
                            .await;
                    }
                }

                ProcessMgrMsg::FgInput { client_id, data, reply } => {
                    let input = children
                        .values()
                        .find(|entry| *entry.fg_owner.lock().unwrap() == Some(client_id))
                        .map(|entry| entry.input.clone());
                    let handled = if let Some(input) = input {
                        match write_pty(&input, &data).await {
                            Ok(()) => Ok(()),
                            Err(error) => Err(format!("failed to write fg input: {error}")),
                        }
                    } else {
                        Err("no foreground session attached".to_string())
                    };
                    let _ = reply.send(handled);
                }

                ProcessMgrMsg::FgResize { client_id, cols, rows, reply } => {
                    let resize = children
                        .values()
                        .find(|entry| *entry.fg_owner.lock().unwrap() == Some(client_id))
                        .map(|entry| entry.resize.clone());
                    let _ = reply.send(if let Some(resize) = resize {
                        set_winsize(resize.as_raw_fd(), cols, rows)
                            .map_err(|error| format!("failed to resize fg session: {error}"))
                    } else {
                        Err("no foreground session attached".into())
                    });
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

fn set_nonblocking(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl operates on a valid fd owned by this process.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fcntl operates on a valid fd owned by this process.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn set_winsize(fd: std::os::fd::RawFd, cols: u16, rows: u16) -> std::io::Result<()> {
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: ioctl operates on a valid tty/pty fd and a properly initialized winsize.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &winsize) };
    if rc == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

async fn read_pty(fd: &AsyncFd<std::fs::File>, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let mut guard = fd.readable().await?;
        match guard.try_io(|inner| inner.get_ref().read(buf)) {
            Ok(result) => return result,
            Err(_would_block) => continue,
        }
    }
}

async fn write_pty(fd: &AsyncFd<std::fs::File>, data: &[u8]) -> std::io::Result<()> {
    let mut written = 0;
    while written < data.len() {
        let mut guard = fd.writable().await?;
        match guard.try_io(|inner| inner.get_ref().write(&data[written..])) {
            Ok(Ok(0)) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "pty write returned 0 bytes",
                ));
            }
            Ok(Ok(n)) => written += n,
            Ok(Err(error)) => return Err(error),
            Err(_would_block) => continue,
        }
    }
    Ok(())
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

async fn clear_output_log(job_id: JobId) {
    let _ = tokio::task::spawn_blocking(move || {
        let path = crate::dirs::output_dir().join(format!("{job_id}.log"));
        if let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(
                %job_id,
                path = %path.display(),
                err = %error,
                "process_mgr: failed to remove stale output log"
            );
        }
    })
    .await;
}

/// Spawn a multi-segment pipeline natively via pipe(2).
///
/// Segments 0..N-2 get their stdout connected to the next segment's stdin
/// via a Unix pipe.  Only the last segment gets a PTY for interactive
/// output.  Each intermediate segment captures stderr separately.
#[allow(clippy::too_many_arguments)]
async fn spawn_native_pipeline(
    job_id: JobId,
    segment_commands: Vec<(String, Vec<String>)>,
    pipeline: cue_core::pipeline::Pipeline,
    effective_snapshot: Option<&EnvSnapshot>,
    _effective_cwd: Option<&std::path::PathBuf>,
    cwd_override: Option<&std::path::PathBuf>,
    sys: &ActorSystem,
    children: &mut HashMap<u32, ProcessEntry>,
    cleanup_tx: &mpsc::Sender<JobId>,
) {
    use std::os::fd::FromRawFd;

    let n = segment_commands.len();
    info!(%job_id, segments = n, "process_mgr: spawning native pipeline");

    // 1. Open PTY for the last segment.
    let pty_pair = match crate::pty::open_pty() {
        Ok(pair) => pair,
        Err(error) => {
            error!(%job_id, err = %error, "process_mgr: open pty for pipeline failed");
            emit_state_change(sys, job_id, JobStatus::Pending, JobStatus::Failed).await;
            let _ = sys
                .scheduler
                .send(SchedulerMsg::JobFinished {
                    job_id,
                    exit_code: -1,
                })
                .await;
            return;
        }
    };
    let master_file = std::fs::File::from(pty_pair.master);
    let last_slave = pty_pair.slave;
    if let Err(error) = set_nonblocking(master_file.as_raw_fd()) {
        error!(%job_id, err = %error, "process_mgr: set pty nonblocking failed");
        emit_state_change(sys, job_id, JobStatus::Pending, JobStatus::Failed).await;
        let _ = sys
            .scheduler
            .send(SchedulerMsg::JobFinished {
                job_id,
                exit_code: -1,
            })
            .await;
        return;
    }
    if let Err(error) = set_winsize(last_slave.as_raw_fd(), DEFAULT_PTY_COLS, DEFAULT_PTY_ROWS) {
        warn!(%job_id, err = %error, "process_mgr: set initial pty size failed");
    }

    // 2. Build pipes between segments.
    let mut pipes: Vec<(std::fs::File, std::fs::File)> = Vec::new(); // (read_end, write_end)
    for _ in 0..n.saturating_sub(1) {
        let mut fds = [-1i32; 2];
        // SAFETY: libc::pipe fills fds with two file descriptors.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
            let error = std::io::Error::last_os_error();
            error!(%job_id, err = %error, "process_mgr: pipe() failed");
            emit_state_change(sys, job_id, JobStatus::Pending, JobStatus::Failed).await;
            let _ = sys
                .scheduler
                .send(SchedulerMsg::JobFinished {
                    job_id,
                    exit_code: -1,
                })
                .await;
            return;
        } else {
            // SAFETY: pipe() returns valid fds.
            let read_end = unsafe { std::fs::File::from_raw_fd(fds[0]) };
            let write_end = unsafe { std::fs::File::from_raw_fd(fds[1]) };
            pipes.push((read_end, write_end));
        }
    }

    // 3. Spawn each segment.
    let mut spawned_children: Vec<tokio::process::Child> = Vec::with_capacity(n);
    let mut stderr_buffers: Vec<Arc<Mutex<RingBuffer>>> = Vec::new();

    for (i, (program, args)) in segment_commands.iter().enumerate() {
        let is_last = i == n - 1;
        let _pipe_to_next = pipeline.segments.get(i).and_then(|s| s.pipe_to_next);

        let mut cmd = tokio::process::Command::new(program);
        if !args.is_empty() {
            cmd.args(args);
        }
        if let Some(snap) = effective_snapshot {
            apply_env(&mut cmd, snap);
        }
        if let Some(ref cwd) = cwd_override {
            cmd.current_dir(cwd);
        }

        // stdin: previous segment's pipe read end (or null for first segment).
        if i > 0 {
            let stdin_fd = pipes[i - 1].0.as_raw_fd();
            // SAFETY: pipe fd is valid; the child will inherit it.
            unsafe {
                cmd.stdin(Stdio::from_raw_fd(stdin_fd));
            }
        } else {
            cmd.stdin(Stdio::null());
        }

        if is_last {
            // Last segment: stdout and stderr → PTY slave.
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            let slave_fd = last_slave.as_raw_fd();
            let master_fd = master_file.as_raw_fd();
            // SAFETY: the child process is single-threaded after fork.
            unsafe {
                cmd.pre_exec(move || {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    #[cfg(target_os = "macos")]
                    let tiocsctty = libc::TIOCSCTTY.into();
                    #[cfg(not(target_os = "macos"))]
                    let tiocsctty = libc::TIOCSCTTY;
                    if libc::ioctl(slave_fd, tiocsctty, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                        if libc::dup2(slave_fd, target) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    if slave_fd > libc::STDERR_FILENO {
                        libc::close(slave_fd);
                    }
                    if master_fd > libc::STDERR_FILENO {
                        libc::close(master_fd);
                    }
                    Ok(())
                });
            }
            cmd.kill_on_drop(true);

            stderr_buffers.push(Arc::new(Mutex::new(RingBuffer::default())));

            let child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    error!(%job_id, seg = i, program = %program, err = %e, "process_mgr: pipeline segment spawn failed");
                    // Kill already-spawned children.
                    for mut prev in spawned_children {
                        let _ = prev.start_kill();
                    }
                    emit_state_change(sys, job_id, JobStatus::Pending, JobStatus::Failed).await;
                    let _ = sys
                        .scheduler
                        .send(SchedulerMsg::JobFinished {
                            job_id,
                            exit_code: -1,
                        })
                        .await;
                    return;
                }
            };
            let pid = child.id().unwrap_or(0);
            info!(%job_id, pid, seg = i, "process_mgr: pipeline last segment spawned");
            spawned_children.push(child);
        } else {
            // Intermediate segment: stdout → pipe write end, stderr → ring buffer.
            let pipe_write_fd = pipes[i].1.as_raw_fd();
            // SAFETY: pipe fd is valid; the child will inherit it.
            unsafe {
                cmd.stdout(Stdio::from_raw_fd(pipe_write_fd));
            }
            cmd.stderr(Stdio::piped());
            cmd.kill_on_drop(true);

            // If this segment pipes stderr-only, we need to adjust.
            // For now, stderr goes to a ring buffer for inspection.
            let stderr_buf = Arc::new(Mutex::new(RingBuffer::default()));
            stderr_buffers.push(stderr_buf.clone());

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    error!(%job_id, seg = i, program = %program, err = %e, "process_mgr: pipeline segment spawn failed");
                    for mut prev in spawned_children {
                        let _ = prev.start_kill();
                    }
                    emit_state_change(sys, job_id, JobStatus::Pending, JobStatus::Failed).await;
                    let _ = sys
                        .scheduler
                        .send(SchedulerMsg::JobFinished {
                            job_id,
                            exit_code: -1,
                        })
                        .await;
                    return;
                }
            };
            let pid = child.id().unwrap_or(0);
            info!(%job_id, pid, seg = i, "process_mgr: pipeline intermediate segment spawned");

            // Spawn a task to drain stderr.
            let seg_stderr_buf = stderr_buf.clone();
            if let Some(stderr_reader) = child.stderr.take() {
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut reader = stderr_reader;
                    let mut buf = vec![0u8; 8192];
                    loop {
                        match reader.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                seg_stderr_buf.lock().unwrap().push(&buf[..n]);
                            }
                            Err(_) => break,
                        }
                    }
                });
            }

            spawned_children.push(child);
        }
    }

    // Close pipe ends in the parent (children have their copies).
    drop(pipes);

    // 4. Set up PTY reader for last segment output.
    let reader_file = match master_file.try_clone() {
        Ok(file) => file,
        Err(error) => {
            error!(%job_id, err = %error, "process_mgr: clone pty reader failed");
            for mut child in spawned_children {
                let _ = child.start_kill();
            }
            emit_state_change(sys, job_id, JobStatus::Pending, JobStatus::Failed).await;
            let _ = sys
                .scheduler
                .send(SchedulerMsg::JobFinished {
                    job_id,
                    exit_code: -1,
                })
                .await;
            return;
        }
    };
    let input_file = match master_file.try_clone() {
        Ok(file) => file,
        Err(error) => {
            error!(%job_id, err = %error, "process_mgr: clone pty input failed");
            for mut child in spawned_children {
                let _ = child.start_kill();
            }
            emit_state_change(sys, job_id, JobStatus::Pending, JobStatus::Failed).await;
            let _ = sys
                .scheduler
                .send(SchedulerMsg::JobFinished {
                    job_id,
                    exit_code: -1,
                })
                .await;
            return;
        }
    };
    let resize_file = match master_file.try_clone() {
        Ok(file) => Arc::new(file),
        Err(error) => {
            error!(%job_id, err = %error, "process_mgr: clone pty resize failed");
            for mut child in spawned_children {
                let _ = child.start_kill();
            }
            emit_state_change(sys, job_id, JobStatus::Pending, JobStatus::Failed).await;
            let _ = sys
                .scheduler
                .send(SchedulerMsg::JobFinished {
                    job_id,
                    exit_code: -1,
                })
                .await;
            return;
        }
    };

    // 5. Emit Pending → Running.
    emit_state_change(sys, job_id, JobStatus::Pending, JobStatus::Running).await;

    // 6. Open log file.
    let log_file = open_log_file(job_id).await;

    let input = match AsyncFd::new(input_file) {
        Ok(file) => Arc::new(file),
        Err(error) => {
            error!(%job_id, err = %error, "process_mgr: async pty input failed");
            for mut child in spawned_children {
                let _ = child.start_kill();
            }
            emit_state_change(sys, job_id, JobStatus::Running, JobStatus::Failed).await;
            let _ = sys
                .scheduler
                .send(SchedulerMsg::JobFinished {
                    job_id,
                    exit_code: -1,
                })
                .await;
            return;
        }
    };
    let reader = match AsyncFd::new(reader_file) {
        Ok(file) => file,
        Err(error) => {
            error!(%job_id, err = %error, "process_mgr: async pty reader failed");
            for mut child in spawned_children {
                let _ = child.start_kill();
            }
            emit_state_change(sys, job_id, JobStatus::Running, JobStatus::Failed).await;
            let _ = sys
                .scheduler
                .send(SchedulerMsg::JobFinished {
                    job_id,
                    exit_code: -1,
                })
                .await;
            return;
        }
    };

    // 7. Spawn pipeline watcher task.
    let (kill_tx, kill_rx) = mpsc::channel::<()>(1);
    let ring_buffer = Arc::new(Mutex::new(RingBuffer::default()));
    let ring_clone = ring_buffer.clone();
    let fg_owner = Arc::new(Mutex::new(None));
    let fg_owner_clone = fg_owner.clone();
    let sys_clone = sys.clone();
    let cleanup_tx_clone = cleanup_tx.clone();
    let reader_handle = tokio::spawn(pipeline_watcher_task(
        job_id,
        spawned_children,
        reader,
        log_file,
        kill_rx,
        sys_clone,
        ring_clone,
        fg_owner_clone,
        cleanup_tx_clone,
        stderr_buffers,
    ));

    children.insert(
        job_id.0,
        ProcessEntry {
            job_id,
            status: JobStatus::Running,
            _reader_handle: reader_handle,
            kill_tx,
            ring_buffer,
            stderr_ring: None,
            input,
            resize: resize_file,
            fg_owner,
        },
    );
}

/// Watcher task for native multi-segment pipelines.
///
/// Reads PTY output from the last segment (same as `reader_task`),
/// waits for all children to exit, and reports the last segment's exit code
/// as the pipeline's exit code.
#[allow(clippy::too_many_arguments)]
async fn pipeline_watcher_task(
    job_id: JobId,
    mut children: Vec<tokio::process::Child>,
    reader: AsyncFd<std::fs::File>,
    log_file: Option<std::fs::File>,
    kill_rx: mpsc::Receiver<()>,
    sys: ActorSystem,
    ring: Arc<Mutex<RingBuffer>>,
    fg_owner: Arc<Mutex<Option<u64>>>,
    cleanup_tx: mpsc::Sender<JobId>,
    _stderr_buffers: Vec<Arc<Mutex<RingBuffer>>>,
) {
    // Use the existing reader_task for the last child's PTY output.
    // Intermediate children are tracked for exit but their output is
    // routed through pipes, not PTY.
    if children.is_empty() {
        return;
    }

    // Take the last child (PTY-connected) for the main reader task.
    let last_child = children.pop().unwrap();

    // Spawn a background task to wait for intermediate children.
    let intermediate_watcher = tokio::spawn(async move {
        for mut child in children {
            let _ = child.wait().await;
        }
    });

    // Use existing reader_task for last child.
    reader_task(
        job_id, last_child, reader, log_file, kill_rx, sys, ring, fg_owner, cleanup_tx,
    )
    .await;

    // Ensure intermediate children are done.
    let _ = intermediate_watcher.await;
}

/// Background task that reads PTY output, populates the ring buffer,
/// writes to the log file, emits events, and waits for the child to exit.
#[allow(clippy::too_many_arguments)]
async fn reader_task(
    job_id: JobId,
    mut child: tokio::process::Child,
    reader: AsyncFd<std::fs::File>,
    log_file: Option<std::fs::File>,
    mut kill_rx: mpsc::Receiver<()>,
    sys: ActorSystem,
    ring: Arc<Mutex<RingBuffer>>,
    fg_owner: Arc<Mutex<Option<u64>>>,
    cleanup_tx: mpsc::Sender<JobId>,
) {
    // FIX 5: wrap the log file so it can be shared with spawn_blocking.
    let log_file = Arc::new(Mutex::new(log_file));
    let mut pty_buf = vec![0u8; 8192];
    let mut pty_done = false;

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
                emit_fg_exit(&sys, &fg_owner, job_id, "killed").await;
                let _ = sys.scheduler.send(SchedulerMsg::JobFinished { job_id, exit_code: -1 }).await;
                // FIX 2: tell the main loop to remove our entry.
                let _ = cleanup_tx.send(job_id).await;
                return;
            }

            result = read_pty(&reader, &mut pty_buf), if !pty_done => {
                match result {
                    Ok(0) => { pty_done = true; }
                    Ok(n) => {
                        let chunk = &pty_buf[..n];
                        ring.lock().unwrap().push(chunk);
                        write_log(&log_file, chunk).await;
                        emit_output(&sys, job_id, OutputStream::Stdout, chunk).await;
                        emit_fg_output(&sys, &fg_owner, chunk).await;
                    }
                    Err(e) => {
                        if e.raw_os_error() == Some(libc::EIO) {
                            pty_done = true;
                        } else {
                            debug!(%job_id, err = %e, "process_mgr: pty read error");
                            pty_done = true;
                        }
                    }
                }
            }
        }

        if pty_done {
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
        emit_fg_exit(&sys, &fg_owner, job_id, "killed").await;
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
        let reason = if exit_code == 0 {
            "done".to_string()
        } else {
            format!("exit {exit_code}")
        };
        emit_fg_exit(&sys, &fg_owner, job_id, &reason).await;

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
                end_scope: None,
                chain_id: None,
                chain_index: None,
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

async fn emit_fg_output(sys: &ActorSystem, fg_owner: &Arc<Mutex<Option<u64>>>, data: &[u8]) {
    let client_id = *fg_owner.lock().unwrap();
    if let Some(client_id) = client_id {
        let _ = sys
            .gateway
            .send(GatewayMsg::SendEvent {
                client_id,
                payload: EventPayload::FgOutput {
                    data: data.to_vec(),
                },
            })
            .await;
    }
}

async fn emit_fg_exit(
    sys: &ActorSystem,
    fg_owner: &Arc<Mutex<Option<u64>>>,
    job_id: JobId,
    reason: &str,
) {
    let client_id = fg_owner.lock().unwrap().take();
    if let Some(client_id) = client_id {
        let _ = sys
            .gateway
            .send(GatewayMsg::SendEvent {
                client_id,
                payload: EventPayload::FgExited {
                    id: job_id.to_string(),
                    reason: reason.to_string(),
                },
            })
            .await;
    }
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;

    fn snapshot() -> EnvSnapshot {
        EnvSnapshot {
            env: BTreeMap::from([
                ("HOME".into(), "/tmp/cue-home".into()),
                ("USER".into(), "tester".into()),
            ]),
            cwd: PathBuf::from("/tmp/work"),
        }
    }

    #[test]
    fn expands_scope_words_for_jobs() {
        let expanded = expand_command_line(
            &[
                "~/bin/tool".into(),
                "~".into(),
                "$HOME".into(),
                "${USER}".into(),
                "prefix-$USER-suffix".into(),
            ],
            Some(&snapshot()),
        );

        assert_eq!(
            expanded,
            vec![
                "/tmp/cue-home/bin/tool",
                "/tmp/cue-home",
                "/tmp/cue-home",
                "tester",
                "prefix-tester-suffix",
            ]
        );
    }

    #[test]
    fn preserves_unsupported_parameter_forms() {
        let expanded = expand_command_line(
            &[
                "echo".into(),
                "${USER:-guest}".into(),
                "${BROKEN".into(),
                "$1".into(),
                "\\$USER".into(),
            ],
            Some(&snapshot()),
        );

        assert_eq!(
            expanded,
            vec!["echo", "${USER:-guest}", "${BROKEN", "$1", "$USER"]
        );
    }
}
