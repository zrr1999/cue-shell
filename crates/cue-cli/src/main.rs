//! `cue` — TUI entry point for cue-shell.
//!
//! 1. Try to connect to the cued daemon socket.
//! 2. If not running, auto-start `cued start` and retry with backoff.
//! 3. Initialize the terminal (crossterm raw mode, alternate screen).
//! 4. Run the TUI event loop.
//! 5. Restore the terminal on exit.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::info;

use cue_tui::client::default_socket_path;

fn main() -> Result<()> {
    // Initialize logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    // Build the tokio runtime.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    rt.block_on(async_main())
}

async fn async_main() -> Result<()> {
    let socket_path = socket_path_from_env();

    // Try to auto-start the daemon if not running.
    ensure_daemon_running(&socket_path).await;

    // Run the TUI (handles connect, terminal setup, event loop, teardown).
    cue_tui::run(&socket_path).await
}

/// Resolve socket path, respecting `CUE_SOCKET` env override.
fn socket_path_from_env() -> PathBuf {
    if let Ok(path) = std::env::var("CUE_SOCKET") {
        PathBuf::from(path)
    } else {
        default_socket_path()
    }
}

/// If the daemon socket doesn't exist, attempt to start `cued start`
/// and wait for it to be ready.
async fn ensure_daemon_running(socket_path: &Path) {
    if socket_path.exists() {
        return;
    }

    info!("cued not running, attempting to start…");
    let cued_bin = std::env::var("CUE_DAEMON_BIN").unwrap_or_else(|_| "cued".into());
    let _child = Command::new(&cued_bin)
        .arg("start")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    // Wait for socket to appear with backoff: 100ms, 200ms, 400ms, 800ms, 1600ms.
    let mut delay = Duration::from_millis(100);
    for _ in 0..5 {
        tokio::time::sleep(delay).await;
        if socket_path.exists() {
            info!("cued socket appeared after auto-start");
            return;
        }
        delay *= 2;
    }

    tracing::warn!("cued did not start in time — will run in offline mode");
}
