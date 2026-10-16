//! The settings panel: what the interface shows, which agent drives a task and
//! how it signs in, the git identity tasks commit with, the environments a task
//! runs in, and the remotes it can be delegated to.
//!
//! Fullscreen, and three columns. The rail picks a list, the list picks a row,
//! and the third column says what is known about it. Only the column holding the
//! focus carries the inverted bar; the column ahead of it is drawn dimmed, as a
//! preview of where the next step goes, so nothing moves on screen when the
//! focus does.
//!
//! The base environment is built in and therefore has no form: selecting it
//! describes it instead, which is the only place an operator can see what a
//! sandbox is actually made of.

use crate::tui::{
    App,
    app::{CredentialStatus, RemoteStatus, TrackerState},
    popups::new_task::AGENTS,
    widgets::{chrome, palette, utils::Dot},
};

use std::{
    env,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, List, ListItem, Padding, Paragraph, Wrap},
};

use crate::{
    agent::AgentKind,
    isolation::{EnvironmentDescription, ProfileSummary, REFRESH_AFTER},
    remote::{DiscoveredRemote, RemoteKey, TrackerConnection},
    tracker::TrackerKind,
    tui::widgets::{
        text::{Field, FieldStyle, TextBox},
        utils::plain,
    },
};

/// The gap between columns, so two focus bars can never touch.
const GAP: u16 = 3;

// ------------------------------------------------------------------ the lists

/// The lists the panel holds, in the order the rail shows them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Interface,
    Agents,
    VersionControl,
    Trackers,
    Environments,
    Remotes,
}

impl Section {
    pub const ALL: [Section; 6] = [
        Self::Interface,
        Self::Agents,
        Self::VersionControl,
        Self::Trackers,
        Self::Environments,
        Self::Remotes,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Interface => "Interface",
            Self::Agents => "Agents",
            Self::VersionControl => "Version control",
            Self::Trackers => "Task tracker",
            Self::Environments => "Environments",
            Self::Remotes => "Remotes",
        }
    }

    /// Whether this list can grow and shrink.
    ///
    /// Only the environments and the remotes are the operator's to add and
    /// remove: the other lists are what the installation already holds.
    pub fn grows(self) -> bool {
        matches!(self, Self::Environments | Self::Remotes)
    }
}

/// A preference that is on or off.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    KeyboardHints,
}

impl Setting {
    pub fn label(self) -> &'static str {
        match self {
            Self::KeyboardHints => "Keyboard hints",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::KeyboardHints => {
                "List the keys that work, at the bottom of the screen. Press ? at any time to \
                 see them, whether or not this is on."
            }
        }
    }
}

/// How deep the focus is, and therefore how many columns are lit.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Rail,
    List,
    Detail,
}

impl Depth {
    pub fn deeper(self) -> Self {
        match self {
            Self::Rail => Self::List,
            Self::List | Self::Detail => Self::Detail,
        }
    }

    pub fn shallower(self) -> Self {
        match self {
            Self::Rail => Self::Rail,
            Self::Detail => Self::List,
            Self::List => Self::Rail,
        }
    }
}

/// One row of the focused list.
///
/// A row is three places: a mark at the left edge where it has a status of its
/// own, its name, and a note at the right edge where there is more to say about
/// it than the name. Only the name is always there.
pub struct Row {
    /// What the list shows: a name where there is one, an address where there is
    /// not yet.
    pub label: String,
    /// Said at the right edge, so a column of notes lines up rather than each
    /// one sitting wherever its name happens to end.
    pub note: Option<&'static str>,
    pub kind: RowKind,
}

pub enum RowKind {
    /// The built-in environment: described, never edited.
    Base,
    Environment {
        /// The name as configured, which is what an edit replaces.
        name: String,
        setup: String,
        is_default: bool,
    },
    /// A coding agent, which may be the one new tasks use, and the login a task
    /// drives it with.
    Agent {
        kind: AgentKind,
        credential: Credential,
    },
    /// The identity git commits under, read from the version-control profile.
    GitIdentity(GitIdentity),
    /// Something a task logs in with: this installation's key, or a login a task
    /// is given.
    Credential(Credential),
    Remote {
        address: String,
        /// What that address last answered with, if it has been met.
        remembered: Option<RemoteKey>,
        /// What it is doing now: the list says so without being asked.
        status: RemoteStatus,
        /// Whether the configuration it holds is the one this client has.
        needs_config: bool,
        /// What its last announcement said its shared trackers are, when it has
        /// answered. Absent while it has not, which is unknown rather than empty.
        trackers: Option<Vec<TrackerConnection>>,
    },
    /// A remote announcing on the network that this client has not adopted.
    Discovered(DiscoveredRemote),
    /// A tracker Altitude can bind an issue to. Today that is Linear, and the
    /// row says whether its live session is connected.
    Tracker {
        state: TrackerState,
    },
    Switch {
        setting: Setting,
        on: bool,
    },
}

impl Row {
    /// The mark in front of this row, where what it says is asked rather than
    /// known: a remote's address, a profile's login, or a git identity.
    fn dot(&self) -> Option<Dot> {
        match &self.kind {
            RowKind::Remote {
                status: RemoteStatus::Reached,
                needs_config: true,
                ..
            } => Some(mark(false, true)),
            RowKind::Remote { status, .. } => Some(remote_mark(status)),
            RowKind::Agent { credential, .. } => credential.dot(),
            RowKind::Credential(credential) => credential.dot(),
            RowKind::GitIdentity(identity) => Some(mark(identity.is_set(), false)),
            RowKind::Tracker { state } => Some(tracker_mark(*state)),
            _ => None,
        }
    }

    /// Whether this row opens a form.
    ///
    /// The built-in base does not: there is no form behind it and nothing to
    /// configure, so selecting it is already as far as it goes, and its
    /// description is shown without a second step. A credential and a git
    /// identity are acted on with enter rather than edited here.
    pub fn opens_form(&self) -> bool {
        matches!(
            self.kind,
            RowKind::Environment { .. } | RowKind::Remote { .. }
        )
    }
}

