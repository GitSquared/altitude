//! The first thing a new installation shows: what a task cannot run without, and
//! where the rest of it lives.
//!
//! A screen rather than a wizard. Each row is the action itself, so an operator
//! whose login already works is two keys from work, and an operator who wants to
//! look around first can leave and come back while the one required choice is
//! still missing. Once an agent is chosen the panel is not offered again: Settings
//! is where these live afterwards.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Padding, Paragraph, Wrap},
};

use crate::{
    agent::AgentKind,
    tui::{
        app::CredentialStatus,
        widgets::{
            chrome::Hint,
            palette,
            select::{Offer, Select},
            surface::Surface,
            text::{Field, FieldStyle},
            utils::truncate,
        },
    },
};

/// What the keys are on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Name,
    Agent,
    Create,
    Settings,
}

const ROWS: [Focus; 4] = [Focus::Name, Focus::Agent, Focus::Create, Focus::Settings];

pub struct GetStarted {
    pub focus: Focus,
    pub name: Field,
    pub editing_name: bool,
    pub agent: Select<AgentKind>,
}

impl GetStarted {
    /// Open on the project's name, and on the first thing still missing.
    pub fn new(project_name: &str, agent: Option<AgentKind>, agents: &[AgentKind]) -> Self {
        let chosen = agent
            .and_then(|chosen| agents.iter().position(|candidate| *candidate == chosen))
            .unwrap_or(0);
        Self {
            focus: if agent.is_some() {
                Focus::Create
            } else {
                Focus::Name
            },
            name: Field::with(project_name),
            editing_name: false,
            agent: Select::new(
                "agent",
                agents
                    .iter()
                    .copied()
                    .map(|kind| Offer::new(kind, kind.to_string()))
                    .collect(),
                chosen,
            ),
        }
    }

    pub fn move_focus(&mut self, forward: bool) {
        let at = ROWS.iter().position(|row| *row == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % ROWS.len()
        } else {
            (at + ROWS.len() - 1) % ROWS.len()
        };
        self.focus = ROWS[next];
    }

    /// Whether an edit has the keys, in which case `?` and `q` are characters.
    pub fn is_typing(&self) -> bool {
        self.editing_name
    }

    /// Put the name back to what is saved, when an edit is left.
    pub fn reset_name(&mut self, name: &str) {
        self.name = Field::with(name);
    }

    pub fn hints(&self, agent: Option<AgentKind>) -> Vec<Hint> {
        if self.editing_name {
            return vec![
                Hint::new("enter", "save the name"),
                Hint::new("esc", "leave it as it was"),
            ];
        }
        if self.agent.is_open() {
            return self.agent.hints();
        }
        let mut hints = vec![Hint::new("j/k", "move"), Hint::new("esc", "look around")];
        match self.focus {
            Focus::Name => hints.insert(0, Hint::new("enter", "rename")),
            Focus::Agent => match agent {
                Some(_) => {
                    hints.insert(0, Hint::new("enter", "sign in"));
                    hints.insert(1, Hint::new("space", "change"));
                }
                None => hints.insert(0, Hint::new("enter", "choose")),
            },
            Focus::Create => hints.insert(0, Hint::new("enter", "create a task")),
            Focus::Settings => hints.insert(0, Hint::new("enter", "open settings")),
        }
        hints
    }
}

