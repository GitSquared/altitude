//! The two small questions a service section asks: which service to connect,
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

/// One thing a service section can add. A version-control section has a Git
/// identity as well as its provider logins, because the two answer different
/// questions: who signed in, and who the commits are from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityChoice {
    Service(Service),
    GitIdentity,
}

impl IdentityChoice {
    fn label(self) -> &'static str {
        match self {
            Self::Service(service) => service.label(),
            Self::GitIdentity => "Git identity",
        }
    }
}

pub struct IdentitySetup {
    choices: Vec<IdentityChoice>,
    at: usize,
}

pub enum SetupDecision {
    Still,
    Closed,
    Chosen(IdentityChoice),
}

impl IdentitySetup {
    pub fn new(choices: Vec<IdentityChoice>) -> Self {
        Self { choices, at: 0 }
    }

    pub fn key(&mut self, key: KeyEvent) -> SetupDecision {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => SetupDecision::Closed,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab if !self.choices.is_empty() => {
                self.at = (self.at + 1) % self.choices.len();
                SetupDecision::Still
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab if !self.choices.is_empty() => {
                self.at = (self.at + self.choices.len() - 1) % self.choices.len();
                SetupDecision::Still
            }
            KeyCode::Enter => self
                .choices
                .get(self.at)
                .copied()
                .map_or(SetupDecision::Still, SetupDecision::Chosen),
            _ => SetupDecision::Still,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let height = u16::try_from(self.choices.len()).unwrap_or(4) + 2;
        let popup = centered(area, 44, height);
        frame.render_widget(Clear, popup);
        let block = Block::bordered()
            .title(" Connect an identity ")
            .border_type(BorderType::Rounded)
            .border_style(palette::primary())
            .padding(Padding::horizontal(1));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let lines = self.choices.iter().enumerate().map(|(index, choice)| {
            let selected = index == self.at;
            Line::styled(
                format!("{}{}", if selected { "› " } else { "  " }, choice.label()),
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

/// The name, email, and signing choice commits carry.
///
/// It lives here rather than in the first-run card because the same question is
/// asked from Settings, where the card is not on screen.
pub struct GitIdentitySetup {
    label: Field,
    name: Field,
    email: Field,
    signs: bool,
    focus: GitIdentityField,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GitIdentityField {
    Label,
    Name,
    Email,
    Sign,
}

pub enum GitIdentityDecision {
    Still,
    Closed,
    Save {
        label: String,
        name: String,
        email: String,
        signs: bool,
    },
}

impl GitIdentitySetup {
    pub fn new(label: &str, name: &str, email: &str, signs: bool) -> Self {
        Self {
            label: Field::with(label),
            name: Field::with(name),
            email: Field::with(email),
            signs,
            focus: GitIdentityField::Label,
        }
    }

    pub fn is_typing(&self) -> bool {
        matches!(
            self.focus,
            GitIdentityField::Label | GitIdentityField::Name | GitIdentityField::Email
        )
    }

    pub fn key(&mut self, key: KeyEvent) -> GitIdentityDecision {
        match key.code {
            KeyCode::Esc => GitIdentityDecision::Closed,
            KeyCode::Tab | KeyCode::Down => {
                self.move_focus(true);
                GitIdentityDecision::Still
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.move_focus(false);
                GitIdentityDecision::Still
            }
            KeyCode::Char(' ') if self.focus == GitIdentityField::Sign => {
                self.signs = !self.signs;
                GitIdentityDecision::Still
            }
            KeyCode::Enter => GitIdentityDecision::Save {
                label: self.label.value().trim().to_owned(),
                name: self.name.value().trim().to_owned(),
                email: self.email.value().trim().to_owned(),
                signs: self.signs,
            },
            _ => {
                match self.focus {
                    GitIdentityField::Label => self.label.handle_key(key),
                    GitIdentityField::Name => self.name.handle_key(key),
                    GitIdentityField::Email => self.email.handle_key(key),
                    GitIdentityField::Sign => {}
                }
                GitIdentityDecision::Still
            }
        }
    }

    fn move_focus(&mut self, forward: bool) {
        const FIELDS: [GitIdentityField; 4] = [
            GitIdentityField::Label,
            GitIdentityField::Name,
            GitIdentityField::Email,
            GitIdentityField::Sign,
        ];
        let at = FIELDS
            .iter()
            .position(|field| *field == self.focus)
            .unwrap_or(0);
        self.focus = if forward {
            FIELDS[(at + 1) % FIELDS.len()]
        } else {
            FIELDS[(at + FIELDS.len() - 1) % FIELDS.len()]
        };
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let popup = centered(area, 58, 16);
        frame.render_widget(Clear, popup);
        let block = Block::bordered()
            .title(" Git identity ")
            .border_type(BorderType::Rounded)
            .border_style(palette::primary())
            .padding(Padding::horizontal(2));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let [label, _, name, _, email, _, sign, hint] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        self.label.draw(
            frame,
            label,
            &FieldStyle {
                focused: self.focus == GitIdentityField::Label,
                title: Some("Name this identity"),
                prefix: "",
            },
        );
        self.name.draw(
            frame,
            name,
            &FieldStyle {
                focused: self.focus == GitIdentityField::Name,
                title: Some("Name"),
                prefix: "",
            },
        );
        self.email.draw(
            frame,
            email,
            &FieldStyle {
                focused: self.focus == GitIdentityField::Email,
                title: Some("Email"),
                prefix: "",
            },
        );
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("{} Sign commits", if self.signs { "[x]" } else { "[ ]" }),
                if self.focus == GitIdentityField::Sign {
                    palette::accent().bold()
                } else {
                    palette::primary()
                },
            )),
            sign,
        );
        frame.render_widget(
            Paragraph::new(Line::styled(
                "space toggles signing · enter saves · esc cancels",
                palette::muted(),
            )),
            hint,
        );
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