/// Everything the panel draws, assembled by the caller from settings, the
/// remembered keys, and the built environments.
pub struct View {
    pub rows: Vec<Row>,
    /// What the base environment is made of, once it has been built here.
    pub base: Option<EnvironmentDescription>,
    /// Where Linear keeps its sign-in profile, for the Trackers page.
    pub tracker_profile_dir: PathBuf,
}

/// One credential, as the list shows it.
pub struct Credential {
    pub label: String,
    pub kind: CredentialKind,
}

impl Credential {
    pub(in crate::tui) fn is_pending(&self) -> bool {
        matches!(
            &self.kind,
            CredentialKind::Profile {
                status: CredentialStatus::Unknown | CredentialStatus::Checking,
                ..
            }
        )
    }

    /// What a profile's own check last said, as a mark in front of its row. The
    /// installation's key is not signed in to, so it has nothing to say.
    fn dot(&self) -> Option<Dot> {
        match &self.kind {
            CredentialKind::Profile { status, .. } => Some(credential_mark(status)),
            CredentialKind::Installation { .. } => None,
        }
    }
}

pub enum CredentialKind {
    /// This installation: the machine it runs on, the key that names it, and
    /// where its state lives.
    Installation {
        hostname: String,
        label: String,
        key: String,
        data_dir: String,
    },
    /// A profile a task mounts. Presence and place only: never the contents.
    Profile {
        profile: &'static str,
        directory: String,
        /// What that profile's own check said about it, if it has been asked.
        status: CredentialStatus,
    },
}

/// The identity and signing a task's git commits use.
///
/// Read from the version-control profile's git config, which every task using
/// that profile shares. An absent field is one git has no value for, which is
/// what a commit under a missing name or an unsigned commit looks like.
#[derive(Clone, Debug, Default)]
pub struct GitIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
    pub signing_key: Option<String>,
    pub signs_commits: bool,
}

impl GitIdentity {
    fn is_set(&self) -> bool {
        self.name.is_some() && self.email.is_some()
    }
}

/// The status vocabulary of this panel: one glyph for an answer that came back,
/// one for an answer that did not, and one that travels while the answer is
/// still out.
///
/// The colour is the terminal's own rather than green or red, so no status here
/// claims a verdict the row has not earned: a filled mark is white, and an empty
/// one is dim.
fn mark(filled: bool, asking: bool) -> Dot {
    let (glyph, style) = match (asking, filled) {
        (true, _) => ('◌', palette::muted()),
        (false, true) => ('●', palette::primary()),
        (false, false) => ('○', palette::muted()),
    };
    if asking {
        Dot::travelling(glyph, style)
    } else {
        Dot::still(glyph, style)
    }
}

fn remote_mark(status: &RemoteStatus) -> Dot {
    match status {
        RemoteStatus::Asking | RemoteStatus::Syncing => mark(false, true),
        RemoteStatus::Reached => mark(true, false),
        RemoteStatus::Unknown | RemoteStatus::Changed | RemoteStatus::Silent(_) => {
            mark(false, false)
        }
    }
}

fn credential_mark(status: &CredentialStatus) -> Dot {
    match status {
        CredentialStatus::Checking => mark(false, true),
        CredentialStatus::Checked {
            signed_in: true, ..
        } => mark(true, false),
        // Everything that is not a confirmed sign-in reads as not signed in, so
        // a failed check and a check that has not run look the same here.
        CredentialStatus::Unknown
        | CredentialStatus::Checked {
            signed_in: false, ..
        } => mark(false, false),
    }
}

fn tracker_mark(state: TrackerState) -> Dot {
    match state {
        TrackerState::Connecting => mark(false, true),
        TrackerState::Connected => mark(true, false),
        TrackerState::NotConnected => mark(false, false),
    }
}

// ------------------------------------------------------------------ the state

/// The panel's state: where the focus is, and whatever is drawn over it.
pub struct Panel {
    pub section: Section,
    pub selected: usize,
    pub depth: Depth,
    pub form: Option<Form>,
    pub detail_scroll: u16,
}

impl Panel {
    pub fn new() -> Self {
        Self {
            section: Section::ALL[0],
            selected: 0,
            // The rail, so the panel opens on its shape rather than on the
            // details of whatever row happens to be first.
            depth: Depth::Rail,
            form: None,
            detail_scroll: 0,
        }
    }
}

impl Default for Panel {
    fn default() -> Self {
        Self::new()
    }
}

impl Panel {
    /// Move within the focused list, which at the rail means the section.
    pub fn move_selection(&mut self, forward: bool, rows: usize) {
        self.detail_scroll = 0;
        match self.depth {
            Depth::Rail => {
                let at = Section::ALL
                    .iter()
                    .position(|section| *section == self.section)
                    .unwrap_or(0);
                let next = if forward {
                    (at + 1) % Section::ALL.len()
                } else {
                    (at + Section::ALL.len() - 1) % Section::ALL.len()
                };
                self.section = Section::ALL[next];
                self.selected = 0;
            }
            Depth::List | Depth::Detail => {
                if rows > 0 {
                    self.selected = if forward {
                        (self.selected + 1) % rows
                    } else {
                        (self.selected + rows - 1) % rows
                    };
                }
            }
        }
    }

    /// The row the focus is on, in the list the rail has selected.
    pub fn selected_row<'a>(&self, view: &'a View) -> Option<&'a Row> {
        view.rows.get(self.selected)
    }
}

// ------------------------------------------------------------------- the form

/// What an action applies to: the list it saves into, and what it replaces.
///
/// One type for the form and for the confirmation, because both need to know
/// which list they are about and which row they are about.
#[derive(Clone, PartialEq, Eq)]
pub enum Target {
    NewEnvironment,
    Environment { original: String },
    NewRemote,
    Remote { original: String },
}

impl Target {
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::NewRemote | Self::Remote { .. })
    }

    pub fn title(&self) -> &'static str {
        match self {
            Self::NewEnvironment => "New environment",
            Self::Environment { .. } => "Edit environment",
            Self::NewRemote => "Add remote",
            Self::Remote { .. } => "Edit remote",
        }
    }

    /// The first field's label: what it asks for.
    pub fn primary_label(&self) -> &'static str {
        if self.is_remote() { "Address" } else { "Name" }
    }

    /// What the form replaces, if anything.
    pub fn original(&self) -> Option<&str> {
        match self {
            Self::Environment { original } | Self::Remote { original } => Some(original),
            _ => None,
        }
    }

    /// The fields tab walks, in order.
    pub fn fields(&self) -> &'static [Focus] {
        if self.is_remote() {
            &[Focus::Primary, Focus::Cancel, Focus::Save]
        } else {
            &[Focus::Primary, Focus::Setup, Focus::Cancel, Focus::Save]
        }
    }
}

