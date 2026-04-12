//! TUI frontend for cue-shell.
//!
//! Architecture: TEA (The Elm Architecture) + Component hybrid.
//! - Global [`AppState`] + [`AppMsg`] enum + pure `update` function
//! - Panels rendered by independent [`Component`] implementors
//! - ratatui 0.30 + crossterm 0.29

pub mod app;
pub mod client;
pub mod component;
pub mod event;
pub mod ui;

pub use app::{AppMsg, AppState, FocusArea};
pub use client::CuedClient;

use anyhow::{Context, Result};

/// Run the TUI application.
///
/// This is the main entry point called by `cue-cli`.  It:
/// 1. Connects to the daemon (if available)
/// 2. Initializes the terminal
/// 3. Runs the event loop
/// 4. Restores the terminal on exit
pub async fn run(socket_path: &std::path::Path) -> Result<()> {
    // Try to connect to cued.
    let client_result = CuedClient::connect(socket_path).await;

    // Split client into reader/writer handle if connected.
    let (socket_reader, writer_handle, connected) = match client_result {
        Ok(c) => {
            let (reader, writer) = c.into_split();
            (Some(reader), Some(client::spawn_writer_task(writer)), true)
        }
        Err(_) => (None, None, false),
    };

    // Initialize terminal.
    let mut terminal = ratatui::init();
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)
        .context("enable mouse capture")?;

    // Install a panic hook that also disables mouse capture.
    // ratatui::init() handles raw mode + alternate screen, but not mouse capture.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
        original_hook(info);
    }));

    // Build app state.
    let mut state = AppState::new();
    let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
    state.terminal_width = w;
    state.terminal_height = h;

    if let Some(wh) = writer_handle {
        state.writer = Some(wh);
        state.connected = connected;
        state
            .status_bar
            .update(component::status_bar::StatusBarMsg::SetConnected(connected));
    }

    // Spawn event loop.
    let mut rx = event::spawn_event_loop(socket_reader)?;

    // Main loop.
    let result = loop {
        // Draw.
        if let Err(e) = terminal.draw(|frame| ui::draw(frame, &state)) {
            break Err(e).context("draw frame");
        }

        // Wait for next event.
        match rx.recv().await {
            Some(msg) => state.update(msg),
            None => break Ok(()), // All senders dropped.
        }

        if state.should_quit {
            break Ok(());
        }
    };

    // Restore terminal.
    crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)
        .context("disable mouse capture")?;
    ratatui::restore();

    result
}

// Need Component trait in scope for .update() calls.
use component::Component as _;
