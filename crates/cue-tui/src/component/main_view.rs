//! Main view component — the card-style REPL area.
//!
//! Displays a vertical stack of "cards", each representing one command
//! and its output.  Cards are colored by status (green=ok, red=error,
//! yellow=pending/streaming).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::Component;
use crate::app::AppMsg;

// ── Card types ──

/// Visual status of a card, determines border color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardStatus {
    Success,
    Error,
    Pending,
    Streaming,
}

impl CardStatus {
    fn border_color(self) -> Color {
        match self {
            Self::Success => Color::Green,
            Self::Error => Color::Red,
            Self::Pending => Color::Yellow,
            Self::Streaming => Color::Cyan,
        }
    }
}

/// A single REPL card: one command + its output.
#[derive(Debug, Clone)]
pub struct Card {
    /// What the user typed.
    pub input: String,
    /// Response / accumulated output.
    pub output: String,
    /// Current status.
    pub status: CardStatus,
    /// Optional short ID label (e.g. "J1").
    pub label: Option<String>,
}

impl Card {
    pub fn new(input: String) -> Self {
        Self {
            input,
            output: String::new(),
            status: CardStatus::Pending,
            label: None,
        }
    }

    /// Height in terminal rows this card would occupy (including border).
    fn height(&self) -> u16 {
        // 2 for top/bottom border + 1 for each output line (min 1)
        let output_lines = self.output.lines().count().max(1) as u16;
        2 + output_lines
    }
}

// ── Component messages ──

/// Messages local to the main view.
pub enum MainViewMsg {
    /// Append a new card for a submitted command.
    PushCard(Card),
    /// Update the latest card's output (streaming).
    AppendOutput { data: String },
    /// Set the latest card's status.
    SetLatestStatus(CardStatus),
    /// Set the latest card's label.
    SetLatestLabel(String),
}

// ── MainView ──

pub struct MainView {
    /// All cards, oldest first.
    pub cards: Vec<Card>,
    /// Scroll offset (0 = bottom, i.e. show newest).
    pub scroll_offset: usize,
}

impl MainView {
    pub fn new() -> Self {
        Self {
            cards: Vec::new(),
            scroll_offset: 0,
        }
    }
}

impl Default for MainView {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for MainView {
    type Message = MainViewMsg;

    fn update(&mut self, msg: MainViewMsg) {
        match msg {
            MainViewMsg::PushCard(card) => {
                self.cards.push(card);
                self.scroll_offset = 0; // auto-scroll to bottom
            }
            MainViewMsg::AppendOutput { data } => {
                if let Some(card) = self.cards.last_mut() {
                    card.output.push_str(&data);
                    card.status = CardStatus::Streaming;
                }
            }
            MainViewMsg::SetLatestStatus(status) => {
                if let Some(card) = self.cards.last_mut() {
                    card.status = status;
                }
            }
            MainViewMsg::SetLatestLabel(label) => {
                if let Some(card) = self.cards.last_mut() {
                    card.label = Some(label);
                }
            }
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        if self.cards.is_empty() {
            let welcome = Paragraph::new(vec![
                Line::raw(""),
                Line::raw("  Welcome to cue-shell"),
                Line::raw("  Type a command and press Enter."),
                Line::raw(""),
                Line::raw("  Shift+Tab to switch modes (Job / Agent / Cron)"),
                Line::raw("  Ctrl+B to toggle sidebar"),
                Line::raw("  Ctrl+C to quit"),
            ])
            .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(welcome, area);
            return;
        }

        // Render cards bottom-up, filling available height.
        let mut y = area.bottom();
        let start = if self.scroll_offset >= self.cards.len() {
            0
        } else {
            self.cards.len() - self.scroll_offset
        };

        for card in self.cards[..start].iter().rev() {
            let h = card.height().min(y - area.y);
            if h == 0 {
                break;
            }
            y -= h;
            let card_area = Rect::new(area.x, y, area.width, h);
            render_card(frame, card, card_area);
            if y <= area.y {
                break;
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<AppMsg> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        match key.code {
            KeyCode::PageUp => {
                self.scroll_offset =
                    (self.scroll_offset + 5).min(self.cards.len().saturating_sub(1));
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(5);
            }
            // Pass Ctrl combos up.
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some(AppMsg::Quit);
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some(AppMsg::ToggleSidebar);
            }
            _ => {}
        }
        None
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<AppMsg> {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_offset =
                    (self.scroll_offset + 3).min(self.cards.len().saturating_sub(1));
            }
            MouseEventKind::ScrollDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(3);
            }
            _ => {}
        }
        None
    }
}

/// Render a single card into the given area.
fn render_card(frame: &mut Frame, card: &Card, area: Rect) {
    let border_color = card.status.border_color();

    let title = match &card.label {
        Some(label) => format!(" > {} [{}] ", card.input, label),
        None => format!(" > {} ", card.input),
    };

    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(title);

    let output = if card.output.is_empty() {
        match card.status {
            CardStatus::Pending => "⏳ waiting...".to_string(),
            CardStatus::Streaming => "▌".to_string(),
            _ => String::new(),
        }
    } else {
        card.output.clone()
    };

    let paragraph = Paragraph::new(output)
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}