/// Which part of the form holds the caret.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Primary,
    Setup,
    Cancel,
    Save,
}

impl Focus {
    /// Whether this part takes typed text, and so owns enter and escape.
    pub fn takes_text(self) -> bool {
        matches!(self, Self::Primary | Self::Setup)
    }
}

/// A page over the panel, holding one environment or one remote.
pub struct Form {
    pub target: Target,
    /// The environment's name, or the remote's address.
    pub primary: Field,
    pub setup: TextBox,
    pub focus: Focus,
}

impl Form {
    pub fn new(target: Target, primary: &str, setup: &str) -> Self {
        Self {
            target,
            primary: Field::with(primary),
            setup: TextBox::new(setup),
            focus: Focus::Primary,
        }
    }

    /// Move the caret to the next or previous field, wrapping.
    pub fn move_focus(&mut self, forward: bool) {
        let fields = self.target.fields();
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
}

// ----------------------------------------------------------------- the drawing

pub fn draw(frame: &mut Frame, area: Rect, panel: &Panel, view: &View, tick: usize) {
    draw_columns(frame, area, panel, view, tick);
    if let Some(form) = &panel.form {
        chrome::dim(frame, area);
        draw_form(frame, area, form);
    }
}

/// One item of a list, in the three places it can hold something: a mark at the
/// left edge, the title, and a note at the right edge. Only the title is always
/// there.
///
/// The note goes to the right edge rather than after the title, so a column of
/// notes lines up instead of each one sitting wherever its title happens to end.
/// The mark is passed in rather than read, because a mark can be the one that
/// travels while it waits, and only the drawing knows where the frame is.
fn item(
    mark: Vec<Span<'static>>,
    title: &str,
    note: Option<&str>,
    width: usize,
    focused: bool,
) -> Line<'static> {
    let note = note.unwrap_or_default();
    let used = mark.iter().map(Span::width).sum::<usize>()
        + Line::from(title).width()
        + Line::from(note).width();
    let padding = " ".repeat(width.saturating_sub(used).max(1));
    let mut spans = mark;
    if focused {
        // Inverted across the row, which is what makes the mark's own colour read
        // as not being part of the bar.
        spans.push(Span::styled(
            format!("{title}{padding}{note}"),
            palette::focus(),
        ));
        return Line::from(spans);
    }
    spans.push(Span::raw(title.to_owned()));
    spans.push(Span::raw(padding));
    if !note.is_empty() {
        spans.push(Span::styled(note.to_owned(), palette::muted()));
    }
    Line::from(spans)
}

fn draw_columns(frame: &mut Frame, area: Rect, panel: &Panel, view: &View, tick: usize) {
    let block = Block::bordered()
        .title(" Settings ")
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1));
    frame.render_widget(Clear, area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [rail, list_area, detail_area] = Layout::horizontal([
        Constraint::Length(18),
        Constraint::Length(34),
        Constraint::Min(20),
    ])
    .spacing(GAP)
    .areas(inner);

    let rail_focused = panel.depth == Depth::Rail;
    let rail_items = Section::ALL
        .iter()
        .map(|section| {
            let focused = rail_focused && *section == panel.section;
            let row = item(
                Vec::new(),
                section.title(),
                None,
                usize::from(rail.width),
                focused,
            );
            ListItem::new(if rail_focused {
                row
            } else if *section == panel.section {
                row.style(palette::inactive_selection())
            } else {
                row.style(palette::muted())
            })
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(rail_items), rail);

    let list_focused = panel.depth == Depth::List;
    let width = usize::from(list_area.width);
    let list_items = view
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let mark = row.dot().map_or_else(Vec::new, |dot| dot.spans(tick));
            let line = item(
                mark,
                &row.label,
                row.note,
                width,
                list_focused && index == panel.selected,
            );
            let line = if list_focused {
                line
            } else {
                line.style(palette::muted())
            };
            // A discovered remote waits at the end of the list until it is
            // adopted, so it stays dim even while the list has the focus.
            ListItem::new(if matches!(row.kind, RowKind::Discovered(_)) {
                line.style(palette::muted())
            } else {
                line
            })
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(list_items), list_area);

    // The column ahead of the focus is drawn dimmed rather than withheld, so the
    // shape of the next step is visible before it is taken. A row with no deeper
    // level has no next step, so its column is already the one you are in.
    let detail_focused = panel.depth == Depth::Detail
        || (panel.depth == Depth::List
            && panel
                .selected_row(view)
                .is_some_and(|row| !row.opens_form()));
    let pane = detail(panel, view, detail_focused, tick);
    let last_row = pane
        .line_count(detail_area.width)
        .saturating_sub(usize::from(detail_area.height));
    let pane = pane.scroll((
        panel
            .detail_scroll
            .min(last_row.min(u16::MAX as usize) as u16),
        0,
    ));
    frame.render_widget(
        if detail_focused {
            pane
        } else {
            pane.style(palette::muted())
        },
        detail_area,
    );
}

// ------------------------------------------------------------------ the copy

/// A small heading inside a page, so a page that has to say several things can
/// be read in the order it means them.
fn section_heading(text: &str) -> Line<'static> {
    Line::styled(text.to_owned(), palette::primary().bold())
}

fn field(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), palette::muted()),
        Span::styled(value, palette::muted()),
    ])
}

/// The third column's page for anything with a connection to maintain: a login,
/// a remote, or this installation.
///
/// They all open the same way — what it is, whether it is connected, and the one
/// next step — so an operator reads one shape rather than five, and the
/// technical detail waits at the bottom where it does not push the status down.
struct Connector {
    title: String,
    reach: Reach,
    status: String,
    /// One value the status accents, e.g. the short key of this installation.
    status_value: Option<String>,
    /// A public name the provider reports, when the check printed one. Never
    /// guessed: absent when the report does not name one.
    identity: Option<String>,
    /// A short reason the status reads the way it does.
    note: Option<String>,
    /// The one next step, when there is one.
    action: Option<&'static str>,
    purpose: &'static str,
    fields: Vec<(&'static str, String)>,
}

/// Whether a connector is on, off, or still being asked about.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    On,
    Off,
    Asking,
}

