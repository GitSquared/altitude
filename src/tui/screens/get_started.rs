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
        popups::repositories::Repositories,
        widgets::{
            chrome::Hint,
            palette,
            select::{Offer, Select},
            surface::Surface,
            text::{Field, FieldStyle},
            utils::truncate,
        },
    },
    vcs::VcsProviderKind,
};

/// What the keys are on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Name,
    Agent,
    Code,
    Machine,
    Create,
    Settings,
}

const ROWS: [Focus; 6] = [
    Focus::Name,
    Focus::Agent,
    Focus::Code,
    Focus::Machine,
    Focus::Create,
    Focus::Settings,
];

/// A machine on this network that this installation does not know yet.
#[derive(Clone)]
pub struct MachineOffer {
    pub label: String,
    pub address: String,
}

/// What the screen has to say about the installation it is setting up.
///
/// Owned values rather than the application's, because the screen is drawn while
/// the foreground is borrowed, and the two things it reads are small.
#[derive(Clone)]
pub struct SetupState {
    pub agent: Option<AgentKind>,
    pub agent_status: CredentialStatus,
    pub vcs: VcsProviderKind,
    pub vcs_status: CredentialStatus,
    pub repository: Option<String>,
    /// The machine this project runs on, when it is not this one.
    pub host: Option<String>,
    /// A remote announcing here that this installation has not adopted.
    pub offer: Option<MachineOffer>,
}

pub struct GetStarted {
    pub focus: Focus,
    pub name: Field,
    pub editing_name: bool,
    pub agent: Select<AgentKind>,
    pub repositories: Option<Repositories>,
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
            repositories: None,
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

    /// Whether the keys are characters here: an edit, or a list long enough that
    /// typing narrows it.
    pub fn is_typing(&self) -> bool {
        self.editing_name
            || self
                .repositories
                .as_ref()
                .is_some_and(|browser| browser.select.is_searching())
    }

    /// Put the name back to what is saved, when an edit is left.
    pub fn reset_name(&mut self, name: &str) {
        self.name = Field::with(name);
    }

