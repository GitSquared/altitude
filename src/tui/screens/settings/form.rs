//! The page that edits one project or remote, and the fields behind it.
//!
//! A project has more to say than fits a modal on a short terminal, so the fields
//! scroll with the caret: the block being edited is always on screen, and the ones
//! above it are one shift-tab away.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};

use crate::{
    agent::AgentKind,
    credentials::{Credential, Service, Slot},
    project::{Project, ProjectCommits},
    tui::{
        popups::{repositories::Repositories, scopes::Scopes},
        widgets::{
            button, chrome, palette,
            text::{Field, FieldStyle, TextBox},
        },
    },
    vcs::Repository,
};

/// What an action applies to: the list it saves into, and what it replaces.
///
/// One type for the form and for the confirmation, because both need to know
/// which list they are about and which row they are about.
#[derive(Clone, PartialEq, Eq)]
pub enum Target {
    NewProject,
    /// The project being edited, so the fields this form does not show survive
    /// the save.
    Project {
        id: String,
    },
    NewRemote,
    Remote {
        original: String,
    },
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
    CommitName,
    CommitEmail,
    Signing,
    Host,
    Accept,
    Cancel,
    Save,
}

impl Focus {
    pub fn takes_text(self) -> bool {
        matches!(
            self,
            Self::Primary
                | Self::Repository
                | Self::Setup
                | Self::Caches
                | Self::Instructions
                | Self::CommitName
                | Self::CommitEmail
                | Self::Signing
                | Self::Scope
        )
    }