impl Connector {
    fn lines(self, heading: impl Fn(String) -> Line<'static>, tick: usize) -> Vec<Line<'static>> {
        let status = self.status_line(tick);
        let mut lines = vec![heading(self.title), Line::from(""), status];
        if let Some(action) = self.action {
            lines.push(Line::from(""));
            lines.push(Line::styled(action, palette::accent()));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(self.purpose));
        if !self.fields.is_empty() {
            lines.push(Line::from(""));
            lines.extend(
                self.fields
                    .into_iter()
                    .map(|(label, value)| field(label, value)),
            );
        }
        if let Some(note) = self.note {
            lines.push(Line::from(""));
            lines.push(Line::styled(note, palette::muted()));
        }
        lines
    }

    /// The status is the page's verdict, so it is bold and monochrome with the
    /// same dot the list uses; only the value beside it, if any, is accented.
    fn status_line(&self, tick: usize) -> Line<'static> {
        let mut spans = mark(self.reach == Reach::On, self.reach == Reach::Asking).spans(tick);
        spans.push(Span::styled(
            self.status.clone(),
            match self.reach {
                Reach::On => palette::primary().bold(),
                Reach::Off | Reach::Asking => palette::muted().bold(),
            },
        ));
        if let Some(value) = &self.status_value {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(value.clone(), palette::accent()));
        }
        if let Some(identity) = &self.identity {
            spans.push(Span::styled(" as ", palette::primary().bold()));
            spans.push(Span::styled(identity.clone(), palette::accent()));
        }
        Line::from(spans)
    }
}

/// The third column. `focused` is decided by the caller, because whether this
/// column holds the focus is not the same question as how deep the focus is: a
/// row with nothing inside it is already here.
fn detail(panel: &Panel, view: &View, focused: bool, tick: usize) -> Paragraph<'static> {
    // The heading is the column's focus indicator: it has no rows to bar.
    let heading = move |text: String| {
        if focused {
            Line::styled(text, palette::heading())
        } else {
            Line::styled(text, palette::muted().bold())
        }
    };
    let Some(row) = panel.selected_row(view) else {
        return Paragraph::new(Span::styled("", palette::muted()));
    };
    let label = row.label.clone();
    let lines = match &row.kind {
        RowKind::Base => base_lines(view.base.as_ref(), heading),
        RowKind::Environment {
            setup, is_default, ..
        } => {
            let mut lines = vec![heading(label)];
            if *is_default {
                lines.push(Line::from(""));
                lines.push(Line::styled(
                    "New tasks use this environment by default.",
                    palette::primary(),
                ));
            }
            lines.push(Line::from(""));
            lines.push(Line::styled("setup command", palette::muted()));
            if setup.trim().is_empty() {
                lines.push(Line::styled(
                    "Nothing on top of the base. This environment is the base, by another name.",
                    palette::muted(),
                ));
            } else {
                lines.push(Line::from(setup.clone()));
            }
            lines
        }
        RowKind::Agent { kind, credential } => match &credential.kind {
            CredentialKind::Profile {
                profile,
                directory,
                status,
            } => profile_connector(label, agent_purpose(*kind), profile, directory, status),
            CredentialKind::Installation { .. } => credential_connector(credential),
        }
        .lines(heading, tick),
        RowKind::GitIdentity(identity) => git_connector(identity).lines(heading, tick),
        RowKind::Credential(credential) => credential_connector(credential).lines(heading, tick),
        RowKind::Tracker { state } => {
            tracker_connector(*state, &view.tracker_profile_dir).lines(heading, tick)
        }
        RowKind::Switch { setting, on } => vec![
            heading(label.clone()),
            Line::from(""),
            Line::styled(
                if *on { "on" } else { "off" },
                if *on {
                    palette::primary()
                } else {
                    palette::muted()
                },
            ),
            Line::from(""),
            Line::from(setting.description()),
        ],
        RowKind::Remote {
            address,
            remembered,
            status,
            needs_config,
            trackers,
        } => {
            let mut lines =
                remote_connector(label, address, remembered.as_ref(), status, *needs_config)
                    .lines(heading, tick);
            lines.push(Line::from(""));
            lines.push(section_heading("Task tracker"));
            lines.push(tracker_summary(trackers.as_deref(), tick));
            lines.push(Line::styled(
                "Press t to choose a task tracker.",
                palette::accent(),
            ));
            lines
        }
        RowKind::Discovered(discovered) => vec![
            heading(label),
            Line::from(""),
            Line::from(format!(
                "We detected this remote running altitude v{} on your network.",
                discovered.version
            )),
            Line::from(""),
            Line::styled("Press enter to start using it", palette::accent()),
        ],
    };
    Paragraph::new(lines).wrap(Wrap { trim: true })
}

/// Instructions shown after opening the remote setup helper.
///
/// Only getting a remote running: the shared task tracker is set up afterwards,
/// on the remote it belongs to, and lives in its own helper.
pub(in crate::tui) fn remote_setup_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(
            "Run altitude serve on a headless machine to use it as a remote and keep your \
             agents working even when you’re offline.",
        ),
        Line::from(""),
        Line::from(
            "Remotes can be shared with your team if they are on the same network. Each team \
             member gets their own space to run their own agents as they see fit.",
        ),
        Line::from(""),
        Line::from(
            "With an altitude remote server, you can also connect your task tracker to pilot \
             tasks and agents running on the remote directly from where you organize your work, \
             without going through altitude’s terminal interface.",
        ),
        Line::from(""),
        Line::styled(
            "Set up a machine on your network to run altitude serve, then press n in altitude > \
             Settings > Remotes to add its hostname.",
            palette::muted(),
        ),
        Line::from(""),
        Line::styled(
            "Hostname alone uses port 25848 (TCP and UDP). Anyone who can reach it is admitted; use a trusted network.",
            palette::muted(),
        ),
    ]
}

