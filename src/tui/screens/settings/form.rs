//! The page that edits one project or remote, and the fields behind it.
//!
//! A project has more to say than fits a modal on a short terminal, so the fields
//! scroll with the caret: the block being edited is always on screen, and the ones
//! above it are one shift-tab away.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    text::Line,
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Widget, Wrap},
};

use crate::{
    agent::AgentKind,
    credentials::{Credential, Service, Slot},
    project::{Project, ProjectCommits},
    tui::{
        popups::{repositories::Repositories, scopes::Scopes},
        screens::settings::rows::ProjectDefaults,
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
            label: "Commit identity",
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
    identity_at: Option<usize>,
    /// The coding agent new tasks here use, or nothing for the installation's
    /// default.
    pub agent: Option<AgentKind>,
    pub agent_credential: Option<String>,
    pub vcs_credential: Option<String>,
    pub tracker_credential: Option<String>,
    credentials: Vec<Credential>,
    pub host: Option<String>,
    /// The machines offered, besides this one. A stale address already on the
    /// project is kept, so editing does not silently move the work.
    host_choices: Vec<String>,
    pub accepts_delegated_work: bool,
    pub focus: Focus,
    /// How many rows of the form are above the viewport.
    scroll: u16,
    /// Whether the wheel moved the form away from the caret's group.
    free_scroll: bool,
    /// Where each group and action was drawn, for a click.
    hits: Vec<(Rect, Focus)>,
    /// How the tracker's scope is named in this form: a Linear team, a Jira
    /// project, a GitHub repository.
    scope_label: String,
    pub repositories: Option<Repositories>,
    pub scopes: Option<Scopes>,
}

const PROJECT_FIELDS: [Focus; 14] = [
    Focus::Primary,
    Focus::Repository,
    Focus::Setup,
    Focus::Caches,
    Focus::Instructions,
    Focus::Agent,
    Focus::VcsIdentity,
    Focus::TrackerIdentity,
    Focus::Identity,
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
            identity_choices: identities,
            identity_at,
            agent: project.agent,
            agent_credential: project.agent_credential.clone().or(defaults.agent),
            vcs_credential: project.vcs_credential.clone().or(defaults.vcs),
            tracker_credential: project
                .tracker
                .as_ref()
                .and_then(|route| route.credential.clone())
                .or(defaults.tracker),
            credentials,
            host: project.host.clone(),
            host_choices,
            accepts_delegated_work: project
                .tracker
                .as_ref()
                .is_some_and(|route| route.accepts_delegated_work),
            focus: Focus::Primary,
            scroll: 0,
            free_scroll: false,
            hits: Vec::new(),
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
            identity_at: None,
            agent: None,
            agent_credential: None,
            vcs_credential: None,
            tracker_credential: None,
            credentials: Vec::new(),
            host: None,
            host_choices: Vec::new(),
            accepts_delegated_work: false,
            focus: Focus::Primary,
            scroll: 0,
            free_scroll: false,
            hits: Vec::new(),
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
        self.free_scroll = false;
    }

    /// Move the form by a wheel notch, letting the operator read past the
    /// caret's group without moving it.
    pub fn scroll_by(&mut self, up: bool) {
        const ROWS: u16 = 3;
        self.free_scroll = true;
        self.scroll = if up {
            self.scroll.saturating_sub(ROWS)
        } else {
            self.scroll.saturating_add(ROWS)
        };
    }

    /// The field a click lands on, if it lands on the page.
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
            Focus::Repository => self.repository.handle_key(key),
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
        self.host.clone()
    }

    pub fn commits(&self) -> Option<ProjectCommits> {
        self.identity_at
            .and_then(|index| self.identity_choices.get(index).cloned())
    }

    pub fn cycle_identity(&mut self) {
        if self.identity_choices.is_empty() {
            return;
        }
        let at = self.identity_at.unwrap_or(0);
        self.identity_at = Some((at + 1) % self.identity_choices.len());
    }

    fn identity_value(&self) -> String {
        self.commits().map_or_else(
            || "not set".to_owned(),
            |commits| format!("{} <{}>", commits.name, commits.email),
        )
    }

    pub fn cycle_agent(&mut self) {
        let options = self.identities_for(Slot::Agent);
        if options.is_empty() {
            return;
        }
        let at = options
            .iter()
            .position(|option| self.agent_credential.as_ref() == Some(option))
            .unwrap_or(0);
        self.agent_credential = Some(options[(at + 1) % options.len()].clone());
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
        if options.is_empty() {
            return;
        }
        let at = options
            .iter()
            .position(|option| self.vcs_credential.as_ref() == Some(option))
            .unwrap_or(0);
        self.vcs_credential = Some(options[(at + 1) % options.len()].clone());
    }

    pub fn cycle_tracker(&mut self) {
        let options = self.identities_for(Slot::Tracker);
        if options.is_empty() {
            return;
        }
        let at = options
            .iter()
            .position(|option| self.tracker_credential.as_ref() == Some(option))
            .unwrap_or(0);
        self.tracker_credential = Some(options[(at + 1) % options.len()].clone());
        self.scope = Field::with("");
    }

    fn identities_for(&self, slot: Slot) -> Vec<String> {
        self.credentials
            .iter()
            .filter(|credential| {
                credential
                    .service()
                    .is_some_and(|service| service.slot() == slot)
            })
            .map(|credential| credential.id.clone())
            .collect()
    }

    fn identity_label(&self, id: Option<&str>, missing: &str) -> String {
        id.and_then(|id| {
            self.credentials
                .iter()
                .find(|credential| credential.id == id)
                .map(|credential| credential.label.clone())
        })
        .unwrap_or_else(|| missing.to_owned())
    }

    fn agent_value(&self) -> String {
        self.identity_label(self.agent_credential.as_deref(), "not chosen")
    }

    fn vcs_value(&self) -> String {
        self.identity_label(self.vcs_credential.as_deref(), "none")
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
    if fields.width == 0 || fields.height == 0 {
        return;
    }
    let groups = groups(project);
    let mut tops = Vec::with_capacity(groups.len());
    let mut heights = Vec::with_capacity(groups.len());
    let mut top = 0u16;
    for group in &groups {
        let height = group.kind.rows();
        tops.push(top);
        heights.push(height);
        top = top.saturating_add(height).saturating_add(1);
    }
    let content_height = top;
    let max_scroll = content_height.saturating_sub(fields.height);
    form.scroll = form.scroll.min(max_scroll);
    if !form.free_scroll
        && let Some(index) = groups
            .iter()
            .position(|candidate| candidate.focus == form.focus)
    {
        let at = tops[index];
        let height = heights[index];
        if at < form.scroll {
            form.scroll = at;
        } else if at.saturating_add(height) > form.scroll.saturating_add(fields.height) {
            form.scroll = at
                .saturating_add(height)
                .saturating_sub(fields.height)
                .min(at);
        }
        form.scroll = form.scroll.min(max_scroll);
    }

    let mut content = Buffer::empty(Rect::new(0, 0, fields.width, content_height.max(1)));
    form.hits.clear();
    let mut cursor = None;
    let mut repository_anchor = None;
    let mut scope_anchor = None;
    for (index, group) in groups.iter().enumerate() {
        let rect = Rect {
            x: 0,
            y: tops[index],
            width: fields.width,
            height: heights[index],
        };
        let drawn = draw_group(&mut content, rect, form, group);
        if group.focus == form.focus {
            cursor = drawn;
        }
        let screen_y = i32::from(fields.y) + i32::from(tops[index]) - i32::from(form.scroll);
        if screen_y + i32::from(heights[index]) > i32::from(fields.y)
            && screen_y < i32::from(fields.bottom())
        {
            let anchor = Rect {
                y: u16::try_from(screen_y.max(i32::from(fields.y))).unwrap_or(fields.y),
                height: heights[index],
                ..fields
            };
            form.hits.push((anchor, group.focus));
            if group.focus == Focus::Repository {
                repository_anchor = Some(anchor);
            }
            if group.focus == Focus::Scope {
                scope_anchor = Some(anchor);
            }
        }
    }

    let target = frame.buffer_mut();
    for row in 0..fields.height {
        let source = form.scroll.saturating_add(row);
        if source >= content_height {
            break;
        }
        for column in 0..fields.width {
            target[(fields.x + column, fields.y + row)] = content[(column, source)].clone();
        }
    }
    if let Some((x, y)) = cursor
        && y >= form.scroll
        && y < form.scroll.saturating_add(fields.height)
    {
        frame.set_cursor_position((fields.x + x, fields.y + y - form.scroll));
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
    let buttons = button::actions(
        frame,
        area,
        body.actions,
        &[
            button::Action::secondary("cancel").focused(form.focus == Focus::Cancel),
            button::Action::primary("save").focused(form.focus == Focus::Save),
        ],
    );
    for (rect, focus) in buttons.into_iter().zip([Focus::Cancel, Focus::Save]) {
        form.hits.push((rect, focus));
    }
}

fn draw_group(buffer: &mut Buffer, area: Rect, form: &Form, group: &Group) -> Option<(u16, u16)> {
    let focused = form.focus == group.focus;
    match group.kind {
        Kind::Single => {
            let label = if group.focus == Focus::Scope {
                form.scope_label.as_str()
            } else {
                group.label
            };
            draw_label(buffer, area, label, focused);
            let field = match group.focus {
                Focus::Primary => &form.primary,
                Focus::Repository => &form.repository,
                Focus::Scope => &form.scope,
                _ => return None,
            };
            field.render(
                buffer,
                below_label(area),
                &FieldStyle {
                    focused,
                    title: None,
                    prefix: "",
                },
            )
        }
        Kind::Multi => {
            draw_label(buffer, area, group.label, focused);
            let text = match group.focus {
                Focus::Setup => &form.setup,
                Focus::Caches => &form.caches,
                Focus::Instructions => &form.instructions,
                _ => return None,
            };
            text.render_titled(buffer, below_label(area), focused, None)
        }
        Kind::Choice => {
            let label = if group.focus == Focus::Scope {
                form.scope_label.as_str()
            } else {
                group.label
            };
            draw_label(buffer, area, label, focused);
            let value = match group.focus {
                Focus::Agent => form.agent_value(),
                Focus::VcsIdentity => form.vcs_value(),
                Focus::TrackerIdentity => form.tracker_value(),
                Focus::Identity => form.identity_value(),
                Focus::Scope => {
                    let scope = form.scope.value();
                    if scope.trim().is_empty() {
                        "whole account".to_owned()
                    } else {
                        scope.to_owned()
                    }
                }
                _ => form.host.as_deref().unwrap_or("this machine").to_owned(),
            };
            let style = if focused {
                palette::primary().bold()
            } else {
                palette::muted()
            };
            Paragraph::new(Line::styled(format!("‹ {value} ›"), style))
                .render(below_label(area), buffer);
            None
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
            Paragraph::new(Line::styled(format!("{mark} {}", group.label), style))
                .render(Rect { height: 1, ..area }, buffer);
            if area.height > 2 {
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
                .wrap(Wrap { trim: true })
                .render(
                    Rect {
                        y: area.y.saturating_add(2),
                        height: area.height.saturating_sub(2),
                        ..area
                    },
                    buffer,
                );
            }
            None
        }
    }
}

fn draw_label(buffer: &mut Buffer, area: Rect, text: &str, focused: bool) {
    let style = if focused {
        palette::primary().bold()
    } else {
        palette::muted()
    };
    Paragraph::new(Line::styled(
        format!("{}{text}", if focused { "› " } else { "  " }),
        style,
    ))
    .render(
        Rect {
            height: area.height.min(1),
            ..area
        },
        buffer,
    );
}

fn below_label(area: Rect) -> Rect {
    Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    }
}
