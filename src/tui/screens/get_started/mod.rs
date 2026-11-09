use std::slice;

use crate::{
    agent::AgentKind,
    tracker::TrackerKind,
    tui::{
        app::{CredentialStatus, TrackerState},
        popups::repositories::Repositories,
        widgets::{
            chrome::Hint,
            focus::FocusRing,
            select::{Offer, Select},
            text::Field,
        },
    },
    vcs::{Repository, VcsProviderKind},
};

use super::settings::GitIdentity;

const CARD_WIDTH: u16 = 80;

const LABEL_WIDTH: usize = 15;

const ACTION_WIDTH: usize = 16;

const HELP_ROWS: u16 = 6;

mod draw;
mod rows;

pub use draw::draw;
use rows::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    Done,
    Blocking,
    Unset,
    Question,
}

impl Marker {
    fn glyph(self) -> &'static str {
        match self {
            Self::Done => "✓",
            Self::Blocking => "●",
            Self::Unset => "○",
            Self::Question => "?",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Name,
    Agent,
    VersionControl,
    Commits,
    Tracker,
    Remote,
    Ready,
    Settings,
}

#[derive(Clone)]
pub struct RemoteOffer {
    pub label: String,
    pub address: String,
}

#[derive(Clone)]
pub struct SetupState {
    pub agent: Option<AgentKind>,
    pub agent_status: CredentialStatus,
    pub vcs: VcsProviderKind,
    pub vcs_status: CredentialStatus,
    pub vcs_listing: bool,
    pub repository: Option<String>,
    pub host: Option<String>,
    pub offer: Option<RemoteOffer>,
    pub tracker: TrackerState,
    pub commits: GitIdentity,
    pub has_key: bool,
}

pub struct Commits {
    pub open: bool,
    pub name: Field,
    pub email: Field,
    pub signs: bool,
    pub field: CommitsField,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CommitsField {
    Name,
    Email,
    Sign,
    Key,
}

const COMMITS_ROWS: [CommitsField; 4] = [
    CommitsField::Name,
    CommitsField::Email,
    CommitsField::Sign,
    CommitsField::Key,
];

impl Commits {
    fn from(identity: &GitIdentity) -> Self {
        Self {
            open: false,
            name: Field::with(identity.name.as_deref().unwrap_or_default()),
            email: Field::with(identity.email.as_deref().unwrap_or_default()),
            signs: identity.signs_commits,
            field: CommitsField::Name,
        }
    }

    pub fn reload(&mut self, identity: &GitIdentity) {
        *self = Self::from(identity);
    }

    pub fn move_focus(&mut self, forward: bool) {
        let at = COMMITS_ROWS
            .iter()
            .position(|field| *field == self.field)
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % COMMITS_ROWS.len()
        } else {
            (at + COMMITS_ROWS.len() - 1) % COMMITS_ROWS.len()
        };
        self.field = COMMITS_ROWS[next];
    }

    pub fn takes_text(&self) -> bool {
        matches!(self.field, CommitsField::Name | CommitsField::Email)
    }
}

pub struct GetStarted {
    pub focus: Focus,
    // Rebuilt by every draw, so it describes the card on screen rather than the
    // card as it was when someone last edited a constant beside it.
    focus_ring: FocusRing<Focus>,
    pub name: Field,
    pub name_editing: bool,
    pub agent: Select<AgentKind>,
    pub vcs: Select<VcsProviderKind>,
    pub tracker: Select<TrackerKind>,
    pub repository: Select<Repository>,
    pub commits: Commits,
    pub repositories: Option<Repositories>,
}

impl GetStarted {
    pub fn new(
        name: &str,
        agent: Option<AgentKind>,
        repository: Option<&str>,
        cached_repositories: &[Repository],
        identity: &GitIdentity,
        agents: &[AgentKind],
    ) -> Self {
        let chosen = agent
            .and_then(|chosen| agents.iter().position(|candidate| *candidate == chosen))
            .unwrap_or(0);
        let focus = if name.trim().is_empty() {
            Focus::Name
        } else if agent.is_none() {
            Focus::Agent
        } else if repository.is_none() {
            Focus::VersionControl
        } else {
            Focus::Ready
        };
        Self {
            focus,
            focus_ring: FocusRing::default(),
            name: Field::with(name),
            name_editing: false,
            commits: Commits::from(identity),
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
            vcs: Select::new(
                "version control",
                VcsProviderKind::ALL
                    .iter()
                    .copied()
                    .map(|kind| Offer::new(kind, kind.label()))
                    .collect(),
                0,
            ),
            tracker: Select::new(
                "task tracker",
                TrackerKind::ALL
                    .iter()
                    .copied()
                    .map(|kind| Offer::new(kind, kind.label()))
                    .collect(),
                0,
            ),
            repository: repository_select(cached_repositories, repository),
        }
    }

