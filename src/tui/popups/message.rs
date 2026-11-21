use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
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
    // Where the buttons were drawn last frame, so a click is answered by the layout
    // the operator can see.
    buttons: Vec<Rect>,
}

impl MessagePrompt {
    pub fn new(task_id: String, agent: String) -> Self {
        Self {
            task_id,
            agent,
            body: TextBox::new(""),
            buttons: Vec::new(),
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
        if keys::is_secondary(key) {
            return self.send();
        }
        self.body.handle_key(key);
        None
    }

    pub fn handle_mouse(&self, mouse: MouseEvent) -> Option<Decision> {
        match button::hit(&self.buttons, mouse)? {
            0 => Some(Decision::Cancel),
            1 => self.send(),
            _ => None,
        }
    }

    // Nothing to say is nothing to send, whichever way the operator asked.
    fn send(&self) -> Option<Decision> {
        let body = self.body.value();
        let body = body.trim();
        (!body.is_empty()).then(|| Decision::Send(body.to_owned()))
    }
}

pub enum Decision {
    Cancel,
    Send(String),
}

pub fn draw(frame: &mut Frame, area: Rect, prompt: &mut MessagePrompt) -> Rect {
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
    prompt.buttons = button::actions(
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
