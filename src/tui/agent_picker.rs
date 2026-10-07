//! Asking which coding agent to drive, the first time.
//!
//! Asked once, at the moment it matters, and remembered afterwards in settings:
//! an environment says what is installed, and this says what works in it. The
//! two are independent, so this is not a property of any environment.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};

use super::{chrome, keys};
use crate::agent::AgentKind;

/// The agents on offer, in the order they are shown.
pub const OFFERED: [AgentKind; 2] = [AgentKind::Pi, AgentKind::Claude];

/// What the operator answered.
pub enum Answer {
    Cancel,
    Chosen(AgentKind),
}

/// The keys that work here, for the guidance line.
pub fn hints() -> Vec<chrome::Hint> {
    vec![
        chrome::Hint::new("tab", "switch"),
        chrome::Hint::new("enter", "choose"),
        chrome::Hint::new("esc", "cancel"),
    ]
}

pub fn handle_key(key: KeyEvent, selected: &mut usize) -> Option<Answer> {
    if keys::is_dismiss(key) {
        return Some(Answer::Cancel);
    }
    match key.code {
        KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
            *selected = 1 - *selected;
            None
        }
        KeyCode::Enter => Some(Answer::Chosen(OFFERED[*selected])),
        _ => None,
    }
}

pub fn draw(frame: &mut Frame, area: Rect, selected: usize) {
    let popup = chrome::centered(area, 50, 8);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Coding agent ")
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(2));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [question, buttons] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(inner);
    frame.render_widget(
        Paragraph::new("Which coding agent should Altitude drive?").wrap(Wrap { trim: true }),
        question,
    );
    let [pi, _, claude] = Layout::horizontal([
        Constraint::Length(12),
        Constraint::Length(3),
        Constraint::Length(12),
    ])
    .areas(buttons);
    chrome::button(
        frame,
        pi,
        &AgentKind::Pi.to_string(),
        selected == 0,
        selected != 0,
    );
    chrome::button(
        frame,
        claude,
        &AgentKind::Claude.to_string(),
        selected == 1,
        selected != 1,
    );
}
