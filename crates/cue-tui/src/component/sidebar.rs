//! Sidebar component — mode-specific entity list + overview counts.
//!
//! Shows a list of jobs/agents/crons depending on the active mode,
//! with an overview section at the bottom.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use super::Component;
use crate::app::AppMsg;

// ── Types ──

/// A single item in the sidebar list.
#[derive(Debug, Clone)]
pub struct SidebarItem {
    /// Short identifier, e.g. "J1", "A2".
    pub id: String,
    /// Human-readable label (command text).
    pub label: String,
    /// Status icon.
    pub status_icon: &'static str,
}

/// Aggregate counts shown in the overview section.
#[derive(Debug, Clone, Default)]
pub struct OverviewCounts {
    pub jobs: u32,
    pub jobs_running: u32,
    pub agents: u32,
    pub crons: u32,
}

// ── Component messages ──

/// Messages local to the sidebar.
pub enum SidebarMsg {
    /// Replace the item list entirely.
    SetItems(Vec<SidebarItem>),
    /// Update overview counts.
    SetOverview(OverviewCounts),
}

// ── Sidebar ──

pub struct Sidebar {
    /// Current list items.
    pub items: Vec<SidebarItem>,
    /// Currently selected index (for highlight).
    pub selected: Option<usize>,
    /// Aggregate counts.
    pub overview: OverviewCounts,
}

impl Sidebar {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            selected: None,
            overview: OverviewCounts::default(),
        }
    }
}

impl Default for Sidebar {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Sidebar {
    type Message = SidebarMsg;

    fn update(&mut self, msg: SidebarMsg) {
        match msg {
            SidebarMsg::SetItems(items) => {
                self.items = items;
                // Keep selection in bounds.
                if self.selected.is_some_and(|sel| sel >= self.items.len()) {
                    self.selected = self.items.len().checked_sub(1);
                }
            }
            SidebarMsg::SetOverview(counts) => {
                self.overview = counts;
            }
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        // Split: top = list, bottom 3 rows = overview.
        let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).split(area);

        // ── Item list ──
        let list_block = Block::new().borders(Borders::ALL).title(" Entities ");

        let items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let style = if self.selected == Some(i) {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{} ", item.status_icon),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(&item.id, Style::default().fg(Color::Cyan)),
                    Span::raw(" "),
                    Span::raw(&item.label),
                ]))
                .style(style)
            })
            .collect();

        let list = List::new(items).block(list_block);
        frame.render_widget(list, chunks[0]);

        // ── Overview ──
        let ov = &self.overview;
        let running = if ov.jobs_running > 0 {
            format!("({}🔄)", ov.jobs_running)
        } else {
            String::new()
        };
        let overview_text = format!("J:{}{} A:{} C:{}", ov.jobs, running, ov.agents, ov.crons);
        let overview_block = Block::new().borders(Borders::ALL).title(" Overview ");
        let overview = Paragraph::new(overview_text)
            .block(overview_block)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(overview, chunks[1]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<AppMsg> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        match (key.modifiers, key.code) {
            (_, KeyCode::Up) => {
                if let Some(sel) = self.selected.as_mut() {
                    *sel = sel.saturating_sub(1);
                } else if !self.items.is_empty() {
                    self.selected = Some(self.items.len() - 1);
                }
            }
            (_, KeyCode::Down) => {
                if let Some(sel) = self.selected.as_mut() {
                    *sel = (*sel + 1).min(self.items.len().saturating_sub(1));
                } else if !self.items.is_empty() {
                    self.selected = Some(0);
                }
            }
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Some(AppMsg::Quit),
            (KeyModifiers::CONTROL, KeyCode::Char('b')) => return Some(AppMsg::ToggleSidebar),
            _ => {}
        }
        None
    }
}