/// The guide for one task tracker a remote runs, opened from the provider list.
pub(in crate::tui) fn remote_tracker_setup_lines(kind: TrackerKind) -> Vec<Line<'static>> {
    match kind {
        TrackerKind::Linear => linear_remote_tracker_lines(),
    }
}

/// Instructions for the shared Linear app a remote runs.
fn linear_remote_tracker_lines() -> Vec<Line<'static>> {
    vec![
        section_heading("Shared Linear app"),
        Line::from(
            "A remote signs in as a shared Linear app, not as you. Its owner installs the app \
             once, and every task delegated there acts as it. This is separate from your \
             personal Linear sign-in under Settings → Task tracker.",
        ),
        Line::from(""),
        Line::from(vec![
            Span::raw("On the remote, edit "),
            Span::styled("~/.config/altitude/serve.toml", palette::accent()),
            Span::raw(
                ". The server prints its settings path when it starts; XDG_CONFIG_HOME, \
                 ALTITUDE_CONFIG_DIR or --config can change it.",
            ),
        ]),
        Line::from(""),
        step(
            "1.",
            "As a workspace administrator, create an OAuth application in Linear's API settings. \
             Name it Altitude and enable Agent Session and Issue webhook events.",
        ),
        step("2.", "Add this section to the remote's serve.toml:"),
        Line::from(""),
        sample("[linear]"),
        sample("client_id = \"<the application's client ID>\""),
        sample("public_url = \"https://altitude.example.com\""),
        Line::from(""),
        step(
            "3.",
            "Route that public HTTPS address to 127.0.0.1:25849 on the remote. Register \
             https://altitude.example.com/linear/oauth/callback as the app's redirect URL and \
             https://altitude.example.com/linear/webhook as its webhook URL. Replace the example \
             domain with yours.",
        ),
        step(
            "4.",
            "Set ALTITUDE_LINEAR_CLIENT_SECRET and ALTITUDE_LINEAR_WEBHOOK_SECRET in the \
             remote's environment, using the app's client secret and webhook signing secret. \
             Keep these available to the service when it starts.",
        ),
        step(
            "5.",
            "Stop altitude serve, then run altitude serve --connect-linear with the same \
             settings. Open the printed URL in your browser to install the app. Once it exits, \
             start altitude serve again.",
        ),
        step(
            "6.",
            "Sign in under Settings → Task tracker, then select this remote and press b to \
             pair your Linear identity. You can now delegate an issue to Altitude in Linear.",
        ),
        Line::from(""),
        Line::styled(
            "The remote shows configured once its app is installed and altitude serve is \
             running with it; this panel does not check Linear from here.",
            palette::muted(),
        ),
    ]
}

fn step(number: &'static str, text: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{number} "), palette::accent()),
        Span::raw(text),
    ])
}

fn sample(text: &'static str) -> Line<'static> {
    Line::styled(text, palette::accent())
}

/// The built-in environment, described rather than configured.
///
/// The version and the packages are read back from the built snapshot, because
/// that is the only place that knows them: the image is a rolling tag and the
/// setup commands name no versions. A machine that has not built it yet says so
/// rather than guessing.
fn base_lines(
    description: Option<&EnvironmentDescription>,
    heading: impl Fn(String) -> Line<'static>,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        heading("Base environment".to_owned()),
        // The subtitle rather than a postscript: it is true of the whole page.
        Line::styled("built in · cannot be edited or deleted", palette::muted()),
        Line::from(""),
        Line::from(
            "Altitude runs your tasks in sandboxes that move between machines as you delegate \
             work.",
        ),
        Line::from(""),
    ];
    match description.filter(|description| !description.alpine_release.is_empty()) {
        Some(description) => {
            // What was read back from the build is accented, so the page says at
            // a glance which parts of it are about this machine rather than about
            // Altitude.
            lines.push(Line::from(vec![
                Span::raw("New tasks are built from Alpine Linux "),
                Span::styled(description.alpine_release.clone(), palette::accent()),
                Span::raw(", with these packages installed:"),
            ]));
            lines.push(Line::from(""));
            // One package a line: a list of versions is read, not scanned.
            if description.packages.is_empty() {
                lines.push(Line::styled(
                    "nothing beyond the base image",
                    palette::muted(),
                ));
            } else {
                lines.extend(
                    description
                        .packages
                        .iter()
                        .map(|package| Line::styled(package.clone(), palette::accent())),
                );
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::raw("Built "),
                Span::styled(met_ago(description.built_at_ms), palette::accent()),
                Span::raw(", and refreshed automatically every "),
                Span::styled(
                    counted(REFRESH_AFTER.as_secs() as i64 / 3_600, "hour"),
                    palette::accent(),
                ),
                Span::raw(", so security patches arrive without you doing anything."),
            ]));
        }
        None => {
            lines.push(Line::from(
                "It has not been built on this machine yet. The first task that uses it builds \
                 it, and this page then says what it was built from.",
            ));
        }
    }
    lines
}

/// A login a task signs in with, as its page reads it. The same shape serves a
/// coding agent and the GitHub CLI.
fn profile_connector(
    title: String,
    purpose: &'static str,
    profile: &'static str,
    directory: &str,
    status: &CredentialStatus,
) -> Connector {
    let (reach, word) = match status {
        CredentialStatus::Checking => (Reach::Asking, "checking…"),
        CredentialStatus::Checked {
            signed_in: true, ..
        } => (Reach::On, "signed in"),
        _ => (Reach::Off, "not signed in"),
    };
    let identity = match status {
        CredentialStatus::Checked {
            signed_in: true,
            output,
        } => public_identity(profile, output),
        _ => None,
    };
    Connector {
        title,
        reach,
        status: word.to_owned(),
        status_value: None,
        identity,
        note: None,
        action: matches!(
            status,
            CredentialStatus::Checked {
                signed_in: false,
                ..
            }
        )
        .then_some("Press enter to sign in."),
        purpose,
        fields: vec![("saved at", tilde(directory))],
    }
}