    fn is_multiline(self) -> bool {
        matches!(self, Self::Setup | Self::Caches | Self::Instructions)
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
            Self::Choice => 3,
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
    vec![
        Group {
            focus: Focus::Primary,
            label: "Name",
            kind: Kind::Single,
        },
        Group {
            focus: Focus::Repository,
            label: "Repository (host/owner/repo)",
            kind: Kind::Single,
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
        Group {
            focus: Focus::Instructions,
            label: "Instructions for the agent",
            kind: Kind::Multi,
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
            focus: Focus::TrackerIdentity,
            label: "Task tracker",
            kind: Kind::Choice,
        },
        Group {
            focus: Focus::Identity,
            label: "Git identity",
            kind: Kind::Choice,
        },
        Group {
            focus: Focus::CommitName,
            label: "Commit name",
            kind: Kind::Single,
        },
        Group {
            focus: Focus::CommitEmail,
            label: "Commit email",
            kind: Kind::Single,
        },
        Group {
            focus: Focus::Signing,
            label: "Signing key (optional)",
            kind: Kind::Single,
        },
        Group {
            focus: Focus::Scope,
            // Overridden with the provider's own word when the form is drawn.
            label: "Task tracker team",
            kind: Kind::Single,
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
    pub commit_name: Field,
    pub commit_email: Field,
    pub signing: Field,
    identity_choices: Vec<ProjectCommits>,
    /// The installation's own identity, which a project inherits when it stores
    /// none.
    default_commits: Option<ProjectCommits>,
    identity_at: Option<usize>,
    /// The coding agent new tasks here use, or nothing for the installation's
    /// default.
    pub agent: Option<AgentKind>,
    pub agent_credential: Option<String>,
    pub vcs_credential: Option<String>,
    pub tracker_credential: Option<String>,
    credentials: Vec<Credential>,
    default_agent: Option<AgentKind>,
    pub host: Option<String>,
    /// The machines offered, besides this one. A stale address already on the
    /// project is kept, so editing does not silently move the work.
    host_choices: Vec<String>,
    pub accepts_delegated_work: bool,
    pub focus: Focus,
    first_group: usize,
    /// How the tracker's scope is named in this form: a Linear team, a Jira
    /// project, a GitHub repository.
    scope_label: String,
    pub repositories: Option<Repositories>,
    pub scopes: Option<Scopes>,
}

const PROJECT_FIELDS: [Focus; 17] = [
    Focus::Primary,
    Focus::Repository,
    Focus::Setup,
    Focus::Caches,
    Focus::Instructions,
    Focus::Agent,
    Focus::VcsIdentity,
    Focus::TrackerIdentity,
    Focus::Identity,
    Focus::CommitName,
    Focus::CommitEmail,
    Focus::Signing,
    Focus::Scope,
    Focus::Host,
    Focus::Accept,
    Focus::Cancel,
    Focus::Save,
];

const REMOTE_FIELDS: [Focus; 3] = [Focus::Primary, Focus::Cancel, Focus::Save];

impl Form {
    /// The form for a project that does not exist yet, with the caches a new
    /// project starts with already filled in.
    pub fn new_project(
        scope_label: &str,
        remotes: &[String],
        default_agent: Option<AgentKind>,
        default_commits: Option<ProjectCommits>,
        identities: Vec<ProjectCommits>,
        credentials: Vec<Credential>,
    ) -> Self {
        let project = Project::new("");
        Self::for_project(
            Target::NewProject,
            &project,
            scope_label,
            remotes,
            default_agent,
            default_commits,
            identities,
            credentials,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn for_project(
        target: Target,
        project: &Project,
        scope_label: &str,
        remotes: &[String],
        default_agent: Option<AgentKind>,
        default_commits: Option<ProjectCommits>,
        identities: Vec<ProjectCommits>,
        credentials: Vec<Credential>,
    ) -> Self {
        let commits = project.commits.clone().or_else(|| default_commits.clone());
        let identity_at = project
            .commits
            .as_ref()
            .and_then(|commits| identities.iter().position(|known| known == commits));
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
            scope: Field::with(
                project
                    .tracker
                    .as_ref()
                    .and_then(|route| route.scope.as_deref())
                    .unwrap_or_default(),
            ),
            commit_name: Field::with(commits.as_ref().map_or("", |commits| commits.name.as_str())),
            commit_email: Field::with(
                commits
                    .as_ref()
                    .map_or("", |commits| commits.email.as_str()),
            ),
            signing: Field::with(
                commits
                    .as_ref()
                    .and_then(|commits| commits.signing_key.as_deref())
                    .unwrap_or_default(),
            ),
            identity_choices: identities,
            default_commits,
            identity_at,
            agent: project.agent,
            agent_credential: project.agent_credential.clone(),
            vcs_credential: project.vcs_credential.clone(),
            tracker_credential: project
                .tracker
                .as_ref()
                .and_then(|route| route.credential.clone()),
            credentials,
            default_agent,
            host: project.host.clone(),
            host_choices,
            accepts_delegated_work: project
                .tracker
                .as_ref()
                .is_some_and(|route| route.accepts_delegated_work),
            focus: Focus::Primary,
            first_group: 0,
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
            commit_name: Field::with(""),
            commit_email: Field::with(""),
            signing: Field::with(""),
            identity_choices: Vec::new(),
            default_commits: None,
            identity_at: None,
            agent: None,
            agent_credential: None,
            vcs_credential: None,
            tracker_credential: None,
            credentials: Vec::new(),
            default_agent: None,
            host: None,
            host_choices: Vec::new(),
            accepts_delegated_work: false,
            focus: Focus::Primary,
            first_group: 0,
            scope_label: String::new(),
            repositories: None,
            scopes: None,
        }
    }

    pub fn move_focus(&mut self, forward: bool) {
        let fields: &[Focus] = if self.target.is_remote() {
            &REMOTE_FIELDS
        } else {
            &PROJECT_FIELDS
        };
        let at = fields
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % fields.len()
        } else {
            (at + fields.len() - 1) % fields.len()
        };
        self.focus = fields[next];
    }

    pub fn enters_newline(&self) -> bool {
        self.focus.is_multiline()
    }

    pub fn handle_text(&mut self, key: KeyEvent) {
        match self.focus {
            Focus::Primary => self.primary.handle_key(key),
            Focus::Repository => self.repository.handle_key(key),
            Focus::Setup => self.setup.handle_key(key),
            Focus::Caches => self.caches.handle_key(key),
            Focus::Instructions => self.instructions.handle_key(key),
            Focus::CommitName => self.commit_name.handle_key(key),
            Focus::CommitEmail => self.commit_email.handle_key(key),
            Focus::Signing => self.signing.handle_key(key),
            Focus::Scope => self.scope.handle_key(key),
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
        self.host.clone()
    }

    pub fn commits(&self) -> Option<ProjectCommits> {
        let name = self.commit_name.value().trim().to_owned();
        let email = self.commit_email.value().trim().to_owned();
        if name.is_empty() || email.is_empty() {
            return None;
        }
        let signing = self.signing.value().trim();
        Some(ProjectCommits {
            name,
            email,
            signing_key: (!signing.is_empty()).then(|| signing.to_owned()),
        })
    }

    pub fn commits_are_default(&self) -> bool {
        self.commits() == self.default_commits
    }

    /// Move to the next saved identity, filling the fields with it. Nothing is
    /// the installation's default.
    pub fn cycle_identity(&mut self) {
        let mut options: Vec<Option<usize>> = vec![None];
        options.extend((0..self.identity_choices.len()).map(Some));
        let at = options
            .iter()
            .position(|option| *option == self.identity_at)
            .unwrap_or(0);
        self.identity_at = options[(at + 1) % options.len()];
        let commits = self
            .identity_at
            .and_then(|index| self.identity_choices.get(index).cloned())
            .or_else(|| self.default_commits.clone());
        self.commit_name =
            Field::with(commits.as_ref().map_or("", |commits| commits.name.as_str()));
        self.commit_email = Field::with(
            commits
                .as_ref()
                .map_or("", |commits| commits.email.as_str()),
        );
        self.signing = Field::with(
            commits
                .as_ref()
                .and_then(|commits| commits.signing_key.as_deref())
                .unwrap_or_default(),
        );
    }

    fn identity_value(&self) -> String {
        match self.commits() {
            None => "not set".to_owned(),
            Some(commits) if Some(&commits) == self.default_commits.as_ref() => {
                format!("default · {} <{}>", commits.name, commits.email)
            }
            Some(commits) => format!("{} <{}>", commits.name, commits.email),
        }
    }

    /// Move to the next identity for a slot, wrapping. Nothing is the
    /// installation's default, so a project only stores a choice when it differs.
    pub fn cycle_agent(&mut self) {
        let options = self.identities_for(Slot::Agent);
        let at = options
            .iter()
            .position(|option| option == &self.agent_credential)
            .unwrap_or(0);
        self.agent_credential = options[(at + 1) % options.len()].clone();
        self.agent = self
            .agent_credential
            .as_deref()
            .and_then(Service::from_id)
            .and_then(|service| match service {
                Service::Agent(kind) => Some(kind),
                _ => None,
            });
    }

    pub fn cycle_vcs(&mut self) {
        let options = self.identities_for(Slot::VersionControl);
        let at = options
            .iter()
            .position(|option| option == &self.vcs_credential)
            .unwrap_or(0);
        self.vcs_credential = options[(at + 1) % options.len()].clone();
    }

    pub fn cycle_tracker(&mut self) {
        let options = self.identities_for(Slot::Tracker);
        let at = options
            .iter()
            .position(|option| option == &self.tracker_credential)
            .unwrap_or(0);
        self.tracker_credential = options[(at + 1) % options.len()].clone();
        self.scope = Field::with("");
    }

    fn identities_for(&self, slot: Slot) -> Vec<Option<String>> {
        let mut options = vec![None];
        options.extend(
            self.credentials
                .iter()
                .filter(|credential| {
                    credential
                        .service()
                        .is_some_and(|service| service.slot() == slot)
                })
                .map(|credential| Some(credential.id.clone())),
        );
        options
    }

    fn identity_label(&self, id: Option<&str>, default: &str) -> String {
        match id {
            Some(id) => self
                .credentials
                .iter()
                .find(|credential| credential.id == id)
                .map(|credential| credential.label.clone())
                .unwrap_or_else(|| default.to_owned()),
            None => format!("default · {default}"),
        }
    }

    fn agent_value(&self) -> String {
        let default = self
            .default_agent
            .map(|agent| agent.to_string())
            .unwrap_or_else(|| "not chosen".to_owned());
        self.identity_label(self.agent_credential.as_deref(), &default)
    }

    fn vcs_value(&self) -> String {
        self.identity_label(self.vcs_credential.as_deref(), "the installation's login")
    }

    fn tracker_value(&self) -> String {
        self.identity_label(self.tracker_credential.as_deref(), "none")
    }

    pub fn cycle_host(&mut self) {
        let mut options = vec![None];
        options.extend(self.host_choices.iter().cloned().map(Some));
        let at = options
            .iter()
            .position(|option| *option == self.host)
            .unwrap_or(0);
        self.host = options[(at + 1) % options.len()].clone();
    }

    pub fn paste(&mut self, text: &str) {
        match self.focus {
            Focus::Primary => self.primary.paste(text),
            Focus::Repository => self.repository.paste(text),
            Focus::Setup => self.setup.paste(text),
            Focus::Caches => self.caches.paste(text),
            Focus::Instructions => self.instructions.paste(text),
            Focus::CommitName => self.commit_name.paste(text),
            Focus::CommitEmail => self.commit_email.paste(text),
            Focus::Signing => self.signing.paste(text),
            Focus::Scope => self.scope.paste(text),
            _ => {}
        }
    }

    pub fn set_repository(&mut self, url: &str) {
        self.repository = Field::with(url);
    }

    /// Take a mouse event for the repository list, answering the repository a
    /// click chose. The wheel only moves the keys.
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

pub(super) fn draw_form(frame: &mut Frame, area: Rect, form: &mut Form, tick: Option<usize>) {
    let project = !form.target.is_remote();
    let area = if project {
        chrome::centered_at_most(area, 80, 84, area.height.saturating_sub(2).clamp(10, 40))
    } else {
        chrome::centered_at_most(area, 60, 60, chrome::modal_height(5))
    };
    let block = Block::bordered()
        .title(format!(" {} ", form.target.title()))
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(2));
    frame.render_widget(Clear, area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 6 {
        return;
    }

    let body = chrome::body(inner);
    let fields = body.content;
    let groups = groups(project);
    // The caret's group must be on screen, so the window of drawn groups starts
    // at the first one that lets it fit. The actions are not groups, so being on
    // one keeps the last group in view rather than jumping back to the top.
    let focused = groups
        .iter()
        .position(|candidate| candidate.focus == form.focus)
        .unwrap_or(groups.len().saturating_sub(1));
    form.first_group = form.first_group.min(focused);
    loop {
        let used: u16 = groups
            .iter()
            .skip(form.first_group)
            .map(|candidate| candidate.kind.rows() + 1)
            .sum();
        if used <= fields.height || form.first_group == focused {
            break;
        }
        form.first_group += 1;
    }

    let mut y = fields.y;
    let mut repository_anchor = None;
    let mut scope_anchor = None;
    for candidate in groups.iter().skip(form.first_group) {
        let height = candidate.kind.rows();
        if y + height > fields.bottom() {
            break;
        }
        let rect = Rect {
            y,
            height,
            ..fields
        };
        if candidate.focus == Focus::Repository {
            repository_anchor = Some(rect);
        }
        if candidate.focus == Focus::Scope {
            scope_anchor = Some(rect);
        }
        draw_group(frame, rect, form, candidate);
        y += height + 1;
    }
    if let Some(anchor) = repository_anchor
        && let Some(browser) = form.repositories.as_mut()
    {
        browser.draw(frame, fields, anchor, tick);
    }
    if let Some(anchor) = scope_anchor
        && let Some(scopes) = form.scopes.as_mut()
    {
        scopes.draw(frame, fields, anchor, tick);
    }

    // The actions sit at the bottom right, where the eye ends up.
    button::actions(
        frame,
        area,
        body.actions,
        &[
            button::Action::secondary("cancel").focused(form.focus == Focus::Cancel),
            button::Action::primary("save").focused(form.focus == Focus::Save),
        ],
    );
}

fn draw_group(frame: &mut Frame, area: Rect, form: &Form, group: &Group) {
    let focused = form.focus == group.focus;
    match group.kind {
        Kind::Single => {
            let label = if group.focus == Focus::Scope {
                form.scope_label.as_str()
            } else {
                group.label
            };
            draw_label(frame, area, label);
            let field = match group.focus {
                Focus::Primary => &form.primary,
                Focus::Repository => &form.repository,
                Focus::CommitName => &form.commit_name,
                Focus::CommitEmail => &form.commit_email,
                Focus::Signing => &form.signing,
                Focus::Scope => &form.scope,
                _ => return,
            };
            field.draw(
                frame,
                below_label(area),
                &FieldStyle {
                    focused,
                    title: None,
                    prefix: "",
                },
            );
        }
        Kind::Multi => {
            draw_label(frame, area, group.label);
            let text = match group.focus {
                Focus::Setup => &form.setup,
                Focus::Caches => &form.caches,
                Focus::Instructions => &form.instructions,
                _ => return,
            };
            text.draw(frame, below_label(area), focused);
        }
        Kind::Choice => {
            draw_label(frame, area, group.label);
            let value = match group.focus {
                Focus::Agent => form.agent_value(),
                Focus::VcsIdentity => form.vcs_value(),
                Focus::TrackerIdentity => form.tracker_value(),
                Focus::Identity => form.identity_value(),
                _ => form.host.as_deref().unwrap_or("this machine").to_owned(),
            };
            let style = if focused {
                palette::primary().bold()
            } else {
                palette::muted()
            };
            frame.render_widget(
                Paragraph::new(Line::styled(format!("‹ {value} ›"), style)),
                below_label(area),
            );
        }
        Kind::Toggle => {
            let mark = if form.accepts_delegated_work {
                "[x]"
            } else {
                "[ ]"
            };
            let style = if focused {
                palette::primary().bold()
            } else {
                palette::muted()
            };
            frame.render_widget(
                Paragraph::new(Line::styled(format!("{mark} {}", group.label), style)),
                Rect { height: 1, ..area },
            );
            if area.height > 2 {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::styled(
                            "If a remote and task tracker are connected, turn this on to let \
                             your task tracker start altitude agents on @mention and delegation.",
                            palette::muted(),
                        ),
                        Line::from(""),
                        Line::styled(
                            "This enables you to pilot your task entirely from your task tracker.",
                            palette::muted(),
                        ),
                    ])
                    .wrap(Wrap { trim: true }),
                    Rect {
                        y: area.y.saturating_add(2),
                        height: area.height.saturating_sub(2),
                        ..area
                    },
                );
            }
        }
    }
}

fn draw_label(frame: &mut Frame, area: Rect, text: &str) {
    frame.render_widget(
        Paragraph::new(Line::styled(text, palette::muted())),
        Rect {
            height: area.height.min(1),
            ..area
        },
    );
}

fn below_label(area: Rect) -> Rect {
    Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    }
}
