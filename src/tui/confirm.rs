//! Asking before something is destroyed.
//!
//! One question, two answers, and the same keys as everywhere else: escape
//! cancels, and `d`, delete or backspace go through with it. What it is about to
//! do is the caller's business, so the caller keeps the action and hands this
//! only the words.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};

use super::{chrome, keys};

/// A question with two answers, one of which cannot be undone.
pub struct Confirm {
    pub question: String,
    /// The word on the destructive button: delete, remove, discard.
    pub destructive: &'static str,
    /// 0 is cancel, 1 is the destructive button.
    pub button: usize,
}

/// What the operator answered.
pub enum Decision {
    Cancel,
    Confirm,
}

impl Confirm {
    pub fn new(question: impl Into<String>, destructive: &'static str) -> Self {
        Self {
            question: question.into(),
            destructive,
            // Starting on cancel means the key that opens this is never the key
            // that destroys something.
            button: 0,
        }
    }

    /// The keys that work here, for the guidance line.
    pub fn hints(&self) -> Vec<chrome::Hint> {
        vec![
            chrome::Hint::new("tab", "switch"),
            chrome::Hint::new("enter", "confirm"),
            chrome::Hint::new("esc", "cancel"),
        ]
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Decision> {
        if keys::is_dismiss(key) {
            return Some(Decision::Cancel);
        }
        // The remove keys mean the same here as everywhere: take it away.
        if keys::is_remove(key) {
            return Some(Decision::Confirm);
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                self.button = 1 - self.button;
                None
            }
            KeyCode::Enter => Some(if self.button == 1 {
                Decision::Confirm
            } else {
                Decision::Cancel
            }),
            _ => None,
        }
    }
}

pub fn draw(frame: &mut Frame, area: Rect, confirm: &Confirm) {
    let popup = chrome::centered(area, 60, 7);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Are you sure? ")
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(2));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [question, buttons] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(inner);
    frame.render_widget(
        Paragraph::new(confirm.question.clone()).wrap(Wrap { trim: true }),
        question,
    );
    let [_, cancel, _, destructive] = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(10),
        Constraint::Length(3),
        Constraint::Length(10),
    ])
    .areas(buttons);
    chrome::button(frame, cancel, "cancel", confirm.button == 0, false);
    chrome::button(
        frame,
        destructive,
        confirm.destructive,
        confirm.button == 1,
        true,
    );
}
