//! Event loop — merges terminal events, socket messages, and a tick timer
//! into a single stream of [`AppMsg`].
//!
//! Uses a dedicated blocking thread for crossterm event reading (avoids
//! needing the `event-stream` feature) plus `tokio::select!` for the
//! async sources.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event as CtEvent};
use tokio::sync::mpsc;

use crate::app::AppMsg;
use crate::client::ClientReader;
use cue_core::ipc::Message;

/// Spawn the event-producing tasks and return a receiver of [`AppMsg`].
///
/// Three sources feed the channel:
/// 1. **Terminal events** — crossterm key/mouse/resize (blocking thread)
/// 2. **Socket messages** — daemon responses and pushed events (async task)
/// 3. **Tick timer** — periodic refresh for the status bar clock (async task)
pub fn spawn_event_loop(
    socket_reader: Option<ClientReader>,
) -> Result<mpsc::UnboundedReceiver<AppMsg>> {
    let (tx, rx) = mpsc::unbounded_channel();

    // ── 1. Terminal events (blocking thread) ──
    let tx_term = tx.clone();
    std::thread::Builder::new()
        .name("tui-events".into())
        .spawn(move || {
            loop {
                // Poll with 100ms timeout to allow thread shutdown on channel close.
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            let msg = match ev {
                                CtEvent::Key(key) => AppMsg::KeyEvent(key),
                                CtEvent::Mouse(mouse) => AppMsg::MouseEvent(mouse),
                                CtEvent::Resize(w, h) => AppMsg::Resize(w, h),
                                _ => continue,
                            };
                            if tx_term.send(msg).is_err() {
                                break; // Receiver dropped, exit thread.
                            }
                        }
                        Err(_) => break,
                    },
                    Ok(false) => continue, // timeout, loop again
                    Err(_) => break,
                }
            }
        })?;

    // ── 2. Socket reader (async task) ──
    if let Some(mut reader) = socket_reader {
        let tx_sock = tx.clone();
        tokio::spawn(async move {
            loop {
                match reader.recv().await {
                    Ok(msg) => {
                        let app_msg = match msg {
                            Message::Response { id, payload } => AppMsg::Response { id, payload },
                            Message::Event { payload } => AppMsg::ServerEvent(payload),
                            Message::Request { .. } => continue, // Unexpected from server.
                        };
                        if tx_sock.send(app_msg).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = tx_sock.send(AppMsg::Disconnected);
                        break;
                    }
                }
            }
        });
    }

    // ── 3. Tick timer ──
    let tx_tick = tx;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            if tx_tick.send(AppMsg::Tick).is_err() {
                break;
            }
        }
    });

    Ok(rx)
}