    pub fn move_focus(&mut self, forward: bool) {
        self.focus = self.focus_ring.next(self.focus, forward);
    }

    pub fn is_typing(&self) -> bool {
        self.name_editing
            || (self.commits.open && self.commits.takes_text())
            || self.agent.is_filtering()
            || self.vcs.is_filtering()
            || self.tracker.is_filtering()
            || self
                .repositories
                .as_ref()
                .is_some_and(|browser| browser.select.is_filtering())
    }

    pub fn choose_repository(&mut self, repository: &Repository) {
        self.repository = repository_select(slice::from_ref(repository), Some(&repository.url));
    }

    pub fn reset_name(&mut self, name: &str) {
        self.name = Field::with(name);
    }

    pub fn hints(&self, setup: &SetupState) -> Vec<Hint> {
        if self.commits.open {
            return match self.commits.field {
                CommitsField::Name | CommitsField::Email => vec![
                    Hint::new("type", "the value"),
                    Hint::new("tab", "next"),
                    Hint::new("↩", "save"),
                    Hint::new("esc", "save and leave"),
                ],
                CommitsField::Sign => vec![
                    Hint::new("↩", "turn it on or off"),
                    Hint::new("tab", "next"),
                    Hint::new("esc", "save and leave"),
                ],
                CommitsField::Key => vec![
                    Hint::new("↩", "show or make the key"),
                    Hint::new("tab", "next"),
                    Hint::new("esc", "save and leave"),
                ],
            };
        }
        if let Some(browser) = &self.repositories {
            return browser.select.hints();
        }
        if self.agent.is_open() {
            return self.agent.hints();
        }
        if self.vcs.is_open() {
            return self.vcs.hints();
        }
        if self.tracker.is_open() {
            return self.tracker.hints();
        }
        if self.name_editing {
            return vec![
                Hint::new("type", "the name"),
                Hint::new("↩", "save"),
                Hint::new("esc", "save and leave"),
            ];
        }
        if self.focus == Focus::Name {
            return vec![Hint::new("↩", "edit the name"), Hint::new("j/k", "move")];
        }
        let mut hints = vec![Hint::new("j/k", "move")];
        match self.focus {
            Focus::Agent => match setup.agent {
                Some(_) => {
                    hints.insert(0, Hint::new("↩", "sign in"));
                    hints.insert(1, Hint::new("space", "change"));
                }
                None => hints.insert(0, Hint::new("↩", "choose")),
            },
            Focus::VersionControl => {
                hints.insert(
                    0,
                    Hint::new(
                        "↩",
                        if signed_in(&setup.vcs_status) {
                            "choose repository"
                        } else {
                            "choose provider"
                        },
                    ),
                );
            }
            Focus::Commits => hints.insert(0, Hint::new("↩", "edit")),
            Focus::Tracker => hints.insert(
                0,
                Hint::new(
                    "↩",
                    if setup.tracker == TrackerState::Connected {
                        "manage"
                    } else {
                        "connect"
                    },
                ),
            ),
            Focus::Remote => {
                if setup.host.is_none() && setup.offer.is_some() {
                    hints.insert(0, Hint::new("↩", "add it"));
                }
            }
            Focus::Ready => hints.insert(
                0,
                Hint::new(
                    "↩",
                    if ready(setup, &self.name) {
                        "start"
                    } else {
                        "not yet"
                    },
                ),
            ),
            Focus::Settings => hints.insert(0, Hint::new("↩", "open settings")),
            Focus::Name => {}
        }
        hints
    }
}

pub fn ready(setup: &SetupState, name: &Field) -> bool {
    !name.is_blank()
        && setup.agent.is_some()
        && signed_in(&setup.agent_status)
        && setup.repository.is_some()
        && signed_in(&setup.vcs_status)
        && setup.commits.is_set()
}

pub fn signed_in(status: &CredentialStatus) -> bool {
    matches!(
        status,
        CredentialStatus::Checked {
            signed_in: true,
            ..
        } | CredentialStatus::Saved
    )
}