/// This installation's key, or a profile's login, as the page reads it.
fn credential_connector(credential: &Credential) -> Connector {
    match &credential.kind {
        CredentialKind::Installation {
            hostname,
            label,
            key,
            data_dir,
        } => Connector {
            title: format!("{hostname} (this machine)"),
            reach: Reach::On,
            status: "registered as key".to_owned(),
            status_value: Some(short_key(key)),
            identity: None,
            note: None,
            action: Some("Press enter to learn more about altitude remotes."),
            purpose: "The key that names this installation to a remote, so a paired remote \
                      knows whose work it holds.",
            fields: vec![
                ("name", label.clone()),
                ("key", key.clone()),
                ("state", tilde(data_dir)),
            ],
        },
        CredentialKind::Profile {
            profile,
            directory,
            status,
        } => profile_connector(
            credential.label.clone(),
            profile_purpose(profile),
            profile,
            directory,
            status,
        ),
    }
}

/// The git identity and signing a task commits with.
fn git_connector(identity: &GitIdentity) -> Connector {
    let is_set = identity.is_set();
    let status = match (is_set, identity.signs_commits) {
        (true, true) => "configured · signing enabled",
        (true, false) => "configured · signing disabled",
        (false, _) => "not set",
    };
    Connector {
        title: "Git identity".to_owned(),
        reach: if is_set { Reach::On } else { Reach::Off },
        status: status.to_owned(),
        status_value: None,
        identity: None,
        note: None,
        action: Some("Press enter to configure Git."),
        purpose: "The name, email, and signing key your tasks commit under, read from the \
                  version-control profile's git config.",
        fields: vec![
            (
                "name",
                identity.name.clone().unwrap_or_else(|| "—".to_owned()),
            ),
            (
                "email",
                identity.email.clone().unwrap_or_else(|| "—".to_owned()),
            ),
            (
                "signing",
                match (&identity.signing_key, identity.signs_commits) {
                    (Some(key), true) => key.clone(),
                    (Some(key), false) => format!("{key} (not used yet)"),
                    (None, _) => "—".to_owned(),
                },
            ),
        ],
    }
}

/// Linear, as the page reads it: a live session rather than a login kept on disk.
fn tracker_connector(state: TrackerState, profile_dir: &Path) -> Connector {
    let (reach, status, note, action) = match state {
        TrackerState::Connected => (Reach::On, "signed in", None, None),
        TrackerState::Connecting => (Reach::Asking, "connecting…", None, None),
        TrackerState::NotConnected => (
            Reach::Off,
            "not signed in",
            None,
            Some("Press enter to sign in."),
        ),
    };
    Connector {
        title: "Linear".to_owned(),
        reach,
        status: status.to_owned(),
        status_value: None,
        identity: None,
        note,
        action,
        purpose: "A task tracking and product management tool. Altitude reads your issues and updates their status as it works on them.",
        fields: vec![("profile", tilde(&profile_dir.display().to_string()))],
    }
}

/// A remote, as the page reads it: what it is, whether it answered, and the
/// address and key the list keeps for it.
fn remote_connector(
    title: String,
    address: &str,
    remembered: Option<&RemoteKey>,
    status: &RemoteStatus,
    needs_config: bool,
) -> Connector {
    let (reach, word, note, action) = match status {
        RemoteStatus::Reached if needs_config => (
            Reach::Asking,
            "synchronizing…",
            Some("Sending this machine's environments and logins.".to_owned()),
            None,
        ),
        RemoteStatus::Reached => (Reach::On, "synced", None, None),
        RemoteStatus::Asking => (Reach::Asking, "connecting…", None, None),
        RemoteStatus::Syncing => (
            Reach::Asking,
            "synchronizing…",
            Some("It is building what it needs to run a task.".to_owned()),
            None,
        ),
        RemoteStatus::Changed => (
            Reach::Off,
            "disconnected",
            Some("It answered with a different key than the one you have.".to_owned()),
            Some("Press enter to review its identity."),
        ),
        RemoteStatus::Silent(reason) => (
            Reach::Off,
            "disconnected",
            Some(format!("It did not answer — {reason}.")),
            Some("Press enter to try again."),
        ),
        RemoteStatus::Unknown => (
            Reach::Off,
            "disconnected",
            None,
            Some("Press enter to reach it."),
        ),
    };
    let mut fields = vec![("address", address.to_owned())];
    match remembered {
        Some(key) => {
            fields.push(("key", key.key.clone()));
            fields.push(("version", key.version.clone()));
            fields.push(("met", met_ago(key.seen_at_ms)));
        }
        None => fields.push(("key", "never met".to_owned())),
    }
    Connector {
        title,
        reach,
        status: word.to_owned(),
        status_value: None,
        identity: remembered.map(|key| key.label.clone()),
        note,
        action,
        purpose: "A machine that keeps running your tasks after you close Altitude, so work \
                  carries on while you are away. Yours alone, or shared with a team.",
        fields,
    }
}

/// The mark for what a remote's announcement said about one tracker: connected,
/// not connected, or unknown because the remote has not answered at all.
pub(in crate::tui) fn tracker_dot(is_ready: Option<bool>) -> Dot {
    match is_ready {
        Some(true) => Dot::still('●', palette::primary()),
        Some(false) => Dot::still('○', palette::muted()),
        None => Dot::still('○', palette::muted()),
    }
}

/// The remote's shared trackers, under its primary details: a status line in the
/// same shape as the first, not a page of its own.
fn tracker_summary(trackers: Option<&[TrackerConnection]>, tick: usize) -> Line<'static> {
    let Some(connections) = trackers else {
        let mut spans = tracker_dot(None).spans(tick);
        spans.push(Span::styled("unknown", palette::muted().bold()));
        return Line::from(spans);
    };
    let initialized = connections
        .iter()
        .filter(|connection| connection.is_ready)
        .map(|connection| connection.provider.label())
        .collect::<Vec<_>>();
    if initialized.is_empty() {
        let mut spans = tracker_dot(Some(false)).spans(tick);
        spans.push(Span::styled("not connected", palette::muted().bold()));
        return Line::from(spans);
    }
    let mut spans = tracker_dot(Some(true)).spans(tick);
    spans.push(Span::styled(
        initialized.join(", "),
        palette::primary().bold(),
    ));
    Line::from(spans)
}

/// What a profile is for, in one line, so a page opens by saying what the
/// operator is looking at.
fn profile_purpose(profile: &str) -> &'static str {
    match profile {
        "vcs/github" => "The GitHub CLI login your tasks clone, push, and read pull requests with.",
        "agents/pi" => "Pi uses your provider account to run tasks.",
        "agents/claude" => "Claude Code uses your account to run tasks.",
        _ => "A login a task is given.",
    }
}

