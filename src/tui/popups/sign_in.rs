//! What a tool's own sign-in asks of the operator, before a terminal opens for
//! it.
//!
//! A tool whose sign-in happens inside its own interface has to be driven: there
//! is no command that finishes on its own. Saying what to do there, and that
//! leaving saves the login, is the difference between arriving in a TUI that
//! means nothing and knowing which key to press.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Clear, Paragraph, Wrap},
};

use crate::tui::{
    keys,
    widgets::{chrome, palette},
};

/// A sign-in the operator drives, described before its terminal opens.
pub struct SignIn {
    pub display: String,
    pub profile: &'static str,
    lines: Vec<Line<'static>>,
}

/// What the operator answered.
pub enum Decision {
    Cancel,
    Open,
}

impl SignIn {
    pub fn new(display: &str, profile: &'static str, instructions: &str) -> Self {
        let lines = vec![
            Line::from(instructions.to_owned()),
            Line::from(""),
            Line::styled(
                format!("Log in to {display}, and exit. Your credentials will be saved."),
                palette::accent(),
            ),
        ];
        Self {
            display: display.to_owned(),
            profile,
            lines,
        }
    }

    pub fn hints(&self) -> Vec<chrome::Hint> {
        vec![
            chrome::Hint::new("enter", "open the terminal"),
            chrome::Hint::new("esc", "cancel"),
        ]
    }

    pub fn handle_key(&self, key: KeyEvent) -> Option<Decision> {
        if keys::is_dismiss(key) {
            return Some(Decision::Cancel);
        }
        matches!(key.code, KeyCode::Enter).then_some(Decision::Open)
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) -> Rect {
        // Only as tall as the words are: borders, a gap, and the button besides.
        let width = (area.width * 60 / 100).max(1);
        let rows = Paragraph::new(self.lines.clone())
            .wrap(Wrap { trim: true })
            .line_count(width.saturating_sub(4).max(1));
        let height = (rows as u16).saturating_add(6).min(area.height);
        let popup = chrome::centered(area, 60, height);
        frame.render_widget(Clear, popup);
        let block = chrome::modal(format!("Sign in to {}", self.display));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let [text, actions] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(inner);
        frame.render_widget(
            Paragraph::new(self.lines.clone()).wrap(Wrap { trim: true }),
            text,
        );
        chrome::actions(
            frame,
            popup,
            actions,
            &[chrome::Action::new("sign in").focused(true).primary()],
        );
        popup
    }
}
