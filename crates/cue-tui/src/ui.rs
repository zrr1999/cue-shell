//! Root render function — lays out all components and draws one frame.
//!
//! ```text
//! ┌──────────┬──────────────────────────┐
//! │ Sidebar  │  Main REPL Area          │
//! │          │                          │
//! ├──────────┴──────────────────────────┤
//! │  [MODE] > input                     │ ← Input Line
//! ├─────────────────────────────────────┤
//! │ status bar                          │ ← Status Bar
//! └─────────────────────────────────────┘
//! ```

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders};

use crate::app::{AppState, FocusArea};
use crate::component::Component;

/// Render the entire TUI into the current frame.
pub fn draw(frame: &mut Frame, state: &AppState) {
    let area = frame.area();

    // ── Vertical split: body | input line | status bar ──
    let vertical = Layout::vertical([
        Constraint::Min(3),    // body (sidebar + main)
        Constraint::Length(1), // input line
        Constraint::Length(1), // status bar
    ])
    .split(area);

    let body_area = vertical[0];
    let input_area = vertical[1];
    let status_area = vertical[2];

    // ── Focus indicator styles ──
    let focus_style = Style::default().fg(Color::Cyan);
    let dim_style = Style::default().fg(Color::DarkGray);

    // ── Body: optional sidebar | main view ──
    if state.sidebar_visible() {
        // Sidebar width: 20–40 cols, ~25% of terminal width.
        let sidebar_width = (area.width / 4)
            .clamp(20, 40)
            .min(area.width.saturating_sub(30));

        let horizontal =
            Layout::horizontal([Constraint::Length(sidebar_width), Constraint::Min(30)])
                .split(body_area);

        // Sidebar with focus border.
        let sb_style = if state.focus == FocusArea::Sidebar {
            focus_style
        } else {
            dim_style
        };
        let sb_block = Block::new()
            .borders(Borders::ALL)
            .border_style(sb_style)
            .title(" Sidebar ");
        let sb_inner = sb_block.inner(horizontal[0]);
        frame.render_widget(sb_block, horizontal[0]);
        state.sidebar.render(frame, sb_inner);

        // MainView with focus border.
        let mv_style = if state.focus == FocusArea::MainView {
            focus_style
        } else {
            dim_style
        };
        let mv_block = Block::new()
            .borders(Borders::ALL)
            .border_style(mv_style)
            .title(" Output ");
        let mv_inner = mv_block.inner(horizontal[1]);
        frame.render_widget(mv_block, horizontal[1]);
        state.main_view.render(frame, mv_inner);
    } else {
        let mv_style = if state.focus == FocusArea::MainView {
            focus_style
        } else {
            dim_style
        };
        let mv_block = Block::new()
            .borders(Borders::ALL)
            .border_style(mv_style)
            .title(" Output ");
        let mv_inner = mv_block.inner(body_area);
        frame.render_widget(mv_block, body_area);
        state.main_view.render(frame, mv_inner);
    }

    // ── Input line ──
    state.input.render(frame, input_area);

    // ── Status bar ──
    state.status_bar.render(frame, status_area);
}
