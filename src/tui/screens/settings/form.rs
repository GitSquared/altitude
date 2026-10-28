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
    project::Project,
    tui::{
        popups::{new_task::AGENTS, repositories::Repositories, scopes::Scopes},
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

    /// What the form replaces, if anything.
    pub fn original(&self) -> Option<&str> {
        match self {
            Self::Remote { original } => Some(original),
            _ => None,
        }
    }

    /// The project this form edits, when it edits one.
    pub fn project_id(&self) -> Option<&str> {
        match self {
            Self::Project { id } => Some(id),
            _ => None,
        }
    }
}

/// Which part of the form holds the caret.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Primary,
    Repository,
    Setup,
    Caches,
    Instructions,
    Scope,
    Agent,
    Host,
    Accept,
    Cancel,
    Save,
}

impl Focus {
    /// Whether this part takes typed text, and so owns enter and escape.
    pub fn takes_text(self) -> bool {
        matches!(
            self,
            Self::Primary
                | Self::Repository
                | Self::Setup
                | Self::Caches
                | Self::Instructions
                | Self::Scope
        )
    }

    /// Whether enter is a newline here rather than a move to the next field.
    fn is_multiline(self) -> bool {
        matches!(self, Self::Setup | Self::Caches | Self::Instructions)
    }
}

/// What one block of the form is, which decides how tall it is and what it draws.
#[derive(Clone, Copy)]
enum Kind {
    Single,
    Multi,
    Choice,
    Toggle,
}

impl Kind {
    /// The rows a block needs, its label included.
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

/// A page over the panel, holding one project or one remote.
pub struct Form {
    pub target: Target,
    /// The name, or the remote's address.
    pub primary: Field,
    pub repository: Field,
    pub setup: TextBox,
    pub caches: TextBox,
    pub instructions: TextBox,
    pub scope: Field,
    /// The coding agent new tasks here use, or nothing for the installation's
    /// default.
    pub agent: Option<AgentKind>,
    /// The installation's default agent, shown so "default" has a name.
    default_agent: Option<AgentKind>,
    /// Where this project's tasks run, or nothing for this machine.
    pub host: Option<String>,
    /// The machines offered, besides this one. A stale address already on the
    /// project is kept, so editing does not silently move the work.
    host_choices: Vec<String>,
    pub accepts_delegated_work: bool,
    pub focus: Focus,
    /// The first group drawn, so the caret's group is always on screen.
    first_group: usize,
    /// How the tracker's scope is named in this form: a Linear team, a Jira
    /// project, a GitHub repository.
    scope_label: String,
    /// The repository list, while it is open over the form.
    pub repositories: Option<Repositories>,
    /// The tracker's teams, while its picker is open over the form.
    pub scopes: Option<Scopes>,
}

/// Every focus a project form walks, in order.
const PROJECT_FIELDS: [Focus; 11] = [
    Focus::Primary,
    Focus::Repository,
    Focus::Setup,
    Focus::Caches,
    Focus::Instructions,
    Focus::Agent,
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
    ) -> Self {
        let project = Project::new("");
        Self::for_project(
            Target::NewProject,
            &project,
            scope_label,
            remotes,
            default_agent,
        )
    }

    pub fn for_project(
        target: Target,
        project: &Project,
        scope_label: &str,
        remotes: &[String],
        default_agent: Option<AgentKind>,
    ) -> Self {
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
            agent: project.agent,
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
            agent: None,
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

    /// Move the caret to the next or previous field, wrapping.
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

    /// Whether enter means a newline rather than the next field.
    pub fn enters_newline(&self) -> bool {
        self.focus.is_multiline()
    }

    /// Give a key to the box that has the caret.
    pub fn handle_text(&mut self, key: KeyEvent) {
        match self.focus {
            Focus::Primary => self.primary.handle_key(key),
            Focus::Repository => self.repository.handle_key(key),
            Focus::Setup => self.setup.handle_key(key),
            Focus::Caches => self.caches.handle_key(key),
            Focus::Instructions => self.instructions.handle_key(key),
            Focus::Scope => self.scope.handle_key(key),
            _ => {}
        }
    }

    /// Whether the form is ready to save, and why not when it is not.
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

    /// Where the project runs, or nothing for this machine.
    pub fn host_value(&self) -> Option<String> {
        self.host.clone()
    }

    /// Move to the next agent on offer, wrapping. Nothing is the installation's
    /// default, so a project only stores a choice when it differs.
    pub fn cycle_agent(&mut self) {
        let mut options: Vec<Option<AgentKind>> = vec![None];
        options.extend(AGENTS.iter().copied().map(Some));
        let at = options
            .iter()
            .position(|option| *option == self.agent)
            .unwrap_or(0);
        self.agent = options[(at + 1) % options.len()];
    }

    /// How the agent choice reads: the installation's default by name, or the
    /// project's own choice.
    fn agent_value(&self) -> String {
        match (self.agent, self.default_agent) {
            (Some(agent), _) => agent.to_string(),
            (None, Some(default)) => format!("default · {default}"),
            (None, None) => "not chosen".to_owned(),
        }
    }

    /// Move to the next machine on offer, wrapping.
    pub fn cycle_host(&mut self) {
        let mut options = vec![None];
        options.extend(self.host_choices.iter().cloned().map(Some));
        let at = options
            .iter()
            .position(|option| *option == self.host)
            .unwrap_or(0);
        self.host = options[(at + 1) % options.len()].clone();
    }

    /// Give pasted text to the box that has the caret.
    pub fn paste(&mut self, text: &str) {
        match self.focus {
            Focus::Primary => self.primary.paste(text),
            Focus::Repository => self.repository.paste(text),
            Focus::Setup => self.setup.paste(text),
            Focus::Caches => self.caches.paste(text),
            Focus::Instructions => self.instructions.paste(text),
            Focus::Scope => self.scope.paste(text),
            _ => {}
        }
    }

    /// Take the repository a picker chose.
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

    /// The caches the form lists, one per line, without blanks.
    pub fn cache_paths(&self) -> Vec<String> {
        self.caches
            .value()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// What the instructions box says, when it says anything.
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

/// The box a field's input is drawn in, one row below its label.
fn below_label(area: Rect) -> Rect {
    Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    }
}
