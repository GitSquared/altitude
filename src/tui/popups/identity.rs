//! The two small questions the identities list asks: which service to connect,
//! and what to call the identity.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    text::Line,
    widgets::{Block, BorderType, Clear, Padding, Paragraph},
};

use crate::{
    credentials::Service,
    tui::widgets::{
        palette,
        text::{Field, FieldStyle},
    },
};

pub struct IdentitySetup {
    services: [Service; 4],
    at: usize,
}

pub enum SetupDecision {
    Still,
    Closed,
    Chosen(Service),
}

impl IdentitySetup {
    pub fn new() -> Self {
        Self {
            services: Service::ALL,
            at: 0,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> SetupDecision {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => SetupDecision::Closed,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.at = (self.at + 1) % self.services.len();
                SetupDecision::Still
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.at = (self.at + self.services.len() - 1) % self.services.len();
                SetupDecision::Still
            }
            KeyCode::Enter => SetupDecision::Chosen(self.services[self.at]),
            _ => SetupDecision::Still,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let height = u16::try_from(self.services.len()).unwrap_or(4) + 2;
        let popup = centered(area, 44, height);
        frame.render_widget(Clear, popup);
        let block = Block::bordered()
            .title(" Connect an identity ")
            .border_type(BorderType::Rounded)
            .border_style(palette::primary())
            .padding(Padding::horizontal(1));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let lines = self.services.iter().enumerate().map(|(index, service)| {
            let selected = index == self.at;
            Line::styled(
                format!("{}{}", if selected { "› " } else { "  " }, service.label()),
                if selected {
                    palette::primary().bold()
                } else {
                    palette::muted()
                },
            )
        });
        frame.render_widget(Paragraph::new(lines.collect::<Vec<_>>()), inner);
    }
}

pub struct RenameIdentity {
    pub id: String,
    field: Field,
}

pub enum RenameDecision {
    Still,
    Closed,
    Chosen(String),
}

impl RenameIdentity {
    pub fn new(id: String, label: &str) -> Self {
        Self {
            id,
            field: Field::with(label),
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> RenameDecision {
        match key.code {
            KeyCode::Esc => RenameDecision::Closed,
            KeyCode::Enter => RenameDecision::Chosen(self.field.value().trim().to_owned()),
            _ => {
                self.field.handle_key(key);
                RenameDecision::Still
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let popup = centered(area, 50, 5);
        frame.render_widget(Clear, popup);
        let block = Block::bordered()
            .title(" Name this identity ")
            .border_type(BorderType::Rounded)
            .border_style(palette::primary())
            .padding(Padding::horizontal(1));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        self.field.draw(
            frame,
            Rect {
                y: inner.y.saturating_add(1),
                height: 1,
                ..inner
            },
            &FieldStyle {
                focused,
                title: None,
                prefix: "",
            },
        );
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(vertical);
    horizontal
}