/// A public name a check printed, when it printed one in a shape the profile's
/// CLI is known to use. Anything else yields nothing rather than a guess:
/// signed in without a name is still true.
fn public_identity(profile: &str, output: &str) -> Option<String> {
    match profile {
        "vcs/github" => github_account(output),
        _ => None,
    }
}

fn github_account(output: &str) -> Option<String> {
    const MARKERS: [&str; 2] = [
        "Logged in to github.com as ",
        "Logged in to github.com account ",
    ];
    for line in plain(output).lines() {
        for marker in MARKERS {
            let Some((_, rest)) = line.split_once(marker) else {
                continue;
            };
            let Some(name) = rest.split_whitespace().next() else {
                continue;
            };
            if name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Some(name.to_owned());
            }
        }
    }
    None
}

fn hostname() -> String {
    let name: String = whoami::hostname()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    if name.is_empty() {
        "unknown".to_owned()
    } else {
        name
    }
}

/// A path as the operator would type it: the home directory's prefix is replaced
/// with `~`, component by component so `/home/gaby-other` is not mistaken for a
/// path under `/home/gaby`.
fn tilde(path: &str) -> String {
    let Some(home) = env::var_os("HOME") else {
        return path.to_owned();
    };
    let Ok(rest) = Path::new(path).strip_prefix(Path::new(&home)) else {
        return path.to_owned();
    };
    if rest.as_os_str().is_empty() {
        return "~".to_owned();
    }
    Path::new("~").join(rest).display().to_string()
}

/// The head of a key, long enough to tell two apart and short enough to read on
/// the status line. The whole key stays in the fields below.
fn short_key(key: &str) -> String {
    const KEEP: usize = 12;
    let head = key.chars().take(KEEP).collect::<String>();
    if key.chars().count() > KEEP {
        format!("{head}…")
    } else {
        head
    }
}

/// How long ago something was seen, coarsely: the exact minute is not the point.
fn met_ago(seen_at_ms: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default();
    let minutes = now.saturating_sub(seen_at_ms).max(0) / 60_000;
    match minutes {
        0..=1 => "just now".to_owned(),
        2..=59 => format!("{} ago", counted(minutes, "minute")),
        60..=2879 => format!("{} ago", counted(minutes / 60, "hour")),
        _ => format!("{} ago", counted(minutes / 1_440, "day")),
    }
}

/// A count and its unit, saying "1 hour" rather than "1 hours".
fn counted(count: i64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

/// Which profile a coding agent's login lives in.
fn agent_profile(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Pi => "agents/pi",
        AgentKind::Claude => "agents/claude",
    }
}

fn agent_purpose(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Pi => "A coding harness by Earendil. Altitude drives it to run your tasks.",
        AgentKind::Claude => "A coding harness by Anthropic. Altitude drives it to run your tasks.",
    }
}

// ------------------------------------------------------------------ the pages

fn draw_form(frame: &mut Frame, area: Rect, form: &Form) {
    let area = if form.target.is_remote() {
        chrome::centered(area, 45, 9)
    } else {
        chrome::centered(area, 65, 18)
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

    let fields_height = inner.height.saturating_sub(3);
    let [fields, buttons] =
        Layout::vertical([Constraint::Length(fields_height), Constraint::Length(3)]).areas(inner);

    if form.target.is_remote() {
        let [primary, rest] =
            Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).areas(fields);
        draw_label(frame, primary, form.target.primary_label());
        form.primary.draw(
            frame,
            field_box(primary),
            &FieldStyle {
                focused: form.focus == Focus::Primary,
                title: None,
                prefix: "",
            },
        );
        let _ = rest;
    } else {
        let [primary, setup] =
            Layout::vertical([Constraint::Length(4), Constraint::Min(5)]).areas(fields);

        draw_label(frame, primary, form.target.primary_label());
        form.primary.draw(
            frame,
            field_box(primary),
            &FieldStyle {
                focused: form.focus == Focus::Primary,
                title: None,
                prefix: "",
            },
        );

        draw_label(frame, setup, "Setup command");
        form.setup
            .draw(frame, field_box(setup), form.focus == Focus::Setup);
    }

    // The actions sit at the bottom right, where the eye ends up.
    let [_, cancel, _, save] = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(10),
        Constraint::Length(3),
        Constraint::Length(10),
    ])
    .areas(buttons);
    chrome::button(frame, cancel, "cancel", form.focus == Focus::Cancel, false);
    chrome::button(frame, save, "save", form.focus == Focus::Save, true);
}

fn draw_label(frame: &mut Frame, area: Rect, text: &str) {
    let [label, _] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(Line::styled(text, palette::muted())), label);
}

/// The box a field's input is drawn in, one row below its label.
fn field_box(area: Rect) -> Rect {
    let [_, input] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    input
}

