//! cued — background daemon entry point.
//!
//! Subcommands:
//!   `cued start [--fg] [--socket PATH]` — start the daemon
//!   `cued stop`                         — send Shutdown to a running daemon
//!   `cued status`                       — check if daemon is running

use std::path::PathBuf;
use std::process;

use anyhow::{Context, Result};
use bpaf::Parser as _;
use tokio::signal;
use tracing::{error, info};

// ── CLI definition (combinator API, no derive feature needed) ──

#[derive(Debug, Clone)]
enum Cli {
    Start {
        #[allow(dead_code)]
        fg: bool,
        socket: Option<PathBuf>,
    },
    Stop {
        socket: Option<PathBuf>,
    },
    Status {
        socket: Option<PathBuf>,
    },
}

fn socket_arg() -> impl bpaf::Parser<Option<PathBuf>> {
    bpaf::long("socket")
        .help("Override socket path")
        .argument::<PathBuf>("PATH")
        .optional()
}

fn start_cmd() -> impl bpaf::Parser<Cli> {
    let fg = bpaf::short('f')
        .long("fg")
        .help("Run in foreground")
        .switch();
    let socket = socket_arg();
    bpaf::construct!(Cli::Start { fg, socket })
        .to_options()
        .command("start")
        .help("Start the daemon")
}

fn stop_cmd() -> impl bpaf::Parser<Cli> {
    let socket = socket_arg();
    bpaf::construct!(Cli::Stop { socket })
        .to_options()
        .command("stop")
        .help("Stop a running daemon")
}

fn status_cmd() -> impl bpaf::Parser<Cli> {
    let socket = socket_arg();
    bpaf::construct!(Cli::Status { socket })
        .to_options()
        .command("status")
        .help("Check daemon status")
}

fn cli() -> bpaf::OptionParser<Cli> {
    let parser = bpaf::construct!([start_cmd(), stop_cmd(), status_cmd()]);
    parser
        .to_options()
        .version(env!("CARGO_PKG_VERSION"))
        .descr("cued — background daemon for cue-shell")
}

fn main() {
    let cmd = cli().run();
    let result = match cmd {
        Cli::Start { fg: _, socket } => run_start(socket),
        Cli::Stop { socket } => run_stop(socket),
        Cli::Status { socket } => run_status(socket),
    };
    if let Err(e) = result {
        eprintln!("cued: {e:#}");
        process::exit(1);
    }
}

// ── Start ──

fn run_start(socket_override: Option<PathBuf>) -> Result<()> {
    // Initialize tracing (stderr, env-filter).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let pid_path = cued::dirs::pid_path();
    let socket_path = socket_override.unwrap_or_else(cued::dirs::socket_path);

    // Check PID file — refuse to start if already running.
    if pid_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&pid_path)
            && let Ok(pid) = content.trim().parse::<u32>()
            && is_process_alive(pid)
        {
            anyhow::bail!(
                "cued already running (pid {pid}). \
                 If stale, remove {} and retry.",
                pid_path.display()
            );
        }
        // Stale PID file — remove it.
        std::fs::remove_file(&pid_path).ok();
    }

    // Ensure directories exist.
    cued::dirs::ensure_dirs().context("create directories")?;

    // Write PID file.
    std::fs::write(&pid_path, format!("{}", process::id()))
        .with_context(|| format!("write PID file {}", pid_path.display()))?;

    info!(
        version = cue_core::version(),
        pid = process::id(),
        socket = %socket_path.display(),
        "cued starting"
    );

    // Build Tokio runtime and run the async entry point.
    let rt = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let result = rt.block_on(async_main(socket_path.clone()));

    // Cleanup.
    cleanup(&pid_path, &socket_path);
    result
}

async fn async_main(socket_path: PathBuf) -> Result<()> {
    // Open database.
    let db_path = cued::dirs::db_path();
    let db = cued::storage::open_db(&db_path)
        .with_context(|| format!("open database {}", db_path.display()))?;

    // Spawn all actors.
    let sys = cued::actor::spawn_all(socket_path, db).await?;

    info!("cued ready — waiting for signals");

    // Wait for SIGTERM or SIGINT.
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())?;

    tokio::select! {
        _ = sigterm.recv() => info!("received SIGTERM"),
        _ = sigint.recv()  => info!("received SIGINT"),
    }

    // Graceful shutdown.
    info!("cued shutting down");
    sys.shutdown().await;

    // Give actors a moment to drain.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    info!("cued stopped");
    Ok(())
}

fn cleanup(pid_path: &PathBuf, socket_path: &PathBuf) {
    std::fs::remove_file(pid_path).ok();
    std::fs::remove_file(socket_path).ok();
}

// ── Stop ──

fn run_stop(socket_override: Option<PathBuf>) -> Result<()> {
    let socket_path = socket_override.unwrap_or_else(cued::dirs::socket_path);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let mut stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .with_context(|| format!("connect to {}", socket_path.display()))?;

        let msg = cue_core::ipc::Message::Request {
            id: 0,
            payload: cue_core::ipc::RequestPayload::Shutdown {},
        };
        cued::actor::gateway::write_message(&mut stream, &msg).await?;

        // Read ack.
        match cued::actor::gateway::read_message(&mut stream).await {
            Ok(cue_core::ipc::Message::Response { payload, .. }) => match payload {
                cue_core::ipc::ResponsePayload::Ok(_) => {
                    println!("cued: shutdown acknowledged");
                }
                cue_core::ipc::ResponsePayload::Err { message, .. } => {
                    error!(%message, "cued: shutdown error");
                }
            },
            Ok(_) => println!("cued: unexpected response"),
            Err(e) => {
                // Connection might close before we read — that's OK.
                println!("cued: connection closed ({e}) — daemon likely stopped");
            }
        }
        Ok(())
    })
}

// ── Status ──

fn run_status(socket_override: Option<PathBuf>) -> Result<()> {
    let pid_path = cued::dirs::pid_path();
    let socket_path = socket_override.unwrap_or_else(cued::dirs::socket_path);

    // Check PID file.
    if pid_path.exists()
        && let Ok(content) = std::fs::read_to_string(&pid_path)
        && let Ok(pid) = content.trim().parse::<u32>()
    {
        if is_process_alive(pid) {
            println!(
                "cued is running (pid {pid}, socket {})",
                socket_path.display()
            );
            return Ok(());
        }
        println!("cued: stale PID file (pid {pid} not running)");
        return Ok(());
    }

    println!("cued is not running");
    Ok(())
}

// ── Helpers ──

/// Check if a process is alive using `kill(pid, 0)`.
fn is_process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 doesn't send a signal, just checks existence.
    unsafe { libc_kill(pid as i32, 0) == 0 }
}

unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe { libc_kill_ffi(pid, sig) }
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill_ffi(pid: i32, sig: i32) -> i32;
}
