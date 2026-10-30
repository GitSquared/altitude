//! The page that edits one project or remote, and the fields behind it.
//!
//! A project has more to say than fits a modal on a short terminal, so the fields
//! scroll with the caret: the block being edited is always on screen, and the ones
//! above it are one shift-tab away.

use std::mem;

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
    credentials::{Credential, Slot, agent_of},
    project::{Project, ProjectCommits},
    tui::{
        popups::{repositories::Repositories, scopes::Scopes},
        screens::settings::rows::ProjectDefaults,
        widgets::{
            button, chrome, palette,
            select::{self, Offer, Select},
            text::{Field, FieldStyle, TextBox},
            utils::truncate,
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

    /// Whether this field is a pill whose list opens where it stands.
    ///
    /// The repository and the team are pills too, but their choices come from the
    /// network: they open a page of their own rather than a list built here.
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
    vec![
        Group {
            focus: Focus::Primary,
            label: "Name",
            kind: Kind::Single,
        },
        Group {
            focus: Focus::Repository,
            label: "Repository",
            kind: Kind::Choice,
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
    /// The choices, each a value you press and a list you step through. The
    /// value's own type is the answer: `None` is the offer that clears a field,
    /// which is what made "nothing chosen" a state the page could not reach.
    agent: Select<Option<String>>,
    vcs: Select<Option<String>>,
    tracker: Select<Option<String>>,
    identity: Select<Option<usize>>,
    host: Select<Option<String>>,
    pub accepts_delegated_work: bool,
    pub focus: Focus,
    /// How many rows of the form are above the viewport.
    scroll: u16,
    /// Whether the wheel moved the form away from the caret's group.
    free_scroll: bool,
    /// Where each group and action was drawn, for a click.
    hits: Vec<(Rect, Focus)>,
    /// Where an open list drew its rows on the frame, so a click can find them.
    pill_rows: Vec<(Rect, usize)>,
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
        let agent_credential = project.agent_credential.clone().or(defaults.agent.clone());
        let vcs_credential = project.vcs_credential.clone().or(defaults.vcs.clone());
        let tracker_credential = project
            .tracker
            .as_ref()
            .and_then(|route| route.credential.clone())
            .or(defaults.tracker.clone());
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
            agent: credential_pick(
                "agent",
                Slot::Agent,
                &credentials,
                agent_credential.as_deref(),
                "not chosen",
            ),
            vcs: credential_pick(
                "version control",
                Slot::VersionControl,
                &credentials,
                vcs_credential.as_deref(),
                "none",
            ),
            tracker: credential_pick(
                "task tracker",
                Slot::Tracker,
                &credentials,
                tracker_credential.as_deref(),
                "none",
            ),
            identity: identity_pick(&identities, identity_at),
            host: host_pick(&host_choices, project.host.as_deref()),
            identity_choices: identities,
            accepts_delegated_work: project
                .tracker
                .as_ref()
                .is_some_and(|route| route.accepts_delegated_work),
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
            agent: credential_pick("agent", Slot::Agent, &[], None, "not chosen"),
            vcs: credential_pick("version control", Slot::VersionControl, &[], None, "none"),
            tracker: credential_pick("task tracker", Slot::Tracker, &[], None, "none"),
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

    /// The harness the chosen login is for, which is what a task would run.
    pub fn agent(&self) -> Option<AgentKind> {
        agent_of(self.agent_credential().as_deref())
    }

    pub fn vcs_credential(&self) -> Option<String> {
        self.vcs.chosen_value().cloned().flatten()
    }

    pub fn tracker_credential(&self) -> Option<String> {
        self.tracker.chosen_value().cloned().flatten()
    }

    /// Whether one of the pills has its list open, in which case the keys are
    /// that list's and not the page's.
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

    /// Open the focused pill's list. Nothing happens on a field whose list is a
    /// page of its own: those open from the keys that own them.
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

    /// Take a key for the open list. Answers whether it was the list's.
    pub fn list_key(&mut self, key: KeyEvent) -> bool {
        let handled = match self.focus {
            Focus::Agent => self.agent.handle_key(key),
            Focus::VcsIdentity => self.vcs.handle_key(key),
            Focus::TrackerIdentity => self.tracker.handle_key(key),
            Focus::Identity => self.identity.handle_key(key),
            Focus::Host => self.host.handle_key(key),
            _ => false,
        };
        // A team belongs to the tracker it came from, so choosing another one —
        // or none at all — leaves no team behind.
        if handled && self.focus == Focus::TrackerIdentity && !self.tracker.is_open() {
            self.scope = Field::with("");
        }
        handled
    }

    /// What an open list's keys are, for the guidance line.
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

    /// A click on one of the list's rows is the choice, the way it is everywhere
    /// else a list is drawn.
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

    /// Draw the open list over the fields, answering where each row landed.
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

    /// How wide the pill on a field wants to be.
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

/// A pill over the logins this installation holds for one slot.
///
/// The empty offer comes first, so the field can be emptied as well as filled:
/// it means the installation's own answer, which is what an unset field runs on
/// anyway.
fn credential_pick(
    name: &'static str,
    slot: Slot,
    credentials: &[Credential],
    chosen: Option<&str>,
    missing: &str,
) -> Select<Option<String>> {
    let mut offers = vec![Offer::new(None, missing.to_owned())];
    offers.extend(
        credentials
            .iter()
            .filter(|credential| {
                credential
                    .service()
                    .is_some_and(|service| service.slot() == slot)
            })
            .map(|credential| Offer::new(Some(credential.id.clone()), credential.label.clone())),
    );
    let at = offers
        .iter()
        .position(|offer| offer.value.as_deref() == chosen)
        .unwrap_or(0);
    Select::new(name, offers, at)
}

fn identity_pick(identities: &[ProjectCommits], chosen: Option<usize>) -> Select<Option<usize>> {
    let mut offers = vec![Offer::new(None, "the installation's".to_owned())];
    for (index, commits) in identities.iter().enumerate() {
        let label = if commits.label.is_empty() {
            commits.name.clone()
        } else {
            commits.label.clone()
        };
        offers.push(Offer::new(
            Some(index),
            format!("{label} <{}>", commits.email),
        ));
    }
    let at = offers
        .iter()
        .position(|offer| offer.value == chosen)
        .unwrap_or(0);
    Select::new("commit identity", offers, at)
}

fn host_pick(hosts: &[String], chosen: Option<&str>) -> Select<Option<String>> {
    let mut offers = vec![Offer::new(None, "this machine".to_owned())];
    offers.extend(
        hosts
            .iter()
            .cloned()
            .map(|host| Offer::new(Some(host.clone()), host)),
    );
    let at = offers
        .iter()
        .position(|offer| offer.value.as_deref() == chosen)
        .unwrap_or(0);
    Select::new("machine", offers, at)
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
    let mut pill_anchor = None;
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
            if group.focus.is_a_pill() && form.list_open() {
                pill_anchor = Some(anchor);
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
    if let Some(anchor) = pill_anchor {
        form.pill_rows = form.draw_list(frame, fields, anchor);
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
            // A pill beside its label rather than a value under it: a value you
            // can change reads as a control, and the page gets a line back for
            // every one of them.
            let value = match group.focus {
                Focus::Repository => {
                    let value = form.repository.value();
                    if value.trim().is_empty() {
                        "not chosen".to_owned()
                    } else {
                        value.to_owned()
                    }
                }
                Focus::Scope => {
                    let scope = form.scope.value();
                    if scope.trim().is_empty() {
                        "whole account".to_owned()
                    } else {
                        scope.to_owned()
                    }
                }
                focus => form.pick_value(focus),
            };
            let width = match group.focus {
                Focus::Repository | Focus::Scope => {
                    // Its list is a page of its own, so the pill only says what
                    // the field holds and its key opens that page.
                    (value.chars().count() as u16 + 3).min(area.width)
                }
                focus => form.pill_width(focus).min(area.width),
            };
            let pill = Rect {
                x: area.right().saturating_sub(width),
                width,
                height: 1,
                ..area
            };
            match group.focus {
                Focus::Repository | Focus::Scope => {
                    select::shown_pill(buffer, pill, &value, focused)
                }
                focus => form.draw_pill(focus, buffer, pill, focused),
            }
            draw_label(
                buffer,
                Rect {
                    width: area.width.saturating_sub(width).saturating_sub(1),
                    height: 1,
                    ..area
                },
                label,
                focused,
            );
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
    let mark = if focused { "› " } else { "  " };
    let room = usize::from(area.width).saturating_sub(mark.chars().count());
    Paragraph::new(Line::styled(
        format!("{mark}{}", truncate(text, room)),
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