impl App {
    /// Everything the panel draws, assembled from the settings file and from
    /// what was read back about each remote and about the base environment.
    pub(in crate::tui) fn settings_view(&self) -> View {
        let default_environment = self.default_environment_name().to_owned();
        let rows = match self.panel.section {
            Section::Interface => vec![Row {
                label: Setting::KeyboardHints.label().to_owned(),
                note: None,
                kind: RowKind::Switch {
                    setting: Setting::KeyboardHints,
                    on: self.keyboard_hints(),
                },
            }],
            Section::Agents => AGENTS
                .into_iter()
                .map(|kind| Row {
                    label: kind.to_string(),
                    note: (self.agent() == Some(kind)).then_some("(default)"),
                    kind: RowKind::Agent {
                        kind,
                        credential: self.agent_credential(kind),
                    },
                })
                .collect(),
            Section::VersionControl => {
                let identity = self.git_identity();
                let mut rows = vec![Row {
                    label: "Git identity".to_owned(),
                    note: identity.is_set().then_some("(set)"),
                    kind: RowKind::GitIdentity(identity),
                }];
                rows.extend(
                    self.profiles
                        .iter()
                        .filter(|profile| profile.profile.starts_with("vcs/"))
                        .map(|profile| Row {
                            label: profile.display.to_owned(),
                            note: None,
                            kind: RowKind::Credential(self.profile_credential(profile)),
                        }),
                );
                rows
            }
            Section::Trackers => vec![Row {
                label: "Linear".to_owned(),
                note: Some(match self.tracker_state {
                    TrackerState::Connected => "(connected)",
                    TrackerState::Connecting => "(connecting)",
                    TrackerState::NotConnected => "(not connected)",
                }),
                kind: RowKind::Tracker {
                    state: self.tracker_state,
                },
            }],
            Section::Environments => self
                .environments()
                .into_iter()
                .map(|environment| {
                    let built_in = environment.is_base();
                    let is_default = environment.name.eq_ignore_ascii_case(&default_environment);
                    Row {
                        label: environment.name.clone(),
                        // The built-in one says what it is rather than what it is
                        // used for: there is nothing to choose about it.
                        note: if built_in {
                            Some("(built in)")
                        } else {
                            is_default.then_some("(default)")
                        },
                        kind: if built_in {
                            RowKind::Base
                        } else {
                            RowKind::Environment {
                                name: environment.name,
                                setup: environment.setup_command,
                                is_default,
                            }
                        },
                    }
                })
                .collect(),
            Section::Remotes => self.remote_rows(),
        };
        View {
            rows,
            base: self.base_description.clone(),
            tracker_profile_dir: self.tracker_profile_dir(),
        }
    }

    /// This installation's hostname, key, and where its state lives.
    fn installation_credential(&self) -> Credential {
        let identity = self.config.identity();
        let hostname = hostname();
        Credential {
            label: hostname.clone(),
            kind: CredentialKind::Installation {
                hostname,
                label: identity
                    .as_ref()
                    .map(|identity| identity.label.clone())
                    .unwrap_or_else(|error| format!("unreadable: {error}")),
                key: identity
                    .as_ref()
                    .map(|identity| identity.id.clone())
                    .unwrap_or_else(|error| format!("unreadable: {error}")),
                data_dir: self.config.data_dir().display().to_string(),
            },
        }
    }

    /// One profile's login, with the place it lives and what its own check last
    /// said. Names and places only: a credential is never read, and never shown.
    fn profile_credential(&self, profile: &ProfileSummary) -> Credential {
        Credential {
            label: profile.display.to_owned(),
            kind: CredentialKind::Profile {
                profile: profile.profile,
                directory: self
                    .config
                    .profile_dir(profile.profile)
                    .display()
                    .to_string(),
                status: self
                    .credential_status
                    .get(profile.profile)
                    .cloned()
                    .unwrap_or_default(),
            },
        }
    }

    /// The login of the profile a coding agent runs under.
    fn agent_credential(&self, kind: AgentKind) -> Credential {
        let profile = agent_profile(kind);
        match self
            .profiles
            .iter()
            .find(|summary| summary.profile == profile)
        {
            Some(summary) => self.profile_credential(summary),
            None => Credential {
                label: kind.to_string(),
                kind: CredentialKind::Profile {
                    profile,
                    directory: String::new(),
                    status: CredentialStatus::default(),
                },
            },
        }
    }

    /// The addresses the settings file configures.
    pub(in crate::tui) fn configured_remotes(&self) -> Vec<String> {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().remotes().to_vec())
            .unwrap_or_default()
    }

    /// The remotes list: detected remotes go last, so adopting one never
    /// reorders the remotes the operator configured.
    fn remote_rows(&self) -> Vec<Row> {
        let installation = self.installation_credential();
        // This installation comes first: it is the identity a remote is paired
        // with.
        let mut rows = vec![Row {
            label: installation.label.clone(),
            note: None,
            kind: RowKind::Credential(installation),
        }];
        rows.extend(self.configured_remotes().into_iter().map(|address| {
            let remembered = self
                .remembered_keys
                .iter()
                .find(|key| key.address.eq_ignore_ascii_case(&address))
                .cloned();
            let status = self
                .remote_status
                .get(&address)
                .cloned()
                .unwrap_or(RemoteStatus::Unknown);
            let trackers = self.remote_trackers.get(&address).cloned();
            Row {
                // A name is what the remote called itself, so an address
                // that has never answered shows itself instead.
                label: match &remembered {
                    Some(key) => key.label.clone(),
                    None => address.clone(),
                },
                note: None,
                kind: RowKind::Remote {
                    needs_config: self.remote_needs_config(&address),
                    address,
                    remembered,
                    status,
                    trackers,
                },
            }
        }));
        rows.extend(self.discovered_rows());
        rows
    }

    /// Discovered remotes, ordered by the name they announce so the list does
    /// not reshuffle as announcements arrive.
    fn discovered_rows(&self) -> Vec<Row> {
        let configured = self.configured_remotes();
        let mut discovered = self
            .discovered_remotes
            .values()
            .filter(|remote| {
                !configured
                    .iter()
                    .any(|address| address.eq_ignore_ascii_case(&remote.address))
                    && !self
                        .remembered_keys
                        .iter()
                        .any(|key| key.key == remote.id && configured.contains(&key.address))
            })
            .cloned()
            .collect::<Vec<_>>();
        discovered.sort_by(|left, right| {
            left.label
                .cmp(&right.label)
                .then_with(|| left.address.cmp(&right.address))
        });
        discovered
            .into_iter()
            .map(|remote| Row {
                label: if remote.label.trim().is_empty() {
                    remote.address.clone()
                } else {
                    remote.label.clone()
                },
                note: None,
                kind: RowKind::Discovered(remote),
            })
            .collect()
    }

    /// How many rows the focused list holds, which is what moving needs.
    pub(in crate::tui) fn row_count(&self) -> usize {
        match self.panel.section {
            Section::Interface => 1,
            Section::Agents => AGENTS.len(),
            Section::VersionControl => {
                1 + self
                    .profiles
                    .iter()
                    .filter(|profile| profile.profile.starts_with("vcs/"))
                    .count()
            }
            Section::Trackers => 1,
            Section::Environments => self.environments().len(),
            Section::Remotes => self.remote_rows().len(),
        }
    }

    pub(in crate::tui) fn selected_row(&self) -> Option<Row> {
        self.settings_view()
            .rows
            .into_iter()
            .nth(self.panel.selected)
    }
}
