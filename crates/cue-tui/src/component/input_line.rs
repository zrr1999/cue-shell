//! Input line component — the command prompt at the bottom of the TUI.
//!
//! Renders: `[JOB ⚡] > {content}` with a blinking cursor.
//! Keys: printable→insert, Backspace→delete, Left/Right→move cursor,
//!        Enter→submit, ↑↓→history, Shift+Tab→mode cycle.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use cue_core::Mode;

use super::Component;
use crate::app::AppMsg;

/// Messages local to the input line.
pub enum InputMsg {
    /// Set the active mode (reflects global mode changes).
    SetMode(Mode),
    /// Clear content after submission.
    Clear,
}

pub struct InputLine {
    /// Current text content.
    pub content: String,
    /// Byte-offset cursor position within `content`.
    pub cursor: usize,
    /// Command history (newest last).
    pub history: Vec<String>,
    /// Current position in history browsing (`None` = not browsing).
    history_idx: Option<usize>,
    /// Stashed content when browsing history.
    stashed: String,
    /// Active input mode.
    pub mode: Mode,
}

impl InputLine {
    pub fn new() -> Self {
        Self {
            content: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            stashed: String::new(),
            mode: Mode::default(),
        }
    }

    /// Take the current content (for submission) and push to history.
    pub fn take_input(&mut self) -> String {
        let input = std::mem::take(&mut self.content);
        self.cursor = 0;
        self.history_idx = None;
        if !input.trim().is_empty() {
            self.history.push(input.clone());
        }
        input
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_idx {
            None => {
                self.stashed = self.content.clone();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_idx = Some(idx);
        self.content.clone_from(&self.history[idx]);
        self.cursor = self.content.len();
    }

    fn history_down(&mut self) {
        let Some(idx) = self.history_idx else {
            return;
        };
        if idx + 1 >= self.history.len() {
            // Restore stashed content.
            self.history_idx = None;
            self.content.clone_from(&self.stashed);
        } else {
            self.history_idx = Some(idx + 1);
            self.content.clone_from(&self.history[idx + 1]);
        }
        self.cursor = self.content.len();
    }
}

impl Default for InputLine {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for InputLine {
    type Message = InputMsg;

    fn update(&mut self, msg: InputMsg) {
        match msg {
            InputMsg::SetMode(mode) => self.mode = mode,
            InputMsg::Clear => {
                self.content.clear();
                self.cursor = 0;
                self.history_idx = None;
            }
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        let mode_style = match self.mode {
            Mode::Job => Style::default().fg(Color::Yellow),
            Mode::Agent => Style::default().fg(Color::Cyan),
            Mode::Cron => Style::default().fg(Color::Magenta),
        };

        let prompt = Span::styled(format!(" [{}] > ", self.mode.indicator()), mode_style);

        let (before, after) = self.content.split_at(self.cursor.min(self.content.len()));
        let cursor_char = after.chars().next().unwrap_or(' ');
        let rest = if after.is_empty() {
            ""
        } else {
            &after[cursor_char.len_utf8()..]
        };

        let line = Line::from(vec![
            prompt,
            Span::raw(before),
            Span::styled(
                cursor_char.to_string(),
                Style::default().fg(Color::Black).bg(Color::White),
            ),
            Span::raw(rest),
        ]);

        let paragraph = Paragraph::new(line);
        frame.render_widget(paragraph, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<AppMsg> {
        // Only handle Press events.
        if key.kind != KeyEventKind::Press {
            return None;
        }

        match (key.modifiers, key.code) {
            // Shift+Tab → mode cycle
            (KeyModifiers::SHIFT, KeyCode::BackTab) | (_, KeyCode::BackTab) => {
                return Some(AppMsg::ModeSwitch);
            }

            // Ctrl+C → quit
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                return Some(AppMsg::Quit);
            }

            // Ctrl+B → toggle sidebar
            (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
                return Some(AppMsg::ToggleSidebar);
            }

            // Enter → submit
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Enter) => {
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    // Shift/Ctrl+Enter → newline (multiline not yet implemented, ignore)
                    return None;
                }
                let input = self.take_input();
                if !input.is_empty() {
                    return Some(AppMsg::Submit(input));
                }
            }

            // History navigation
            (_, KeyCode::Up) => self.history_up(),
            (_, KeyCode::Down) => self.history_down(),

            // Cursor movement
            (_, KeyCode::Left) => {
                if self.cursor > 0 {
                    // Move back one character.
                    let prev = self.content[..self.cursor]
                        .chars()
                        .next_back()
                        .map_or(0, char::len_utf8);
                    self.cursor -= prev;
                }
            }
            (_, KeyCode::Right) => {
                if self.cursor < self.content.len() {
                    let next = self.content[self.cursor..]
                        .chars()
                        .next()
                        .map_or(0, char::len_utf8);
                    self.cursor += next;
                }
            }
            (_, KeyCode::Home) => self.cursor = 0,
            (_, KeyCode::End) => self.cursor = self.content.len(),

            // Backspace
            (_, KeyCode::Backspace) => {
                if self.cursor > 0 {
                    let prev = self.content[..self.cursor]
                        .chars()
                        .next_back()
                        .map_or(0, char::len_utf8);
                    self.content.drain(self.cursor - prev..self.cursor);
                    self.cursor -= prev;
                }
            }

            // Delete
            (_, KeyCode::Delete) => {
                if self.cursor < self.content.len() {
                    let next = self.content[self.cursor..]
                        .chars()
                        .next()
                        .map_or(0, char::len_utf8);
                    self.content.drain(self.cursor..self.cursor + next);
                }
            }

            // Printable character
            (_, KeyCode::Char(c)) => {
                self.content.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.history_idx = None;
            }

            _ => {}
        }

        None
    }

    fn handle_mouse(&mut self, _mouse: MouseEvent) -> Option<AppMsg> {
        None
    }
}