/// Whether a profile's login is known to work. A file on disk counts until a
/// check says otherwise, because that is what a task would mount.
pub fn signed_in(status: &CredentialStatus) -> bool {
    matches!(
        status,
        CredentialStatus::Checked {
            signed_in: true,
            ..
        } | CredentialStatus::Saved
    )
}

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    panel: &mut GetStarted,
    agent: Option<AgentKind>,
    status: &CredentialStatus,
) {
    let body = Surface::new()
        .padding(Padding::uniform(2))
        .draw(frame, area);
    let [title, rows, actions_area, detail, note] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas(body);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("GET STARTED", palette::heading()),
            Span::raw("  "),
            Span::styled("one choice before a task can run", palette::muted()),
        ])),
        title,
    );

    let width = rows.width;
    let name = row(
        true,
        "Project",
        panel.name.value(),
        "enter rename",
        panel.focus == Focus::Name,
        width,
    );
    let agent_status = match agent {
        None => "not chosen".to_owned(),
        Some(kind) if matches!(status, CredentialStatus::Checking) => {
            format!("{kind} · checking…")
        }
        Some(kind) if signed_in(status) => format!("{kind} · signed in"),
        Some(kind) => format!("{kind} · not signed in"),
    };
    let agent_row = row(
        signed_in(status),
        "Agent",
        &agent_status,
        if agent.is_some() {
            "enter sign in"
        } else {
            "enter choose"
        },
        panel.focus == Focus::Agent,
        width,
    );
    let actions = Line::from(vec![
        action("Create the first task", panel.focus == Focus::Create),
        Span::raw("      "),
        action("Open Settings", panel.focus == Focus::Settings),
    ]);
    frame.render_widget(Paragraph::new(vec![name, agent_row]), rows);
    frame.render_widget(Paragraph::new(actions), actions_area);

    let anchor = Rect {
        y: rows.y + 1,
        height: 1,
        ..rows
    };
    if panel.agent.is_open() {
        panel.agent.draw_list_below(frame, body, anchor);
    } else if panel.editing_name {
        draw_name_editor(frame, detail, panel);
    } else {
        frame.render_widget(
            Paragraph::new(detail_text(panel.focus, agent))
                .wrap(Wrap { trim: true })
                .style(palette::muted()),
            detail,
        );
    }

    frame.render_widget(
        Paragraph::new(Line::styled(
            "You can tweak these options in Settings later.",
            palette::muted(),
        )),
        note,
    );
}

fn draw_name_editor(frame: &mut Frame, area: Rect, panel: &mut GetStarted) {
    let [label, field] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(3)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::styled(
            "What is this project called?",
            palette::muted(),
        )),
        label,
    );
    panel.name.draw(
        frame,
        field,
        &FieldStyle {
            focused: true,
            title: None,
            prefix: "",
        },
    );
}

fn detail_text(focus: Focus, agent: Option<AgentKind>) -> String {
    match focus {
        Focus::Name => {
            "The project is the work: its repository, its caches, what the agent is told, and the \
             machine it runs on. A name you recognise is enough."
                .to_owned()
        }
        Focus::Agent if agent.is_none() => {
            "The agent is what does the work, in a sandbox of its own. Choose one you can sign in \
             to; either works with any project."
                .to_owned()
        }
        Focus::Agent => {
            "Signing in happens in the agent's own interface, in a throwaway sandbox. What it \
             writes is kept here and mounted into every task."
                .to_owned()
        }
        Focus::Create if agent.is_none() => {
            "A task needs an agent. Choose one and sign in, and this is where work starts."
                .to_owned()
        }
        Focus::Create => {
            "Say what you want done. Altitude starts an agent on it, and carries its questions \
             back to you from the task list."
                .to_owned()
        }
        Focus::Settings => {
            "Repositories, caches, instructions, the machine a project runs on, and any task \
             tracker are all in Settings."
                .to_owned()
        }
    }
}

fn row(
    done: bool,
    label: &str,
    value: &str,
    action: &str,
    focused: bool,
    width: u16,
) -> Line<'static> {
    let marker = if done {
        "✓"
    } else if focused {
        "●"
    } else {
        "○"
    };
    let prefix = format!("{marker}  {label:<10}");
    let trailing = format!("{action:<16}");
    let available = usize::from(width)
        .saturating_sub(prefix.chars().count())
        .saturating_sub(trailing.chars().count());
    let value = truncate(value, available);
    let style = if focused {
        palette::primary().bold()
    } else {
        palette::primary()
    };
    Line::from(vec![
        Span::styled(
            prefix,
            if focused {
                palette::primary().bold()
            } else {
                palette::muted()
            },
        ),
        Span::styled(format!("{value:<available$}"), style),
        Span::styled(trailing, palette::muted()),
    ])
}

fn action(label: &str, focused: bool) -> Span<'static> {
    if focused {
        Span::styled(format!(" {label} "), palette::focus())
    } else {
        Span::styled(format!(" {label} "), palette::muted())
    }
}

/// The keys a name edit owns, so the screen's own keys can stand aside.
pub fn name_key(key: KeyEvent) -> NameKey {
    match key.code {
        KeyCode::Enter => NameKey::Save,
        KeyCode::Esc => NameKey::Cancel,
        _ => NameKey::Typing(key),
    }
}

pub enum NameKey {
    Save,
    Cancel,
    Typing(KeyEvent),
}
