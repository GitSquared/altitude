use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    widgets::Clear,
};

use super::text::{Field, FieldStyle};

/// A one-line question, asked over whatever is on screen.
pub struct Prompt {
    title: &'static str,
    field: Field,
}

pub enum PromptAction {
    Cancel,
    Submit(String),
}

impl Prompt {
    pub fn new(title: &'static str) -> Self {
        Self {
            title,
            field: Field::new(),
        }
    }

    pub fn paste(&mut self, text: &str) {
        self.field.paste(text);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<PromptAction> {
        match key.code {
            KeyCode::Esc => Some(PromptAction::Cancel),
            KeyCode::Enter if !self.field.is_blank() => {
                Some(PromptAction::Submit(self.field.value().trim().to_owned()))
            }
            _ => {
                self.field.handle_key(key);
                None
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let area = centered_rect(70, 7, area);
        frame.render_widget(Clear, area);
        self.field.draw(
            frame,
            area,
            &FieldStyle {
                focused: true,
                title: Some(self.title),
                prefix: "❯ ",
            },
        );
    }
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .areas(vertical);
    horizontal
}
