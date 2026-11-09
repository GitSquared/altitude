use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Clear, Paragraph, Wrap},
};

use crate::tui::{
    keys,
    widgets::{button, chrome, palette, text::TextBox},
};

pub struct MessagePrompt {
    task_id: String,
    agent: String,
    body: TextBox,
}

impl MessagePrompt {
    pub fn new(task_id: String, agent: String) -> Self {
        Self {
            task_id,
            agent,
            body: TextBox::new(""),
        }
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn hints(&self) -> Vec<chrome::Hint> {
        vec![
            chrome::Hint::new("ctrl-↩", "send"),
            chrome::Hint::new("esc", "cancel"),
        ]
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Decision> {
        if key.code == KeyCode::Esc {
            return Some(Decision::Cancel);
        }
        if keys::is_secondary(key) && !self.body.value().trim().is_empty() {
            return Some(Decision::Send(self.body.value().trim().to_owned()));
        }
        self.body.handle_key(key);
        None
    }
}

pub enum Decision {
    Cancel,
    Send(String),
}

pub fn draw(frame: &mut Frame, area: Rect, prompt: &MessagePrompt) -> Rect {
    let popup = chrome::centered(area, 64, chrome::modal_height(9));
    frame.render_widget(Clear, popup);
    let block = chrome::modal(format!("Message {}", prompt.agent));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [hint, field, _, actions] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Line::styled(
            "This becomes task input. The agent answers in its threads, or in what it says.",
            palette::muted(),
        ))
        .wrap(Wrap { trim: true }),
        hint,
    );
    prompt.body.draw_titled(frame, field, true, None);
    button::actions(
        frame,
        popup,
        actions,
        &[
            button::Action::secondary("cancel"),
            button::Action::primary("send").focused(true),
        ],
    );
    popup
}
