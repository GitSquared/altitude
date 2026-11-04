use std::mem;

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
};

use crate::{
    agent::AgentKind,
    credentials::{Credential, Service, Slot, agent_of},
    project::{Project, ProjectCommits},
    tui::{
        popups::{repositories::Repositories, scopes::Scopes},
        screens::settings::rows::ProjectDefaults,
        widgets::{
            chrome,
            focus::FocusRing,
            select::Select,
            text::{Field, TextBox},
        },
    },
    vcs::Repository,
    vcs::VcsProviderKind,
};

mod choices;
mod draw;

use choices::{credential_pick, host_pick, identity_pick};
pub use draw::draw_form;

#[derive(Clone, PartialEq, Eq)]
pub enum Target {
    NewProject,
    Project { id: String },
    NewRemote,
    Remote { original: String },
}

impl Target {
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::NewRemote | Self::Remote { .. })
    }

    pub fn title(&self) -> &'static str {
        match self {
            Self::NewProject => "New project",
            Self::Project { .. } => "Edit project",
            Self::NewRemote => "Add remote",
            Self::Remote { .. } => "Edit remote",
        }
    }

    pub fn original(&self) -> Option<&str> {
        match self {
            Self::Remote { original } => Some(original),
            _ => None,
        }
    }

    pub fn project_id(&self) -> Option<&str> {
        match self {
            Self::Project { id } => Some(id),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Primary,
    Repository,
    Setup,
    Caches,
    Instructions,
    Scope,
    Agent,
    VcsIdentity,
    TrackerIdentity,
    Identity,
    Host,
    Accept,
    Cancel,
    Save,
}

impl Focus {
    pub fn takes_text(self) -> bool {
        matches!(
            self,
            Self::Primary | Self::Repository | Self::Setup | Self::Caches | Self::Instructions
        )
    }

    pub fn is_multiline(self) -> bool {
        matches!(self, Self::Setup | Self::Caches | Self::Instructions)
    }

    pub fn is_a_pill(self) -> bool {
        matches!(
            self,
            Self::Agent | Self::VcsIdentity | Self::TrackerIdentity | Self::Identity | Self::Host
        )
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Single,
    Multi,
    Choice,
    Toggle,
}

impl Kind {
    fn rows(self) -> u16 {
        match self {
            Self::Single => 4,
            Self::Multi => 6,
            Self::Toggle => 8,
            // One line: the label, and the value as a pill at the right edge.
            Self::Choice => 1,
        }
    }
}

struct Group {
    focus: Focus,
    label: &'static str,
    kind: Kind,
}

fn groups(project: bool) -> Vec<Group> {
    if !project {
        return vec![Group {
            focus: Focus::Primary,
            label: "Address",
            kind: Kind::Single,
        }];
    }
    // The order is the order the work is decided in: what it is called and what
    // runs it, then the code it changes and who commits it, then where its issues
    // come from and whether it takes them, then what the agent is told, and last
    // the two that only matter once a build has to happen.
    vec![
        Group {
            focus: Focus::Primary,
            label: "Name",
            kind: Kind::Single,
        },
        Group {
            focus: Focus::Agent,
            label: "Coding agent",
            kind: Kind::Choice,
        },
        Group {
            focus: Focus::VcsIdentity,
            label: "Version control",
            kind: Kind::Choice,
        },
        Group {
            focus: Focus::Repository,
            label: "Repository",
            kind: Kind::Choice,
        },
        Group {
            focus: Focus::Identity,
            label: "Commit identity",
            kind: Kind::Choice,
        },
        Group {
            focus: Focus::TrackerIdentity,
            label: "Task tracker",
            kind: Kind::Choice,
        },
        Group {
            focus: Focus::Scope,
            label: "Task tracker team",
            kind: Kind::Choice,
        },
        Group {
            focus: Focus::Host,
            label: "Start new tasks on",
            kind: Kind::Choice,
        },
        Group {
            focus: Focus::Accept,
            label: "Accept delegated work",
            kind: Kind::Toggle,
        },
        Group {
            focus: Focus::Instructions,
            label: "Instructions for the agent",
            kind: Kind::Multi,
        },
        Group {
            focus: Focus::Setup,
            label: "Setup command",
            kind: Kind::Multi,
        },
        Group {
            focus: Focus::Caches,
            label: "Caches, one path per line",
            kind: Kind::Multi,
        },
    ]
}

pub struct Form {
    pub target: Target,
    pub primary: Field,
    pub repository: Field,
    pub setup: TextBox,
    pub caches: TextBox,
    pub instructions: TextBox,
    pub scope: Field,
    identity_choices: Vec<ProjectCommits>,
    agent: Select<Option<String>>,
    vcs: Select<Option<String>>,
    tracker: Select<Option<String>>,
    identity: Select<Option<usize>>,
    host: Select<Option<String>>,
    pub accepts_delegated_work: bool,
    pub focus: Focus,
    scroll: u16,
    free_scroll: bool,
    hits: Vec<(Rect, Focus)>,
    pill_rows: Vec<(Rect, usize)>,
    scope_label: String,
    pub repositories: Option<Repositories>,
    pub scopes: Option<Scopes>,
}

impl Form {
    pub fn new_project(
        scope_label: &str,
        remotes: &[String],
        defaults: ProjectDefaults,
        identities: Vec<ProjectCommits>,
        credentials: Vec<Credential>,
    ) -> Self {
        let project = Project::new("");
        Self::for_project(
            Target::NewProject,
            &project,
            scope_label,
            remotes,
            defaults,
            identities,
            credentials,
        )
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn for_project(
        target: Target,
        project: &Project,
        scope_label: &str,
        remotes: &[String],
        defaults: ProjectDefaults,
        identities: Vec<ProjectCommits>,
        credentials: Vec<Credential>,
    ) -> Self {
        let mut identities = identities;
        let selected_commits = project.commits.clone().or(defaults.commits.clone());
        let identity_at = selected_commits.map(|commits| {
            identities
                .iter()
                .position(|known| *known == commits)
                .unwrap_or_else(|| {
                    identities.push(commits);
                    identities.len() - 1
                })
        });
        let agent_credential = project.agent_credential.clone().or(defaults.agent.clone());
        let vcs_credential = project.vcs_credential.clone().or(defaults.vcs.clone());
        let tracker_credential = project
            .tracker
            .as_ref()
            .and_then(|binding| binding.credential.clone())
            .or(defaults.tracker.clone());
        // A team is the tracker's, so a form with no tracker has no team and
        // nothing to delegate to.
        let has_tracker = tracker_credential.is_some();
        let mut host_choices = remotes.to_vec();
        if let Some(host) = &project.host
            && !host_choices.contains(host)
        {
            host_choices.push(host.clone());
        }
        Self {
            target,
            primary: Field::with(&project.name),
            repository: Field::with(project.repository.as_deref().unwrap_or_default()),
            setup: TextBox::new(&project.setup_command),
            caches: TextBox::new(&project.caches.join("\n")),
            instructions: TextBox::new(project.instructions.as_deref().unwrap_or_default()),
            scope: Field::with(if has_tracker {
                project
                    .tracker
                    .as_ref()
                    .and_then(|binding| binding.scope.as_deref())
                    .unwrap_or_default()
            } else {
                ""
            }),
            // An agent is what runs the work: a project without one is a project
            // that cannot start, so there is no "nothing chosen" to offer.
            agent: credential_pick(
                "agent",
                Slot::Agent,
                &credentials,
                agent_credential.as_deref(),
                None,
            ),
            vcs: credential_pick(
                "version control",
                Slot::VersionControl,
                &credentials,
                vcs_credential.as_deref(),
                Some("none"),
            ),
            tracker: credential_pick(
                "task tracker",
                Slot::Tracker,
                &credentials,
                tracker_credential.as_deref(),
                Some("none"),
            ),
            identity: identity_pick(&identities, identity_at),
            host: host_pick(&host_choices, project.host.as_deref()),
            identity_choices: identities,
            accepts_delegated_work: has_tracker
                && project
                    .tracker
                    .as_ref()
                    .is_some_and(|binding| binding.accepts_delegated_work),
            focus: Focus::Primary,
            scroll: 0,
            free_scroll: false,
            hits: Vec::new(),
            pill_rows: Vec::new(),
            scope_label: scope_label.to_owned(),
            repositories: None,
            scopes: None,
        }
    }

    pub fn for_remote(target: Target, address: &str) -> Self {
        Self {
            target,
            primary: Field::with(address),
            repository: Field::with(""),
            setup: TextBox::new(""),
            caches: TextBox::new(""),
            instructions: TextBox::new(""),
            scope: Field::with(""),
            identity_choices: Vec::new(),
            agent: credential_pick("agent", Slot::Agent, &[], None, None),
            vcs: credential_pick(
                "version control",
                Slot::VersionControl,
                &[],
                None,
                Some("none"),
            ),
            tracker: credential_pick("task tracker", Slot::Tracker, &[], None, Some("none")),
            identity: identity_pick(&[], None),
            host: host_pick(&[], None),
            accepts_delegated_work: false,
            focus: Focus::Primary,
            scroll: 0,
            free_scroll: false,
            hits: Vec::new(),
            pill_rows: Vec::new(),
            scope_label: String::new(),
            repositories: None,
            scopes: None,
        }
    }

    // A form is a list that scrolls, so what is on screen is a window over it and
    // not the list itself: the ring is the fields the form has, in the order they
    // are drawn, which is the order `groups` puts them in. Focusing one that is
    // scrolled out of sight is how the keyboard scrolls the form.
    fn focus_ring(&self) -> FocusRing<Focus> {
        let mut ring = FocusRing::default();
        for group in groups(!self.target.is_remote()) {
            if self.is_enabled(group.focus) {
                ring.add(group.focus);
            }
        }
        ring.add(Focus::Cancel);
        ring.add(Focus::Save);
        ring
    }

    pub fn move_focus(&mut self, forward: bool) {
        self.focus = self.focus_ring().next(self.focus, forward);
        self.free_scroll = false;
    }

    fn is_enabled(&self, focus: Focus) -> bool {
        !matches!(focus, Focus::Scope | Focus::Accept) || self.has_tracker()
    }

    pub fn scroll_by(&mut self, up: bool) {
        const ROWS: u16 = 3;
        self.free_scroll = true;
        self.scroll = if up {
            self.scroll.saturating_sub(ROWS)
        } else {
            self.scroll.saturating_add(ROWS)
        };
    }

    pub fn hit(&self, column: u16, row: u16) -> Option<Focus> {
        self.hits.iter().find_map(|(area, focus)| {
            (column >= area.x && column < area.right() && row >= area.y && row < area.bottom())
                .then_some(*focus)
        })
    }

    pub fn enters_newline(&self) -> bool {
        self.focus.is_multiline()
    }

    pub fn handle_text(&mut self, key: KeyEvent) {
        match self.focus {
            Focus::Primary => self.primary.handle_key(key),
            Focus::Setup => self.setup.handle_key(key),
            Focus::Caches => self.caches.handle_key(key),
            Focus::Instructions => self.instructions.handle_key(key),
            _ => {}
        }
    }

    pub fn refusal(&self) -> Option<&'static str> {
        if self.primary.is_blank() {
            return Some(if self.target.is_remote() {
                "an address is needed"
            } else {
                "a name is needed"
            });
        }
        None
    }

    pub fn host_value(&self) -> Option<String> {
        self.host.chosen_value().cloned().flatten()
    }

    pub fn commits(&self) -> Option<ProjectCommits> {
        let at = self.identity.chosen_value().copied().flatten()?;
        self.identity_choices.get(at).cloned()
    }

    pub fn agent_credential(&self) -> Option<String> {
        self.agent.chosen_value().cloned().flatten()
    }

    pub fn agent(&self) -> Option<AgentKind> {
        agent_of(self.agent_credential().as_deref())
    }

    pub fn vcs_credential(&self) -> Option<String> {
        self.vcs.chosen_value().cloned().flatten()
    }

    fn vcs_provider(&self) -> VcsProviderKind {
        self.vcs_credential()
            .as_deref()
            .and_then(Service::from_id)
            .and_then(|service| match service {
                Service::VersionControl(kind) => Some(kind),
                _ => None,
            })
            .unwrap_or_else(VcsProviderKind::sole)
    }

    pub fn tracker_credential(&self) -> Option<String> {
        self.tracker.chosen_value().cloned().flatten()
    }

    pub fn has_tracker(&self) -> bool {
        self.tracker_credential().is_some()
    }

    pub fn list_open(&self) -> bool {
        match self.focus {
            Focus::Agent => self.agent.is_open(),
            Focus::VcsIdentity => self.vcs.is_open(),
            Focus::TrackerIdentity => self.tracker.is_open(),
            Focus::Identity => self.identity.is_open(),
            Focus::Host => self.host.is_open(),
            _ => false,
        }
    }

    pub fn open_pill(&mut self) {
        match self.focus {
            Focus::Agent => self.agent.open(),
            Focus::VcsIdentity => self.vcs.open(),
            Focus::TrackerIdentity => self.tracker.open(),
            Focus::Identity => self.identity.open(),
            Focus::Host => self.host.open(),
            _ => {}
        }
    }

    pub fn list_key(&mut self, key: KeyEvent) -> bool {
        let tracker = self.tracker_credential();
        let handled = match self.focus {
            Focus::Agent => self.agent.handle_key(key),
            Focus::VcsIdentity => self.vcs.handle_key(key),
            Focus::TrackerIdentity => self.tracker.handle_key(key),
            Focus::Identity => self.identity.handle_key(key),
            Focus::Host => self.host.handle_key(key),
            _ => false,
        };
        if handled && self.focus == Focus::TrackerIdentity && !self.tracker.is_open() {
            // A team belongs to the tracker it came from, so choosing another
            // one — or none at all — leaves no team behind. Closing the list is
            // not choosing.
            if self.tracker_credential() != tracker {
                self.scope = Field::with("");
            }
            // Nothing is delegated when there is no tracker to delegate from.
            if self.tracker_credential().is_none() {
                self.accepts_delegated_work = false;
            }
        }
        handled
    }

    pub fn list_hints(&self) -> Option<Vec<chrome::Hint>> {
        match self.focus {
            Focus::Agent if self.agent.is_open() => Some(self.agent.hints()),
            Focus::VcsIdentity if self.vcs.is_open() => Some(self.vcs.hints()),
            Focus::TrackerIdentity if self.tracker.is_open() => Some(self.tracker.hints()),
            Focus::Identity if self.identity.is_open() => Some(self.identity.hints()),
            Focus::Host if self.host.is_open() => Some(self.host.hints()),
            _ => None,
        }
    }

    pub fn pill_mouse(&mut self, mouse: MouseEvent) -> bool {
        let rows = mem::take(&mut self.pill_rows);
        let taken = match self.focus {
            Focus::Agent => self.agent.mouse(mouse, &rows),
            Focus::VcsIdentity => self.vcs.mouse(mouse, &rows),
            Focus::TrackerIdentity => self.tracker.mouse(mouse, &rows),
            Focus::Identity => self.identity.mouse(mouse, &rows),
            Focus::Host => self.host.mouse(mouse, &rows),
            _ => false,
        };
        self.pill_rows = rows;
        taken
    }

    fn draw_list(&self, frame: &mut Frame, area: Rect, anchor: Rect) -> Vec<(Rect, usize)> {
        match self.focus {
            Focus::Agent => self.agent.draw_list(frame, area, anchor),
            Focus::VcsIdentity => self.vcs.draw_list(frame, area, anchor),
            Focus::TrackerIdentity => self.tracker.draw_list(frame, area, anchor),
            Focus::Identity => self.identity.draw_list(frame, area, anchor),
            Focus::Host => self.host.draw_list(frame, area, anchor),
            _ => Vec::new(),
        }
    }

    fn pill_width(&self, focus: Focus) -> u16 {
        match focus {
            Focus::Agent => self.agent.pill_width(false),
            Focus::VcsIdentity => self.vcs.pill_width(false),
            Focus::TrackerIdentity => self.tracker.pill_width(false),
            Focus::Identity => self.identity.pill_width(false),
            Focus::Host => self.host.pill_width(false),
            _ => 0,
        }
    }

    fn draw_pill(&self, focus: Focus, buffer: &mut Buffer, area: Rect, focused: bool) {
        match focus {
            Focus::Agent => self.agent.draw_pill_in(buffer, area, focused, false),
            Focus::VcsIdentity => self.vcs.draw_pill_in(buffer, area, focused, false),
            Focus::TrackerIdentity => self.tracker.draw_pill_in(buffer, area, focused, false),
            Focus::Identity => self.identity.draw_pill_in(buffer, area, focused, false),
            Focus::Host => self.host.draw_pill_in(buffer, area, focused, false),
            _ => {}
        }
    }

    fn pick_value(&self, focus: Focus) -> String {
        let value = match focus {
            Focus::Agent => self.agent.label().to_owned(),
            Focus::VcsIdentity => self.vcs.label().to_owned(),
            Focus::TrackerIdentity => self.tracker.label().to_owned(),
            Focus::Identity => self.identity.label().to_owned(),
            Focus::Host => self.host.label().to_owned(),
            _ => String::new(),
        };
        value.to_owned()
    }

    pub fn paste(&mut self, text: &str) {
        match self.focus {
            Focus::Primary => self.primary.paste(text),
            Focus::Setup => self.setup.paste(text),
            Focus::Caches => self.caches.paste(text),
            Focus::Instructions => self.instructions.paste(text),
            _ => {}
        }
    }

    pub fn choose_repository(&mut self, url: &str) {
        self.repository = Field::with(url);
    }

    pub fn repository_mouse(&mut self, mouse: MouseEvent) -> Option<Repository> {
        let chosen = self.repositories.as_mut()?.mouse(mouse)?;
        self.repositories = None;
        Some(chosen)
    }

    pub fn cache_paths(&self) -> Vec<String> {
        self.caches
            .value()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    pub fn instruction_text(&self) -> Option<String> {
        let text = self.instructions.value();
        (!text.trim().is_empty()).then(|| text.trim_end().to_owned())
    }
}
