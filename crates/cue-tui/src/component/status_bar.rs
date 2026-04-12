//! Status bar component — bottom line showing entity counts and connection status.
//!
//! Layout: `J:3(1🔄) A:1 C:2    cued:ok  14:30`
//! Left-aligned: entity counts.  Right-aligned: connection status + clock.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::Component;
use crate::app::AppMsg;
use crate::component::sidebar::OverviewCounts;

// ── Component messages ──

/// Messages local to the status bar.
pub enum StatusBarMsg {
    /// Update entity counts.
    SetOverview(OverviewCounts),
    /// Update connection state.
    SetConnected(bool),
}

// ── StatusBar ──

pub struct StatusBar {
    /// Aggregate counts.
    pub overview: OverviewCounts,
    /// Whether we are connected to cued.
    pub connected: bool,
}

impl StatusBar {
    pub fn new() -> Self {
        Self {
            overview: OverviewCounts::default(),
            connected: false,
        }
    }
}

impl Default for StatusBar {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for StatusBar {
    type Message = StatusBarMsg;

    fn update(&mut self, msg: StatusBarMsg) {
        match msg {
            StatusBarMsg::SetOverview(counts) => self.overview = counts,
            StatusBarMsg::SetConnected(c) => self.connected = c,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        let ov = &self.overview;
        let running = if ov.jobs_running > 0 {
            format!("({}🔄)", ov.jobs_running)
        } else {
            String::new()
        };
        let left = format!(" J:{}{} A:{} C:{}", ov.jobs, running, ov.agents, ov.crons);

        let conn_status = if self.connected { "cued:ok" } else { "cued:--" };
        let conn_color = if self.connected {
            Color::Green
        } else {
            Color::Red
        };

        // Get current time.
        let now = {
            use std::time::{SystemTime, UNIX_EPOCH};
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            // Simple HH:MM from unix timestamp (local time approximation via libc).
            // For a skeleton, UTC is acceptable.
            let hours = (secs / 3600) % 24;
            let minutes = (secs / 60) % 60;
            format!("{hours:02}:{minutes:02}")
        };

        let right = format!("{conn_status}  {now} ");

        // Pad the middle with spaces.
        let mid_width = (area.width as usize).saturating_sub(left.len() + right.len());
        let padding = " ".repeat(mid_width);

        let line = Line::from(vec![
            Span::styled(&left, Style::default().fg(Color::White)),
            Span::raw(padding),
            Span::styled(conn_status, Style::default().fg(conn_color)),
            Span::raw("  "),
            Span::styled(now, Style::default().fg(Color::DarkGray)),
            Span::raw(" "),
        ]);

        let bar = Paragraph::new(line).style(Style::default().bg(Color::DarkGray).fg(Color::White));
        frame.render_widget(bar, area);
    }

    fn handle_key(&mut self, _key: KeyEvent) -> Option<AppMsg> {
        // Status bar does not consume key events.
        None
    }

    fn handle_mouse(&mut self, _mouse: MouseEvent) -> Option<AppMsg> {
        None
    }
}