    pub fn hints(&self, setup: &SetupState) -> Vec<Hint> {
        if self.editing_name {
            return vec![
                Hint::new("enter", "save the name"),
                Hint::new("esc", "leave it as it was"),
            ];
        }
        if let Some(browser) = &self.repositories {
            return browser.select.hints();
        }
        if self.agent.is_open() {
            return self.agent.hints();
        }
        let mut hints = vec![Hint::new("j/k", "move"), Hint::new("esc", "look around")];
        match self.focus {
            Focus::Name => hints.insert(0, Hint::new("enter", "rename")),
            Focus::Agent => match setup.agent {
                Some(_) => {
                    hints.insert(0, Hint::new("enter", "sign in"));
                    hints.insert(1, Hint::new("space", "change"));
                }
                None => hints.insert(0, Hint::new("enter", "choose")),
            },
            Focus::Code => {
                if signed_in(&setup.vcs_status) {
                    hints.insert(
                        0,
                        Hint::new(
                            "enter",
                            if setup.repository.is_some() {
                                "change repository"
                            } else {
                                "choose repository"
                            },
                        ),
                    );
                } else {
                    hints.insert(0, Hint::new("enter", "sign in"));
                }
            }
            Focus::Machine => {
                if setup.host.is_none() && setup.offer.is_some() {
                    hints.insert(0, Hint::new("enter", "add it"));
                }
            }
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

pub fn draw(frame: &mut Frame, area: Rect, panel: &mut GetStarted, setup: &SetupState) {
    let body = Surface::new()
        .padding(Padding::uniform(2))
        .draw(frame, area);
    let [title, rows, actions_area, detail, note] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas(body);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("GET STARTED", palette::heading()),
            Span::raw("  "),
            Span::styled("what a task needs before it can run", palette::muted()),
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
    let agent_status = match setup.agent {
        None => "not chosen".to_owned(),
        Some(kind) if matches!(setup.agent_status, CredentialStatus::Checking) => {
            format!("{kind} · checking…")
        }
        Some(kind) if signed_in(&setup.agent_status) => format!("{kind} · signed in"),
        Some(kind) => format!("{kind} · not signed in"),
    };
    let agent_row = row(
        signed_in(&setup.agent_status),
        "Agent",
        &agent_status,
        if setup.agent.is_some() {
            "enter sign in"
        } else {
            "enter choose"
        },
        panel.focus == Focus::Agent,
        width,
    );
    let vcs = setup.vcs.label();
    let vcs_signed_in = signed_in(&setup.vcs_status);
    let code_value = match (&setup.repository, vcs_signed_in) {
        (Some(repository), true) => repository.clone(),
        (Some(repository), false) => format!("{repository} · {vcs} not signed in"),
        (None, true) => format!("no repository · {vcs} connected"),
        (None, false) => format!("no repository · {vcs} not connected"),
    };
    let code_row = row(
        setup.repository.is_some() && vcs_signed_in,
        "Code",
        &code_value,
        if vcs_signed_in {
            "enter choose"
        } else {
            "enter sign in"
        },
        panel.focus == Focus::Code,
        width,
    );
    let machine_value = match (&setup.host, &setup.offer) {
        (Some(host), _) => host.clone(),
        (None, Some(offer)) => format!("{} · found on this network", offer.label),
        (None, None) => "this machine".to_owned(),
    };
    let machine_row = row(
        true,
        "Machine",
        &machine_value,
        if setup.host.is_none() && setup.offer.is_some() {
            "enter add"
        } else {
            ""
        },
        panel.focus == Focus::Machine,
        width,
    );
    let actions = Line::from(vec![
        action("Create the first task", panel.focus == Focus::Create),
        Span::raw("      "),
        action("Open Settings", panel.focus == Focus::Settings),
    ]);
    frame.render_widget(
        Paragraph::new(vec![name, agent_row, code_row, machine_row]),
        rows,
    );
    frame.render_widget(Paragraph::new(actions), actions_area);

    let code_anchor = Rect {
        y: rows.y + 2,
        height: 1,
        ..rows
    };
    let agent_anchor = Rect {
        y: rows.y + 1,
        height: 1,
        ..rows
    };
    if panel.agent.is_open() {
        panel.agent.draw_list_below(frame, body, agent_anchor);
    } else if let Some(browser) = panel.repositories.as_mut() {
        browser.draw(frame, body, code_anchor);
    } else if panel.editing_name {
        draw_name_editor(frame, detail, panel);
    } else {
        frame.render_widget(
            Paragraph::new(detail_text(panel.focus, setup))
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

fn detail_text(focus: Focus, setup: &SetupState) -> String {
    let vcs = setup.vcs.label();
    match focus {
        Focus::Name => {
            "The project is the work: its repository, its caches, what the agent is told, and the \
             machine it runs on. A name you recognise is enough."
                .to_owned()
        }
        Focus::Agent if setup.agent.is_none() => {
            "The agent is what does the work, in a sandbox of its own. Choose one you can sign in \
             to; either works with any project."
                .to_owned()
        }
        Focus::Agent => {
            "Signing in happens in the agent's own interface, in a throwaway sandbox. What it \
             writes is kept here and mounted into every task."
                .to_owned()
        }
        Focus::Code if !signed_in(&setup.vcs_status) => {
            format!(
                "Sign in to {vcs} so a task can push its work out of the sandbox. Nothing a task \
                 does survives its sandbox; the repository is where it goes."
            )
        }
        Focus::Code if setup.repository.is_none() => {
            "Choose the repository tasks start from. It is cloned into each sandbox before the \
             first turn, and it is where finished work is pushed."
                .to_owned()
        }
        Focus::Code => {
            "The repository a task starts from and pushes back to. Changing it changes where the \
             next task's work goes."
                .to_owned()
        }
        Focus::Create if setup.agent.is_none() => {
            "A task needs an agent. Choose one and sign in, and this is where work starts."
                .to_owned()
        }
        Focus::Create if setup.repository.is_none() => {
            "A task needs a repository to put its work in. Choose one above, and this is where \
             work starts."
                .to_owned()
        }
        Focus::Machine if setup.host.is_some() => format!(
            "This project's tasks run on {}. That machine keeps the code it clones and the caches \
             it builds, so moving the project moves where its work lives.",
            setup.host.as_deref().unwrap_or_default()
        ),
        Focus::Machine if setup.offer.is_some() => {
            "A remote on this network. Adding it lets this project run there, with its own caches, \
             and leaves everything else here. Other machines live in Settings → Remotes."
                .to_owned()
        }
        Focus::Machine => {
            "Tasks run on this machine, in sandboxes thrown away after them. A remote is another \
             machine that can run a project; one on this network would appear here."
                .to_owned()
        }
        Focus::Create => {
            "Say what you want done. Altitude starts an agent on it, and carries its questions \
             back to you from the task list."
                .to_owned()
        }
        Focus::Settings => {
            "Caches, instructions, the machine a project runs on, and any task tracker are all in \
             Settings."
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
