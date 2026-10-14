//! The settings panel: what the interface shows, which agent drives a task, the
//! credentials on this machine, the environments a task runs in, and the remotes
//! it can be delegated to.
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
    widgets::{chrome, palette, utils::Dot},
};

use std::{
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
    isolation::{EnvironmentDescription, ProfileMount, REFRESH_AFTER},
    remote::RemoteKey,
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
    Credentials,
    Trackers,
    Environments,
    Remotes,
}

impl Section {
    pub const ALL: [Section; 6] = [
        Self::Interface,
        Self::Agents,
        Self::Credentials,
        Self::Trackers,
        Self::Environments,
        Self::Remotes,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Interface => "Interface",
            Self::Agents => "Agents",
            Self::Credentials => "Credentials",
            Self::Trackers => "Trackers",
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

    /// What a list that can be empty says in place of its rows.
    ///
    /// The remotes list is the only one that can be: the base environment is
    /// always there, and the other lists are built from what is already
    /// configured.
    pub fn empty_note(self) -> Option<Line<'static>> {
        match self {
            Self::Remotes => Some(Line::from(vec![
                Span::raw("No remotes are configured. Press "),
                Span::styled("n", palette::accent()),
                Span::raw(" to add one."),
            ])),
            _ => None,
        }
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
    /// A coding agent, which may be the one new tasks use.
    Agent(AgentKind),
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
    },
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
    /// The mark in front of this row, where what it says is asked rather than known:
    /// a remote's address, or a profile's login.
    fn dot(&self) -> Option<Dot> {
        match &self.kind {
            RowKind::Remote { status, .. } => Some(status.dot()),
            RowKind::Credential(credential) => credential.dot(),
            RowKind::Tracker { state } => Some(match state {
                TrackerState::Connected => Dot::still('●', palette::good()),
                TrackerState::Connecting => Dot::travelling('◌', palette::attention()),
                TrackerState::NotConnected => Dot::still('○', palette::muted()),
            }),
            _ => None,
        }
    }

    /// Whether this row opens into anything.
    ///
    /// The built-in base does not: there is no form behind it and nothing to
    /// configure, so selecting it is already as far as it goes, and its
    /// description is shown without a second step.
    pub fn is_actionable(&self) -> bool {
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
    /// What a profile's own check last said, as a mark in front of its row. The
    /// installation's key is not signed in to, so it has nothing to say.
    fn dot(&self) -> Option<Dot> {
        match &self.kind {
            CredentialKind::Profile { status, .. } => Some(status.dot()),
            CredentialKind::Installation { .. } => None,
        }
    }
}

pub enum CredentialKind {
    /// This installation: the key that names it and where its state lives.
    Installation {
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
        mounts: Vec<ProfileMount>,
    },
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
            depth: Depth::List,
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
            } else {
                row.style(palette::muted())
            })
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(rail_items), rail);

    let list_focused = panel.depth == Depth::List;
    if view.rows.is_empty() {
        // An empty list says so where its rows would be, because that is where
        // the key that fills it belongs.
        if let Some(note) = panel.section.empty_note() {
            frame.render_widget(
                Paragraph::new(note)
                    .style(palette::muted())
                    .wrap(Wrap { trim: true }),
                list_area,
            );
        }
    } else {
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
                ListItem::new(if list_focused {
                    line
                } else {
                    line.style(palette::muted())
                })
            })
            .collect::<Vec<_>>();
        frame.render_widget(List::new(list_items), list_area);
    }

    // The column ahead of the focus is drawn dimmed rather than withheld, so the
    // shape of the next step is visible before it is taken. A row with no deeper
    // level has no next step, so its column is already the one you are in.
    let detail_focused = panel.depth == Depth::Detail
        || (panel.depth == Depth::List
            && panel
                .selected_row(view)
                .is_some_and(|row| !row.is_actionable()));
    let pane = detail(panel, view, detail_focused);
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

/// How a remote is made, and what one is for.
///
/// Shown in the remotes detail and, when that list is empty, in place of the
/// detail the missing row would otherwise give.
fn remote_education_lines() -> Vec<Line<'static>> {
    vec![
        Line::styled(
            "A remote keeps tasks running after you close this client.",
            palette::muted(),
        ),
        Line::from(""),
        Line::from(vec![
            Span::styled("On another machine, run ", palette::muted()),
            Span::styled("altitude serve", palette::accent()),
            Span::styled(" (uses Microsandbox). Default port: ", palette::muted()),
            Span::styled("25848", palette::accent()),
            Span::styled(" (TCP and UDP).", palette::muted()),
        ]),
        Line::from(""),
        Line::styled(
            "Use a trusted network: reachable clients are admitted. Add its host:port here.",
            palette::muted(),
        ),
        Line::from(""),
        Line::from(vec![
            Span::styled("Press ", palette::muted()),
            Span::styled("p", palette::accent()),
            Span::styled(
                " to send your environments and login profiles to the selected remote.",
                palette::muted(),
            ),
        ]),
    ]
}

/// The third column. `focused` is decided by the caller, because whether this
/// column holds the focus is not the same question as how deep the focus is: a
/// row with nothing inside it is already here.
fn detail(panel: &Panel, view: &View, focused: bool) -> Paragraph<'static> {
    // The heading is the column's focus indicator: it has no rows to bar.
    let heading = move |text: String| {
        if focused {
            Line::styled(text, palette::heading())
        } else {
            Line::styled(text, palette::muted().bold())
        }
    };
    let Some(row) = panel.selected_row(view) else {
        // The only list that can be empty teaches rather than stays blank.
        if panel.section == Section::Remotes {
            return Paragraph::new(remote_education_lines()).wrap(Wrap { trim: true });
        }
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
                    palette::good(),
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
        RowKind::Agent(kind) => vec![
            heading(label.clone()),
            Line::from(""),
            Line::styled(
                match kind {
                    AgentKind::Pi => "Pi by Earendil",
                    AgentKind::Claude => "Claude by Anthropic",
                },
                palette::primary(),
            ),
        ],
        RowKind::Credential(credential) => credential_lines(credential, heading),
        RowKind::Tracker { state } => tracker_lines(*state, &view.tracker_profile_dir, heading),
        RowKind::Switch { setting, on } => vec![
            heading(label.clone()),
            Line::from(""),
            Line::styled(
                if *on { "on" } else { "off" },
                if *on {
                    palette::good()
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
        } => {
            let mut lines = vec![
                heading(label),
                Line::from(""),
                Line::from(vec![
                    Span::styled("address   ", palette::muted()),
                    address.clone().into(),
                ]),
            ];
            match status {
                RemoteStatus::Silent(reason) => lines.push(Line::styled(
                    format!("did not answer: {reason}"),
                    palette::bad(),
                )),
                RemoteStatus::Asking => lines.push(Line::styled("asking…", palette::attention())),
                RemoteStatus::Syncing => lines.push(Line::styled(
                    "syncing — it is building what it needs to run a task",
                    palette::attention(),
                )),
                RemoteStatus::Changed => lines.push(Line::styled(
                    "it answered with a different key than the one you have. \
                         d forgets the old one, then enter accepts this one.",
                    palette::bad(),
                )),
                RemoteStatus::Unknown | RemoteStatus::Reached => {}
            }
            if *needs_config && *status == RemoteStatus::Reached {
                lines.push(Line::styled(
                    "it does not hold this configuration yet — p sends it",
                    palette::attention(),
                ));
            }
            lines.push(Line::from(""));
            match remembered {
                Some(key) => {
                    lines.push(Line::from(vec![
                        Span::styled("key       ", palette::muted()),
                        key.key.clone().into(),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("version   ", palette::muted()),
                        key.version.clone().into(),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("met       ", palette::muted()),
                        met_ago(key.seen_at_ms).into(),
                    ]));
                }
                None => {
                    lines.push(Line::styled("never met", palette::muted()));
                    lines.push(Line::styled(
                        "No key is remembered for this address yet. Press enter to reach it, \
                             which records the key it answers with.",
                        palette::muted(),
                    ));
                }
            }
            lines.push(Line::from(""));
            lines.extend(remote_education_lines());
            lines
        }
    };
    Paragraph::new(lines).wrap(Wrap { trim: true })
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

/// How to connect Linear, and what Altitude does with the connection.
///
/// The page names the profile directory and never its contents. Signing in runs
/// through Linear's official MCP server, which Altitude starts for itself; the
/// operator finishes it in a browser, and the page says so rather than implying
/// a key is copied anywhere.
fn tracker_lines(
    state: TrackerState,
    profile_dir: &Path,
    heading: impl Fn(String) -> Line<'static>,
) -> Vec<Line<'static>> {
    let (status, style) = match state {
        TrackerState::Connected => ("connected", palette::good()),
        TrackerState::Connecting => ("finishing sign-in in your browser", palette::attention()),
        TrackerState::NotConnected => ("not connected", palette::attention()),
    };
    let mut lines = vec![
        heading("Linear".to_owned()),
        Line::from(""),
        Line::styled(status, style),
        Line::from(""),
        Line::from(
            "Press enter to connect in your browser. Uses Linear's official MCP server; \
             requires Node.js (npx).",
        ),
        Line::from(""),
        Line::styled("profile", palette::muted()),
        Line::styled(profile_dir.display().to_string(), palette::primary()),
        Line::from(""),
        Line::from("Login stays outside task capsules. Altitude controls tracker writes."),
    ];
    match state {
        TrackerState::Connected => {}
        TrackerState::Connecting => {
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "Press enter again to give up on this sign-in.",
                palette::muted(),
            ));
        }
        TrackerState::NotConnected => {}
    }
    lines
}

/// What is known about one credential.
fn credential_lines(
    credential: &Credential,
    heading: impl Fn(String) -> Line<'static>,
) -> Vec<Line<'static>> {
    let mut lines = vec![heading(credential.label.clone()), Line::from("")];
    match &credential.kind {
        CredentialKind::Installation {
            label,
            key,
            data_dir,
        } => {
            lines.push(Line::from(vec![
                Span::styled("label     ", palette::muted()),
                label.clone().into(),
            ]));
            lines.push(Line::from(vec![
                Span::styled("key       ", palette::muted()),
                key.clone().into(),
            ]));
            lines.push(Line::from(vec![
                Span::styled("state     ", palette::muted()),
                data_dir.clone().into(),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "The key names this machine and cannot be edited here. It is what a tracker's \
                 actor is mapped to, and what a remote checks.",
                palette::muted(),
            ));
        }
        CredentialKind::Profile {
            profile,
            directory,
            status,
            mounts,
        } => {
            lines.extend(status_lines(status));
            lines.push(Line::from(vec![
                Span::styled("profile   ", palette::muted()),
                (*profile).into(),
            ]));
            lines.push(Line::from(vec![
                Span::styled("on disk   ", palette::muted()),
                directory.clone().into(),
            ]));
            if !mounts.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::styled("A task sees", palette::muted()));
                // A directory at the profile's root is mounted as the profile itself,
                // so there is nothing to say after the arrow.
                lines.extend(mounts.iter().map(|mount| {
                    Line::from(match mount.path.is_empty() {
                        true => mount.guest.clone(),
                        false => format!("{}  →  {}", mount.guest, mount.path),
                    })
                }));
            }
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "A login is kept here and mounted into every task, so it survives between them. \
                 Altitude shows where it lives and never its contents.",
                palette::muted(),
            ));
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "Press enter for a shell in a sandbox to sign in in. It is thrown away when you \
                 leave it; the login is not.",
                palette::good(),
            ));
        }
    }
    lines
}

/// What a profile's own check said about it, as its page reads it.
///
/// The command's own words are kept: a check is a command the profile's CLI knows how
/// to answer, and paraphrasing it would be Altitude guessing at what it means.
fn status_lines(status: &CredentialStatus) -> Vec<Line<'static>> {
    match status {
        CredentialStatus::Unknown => vec![Line::styled("not checked yet", palette::muted())],
        CredentialStatus::Checking => {
            vec![Line::styled(
                "asking its own command…",
                palette::attention(),
            )]
        }
        CredentialStatus::Checked { signed_in, output } => {
            let mut lines = vec![Line::styled(
                if *signed_in {
                    "signed in"
                } else {
                    "not signed in"
                },
                if *signed_in {
                    palette::good()
                } else {
                    palette::muted()
                },
            )];
            let output = plain(output);
            if !output.trim().is_empty() {
                lines.push(Line::from(""));
                lines.extend(output.lines().map(|line| Line::from(line.to_owned())));
            }
            lines
        }
        CredentialStatus::NotBuilt => vec![
            Line::styled("not asked here", palette::attention()),
            Line::from(""),
            Line::styled(
                "The environment this login needs is not built on this machine yet. \
                 Press enter to build it and sign in.",
                palette::muted(),
            ),
        ],
        CredentialStatus::Silent(reason) => vec![
            Line::styled("could not be asked", palette::bad()),
            Line::from(""),
            Line::styled(reason.clone(), palette::muted()),
        ],
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

// ------------------------------------------------------------------ the pages

fn draw_form(frame: &mut Frame, area: Rect, form: &Form) {
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
            Section::Agents => [AgentKind::Pi, AgentKind::Claude]
                .into_iter()
                .map(|kind| Row {
                    label: kind.to_string(),
                    note: (self.agent() == Some(kind)).then_some("(default)"),
                    kind: RowKind::Agent(kind),
                })
                .collect(),
            Section::Credentials => self
                .credentials()
                .into_iter()
                .map(|credential| Row {
                    label: credential.label.clone(),
                    note: None,
                    kind: RowKind::Credential(credential),
                })
                .collect(),
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
            Section::Remotes => self
                .configured_remotes()
                .into_iter()
                .map(|address| {
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
                        },
                    }
                })
                .collect(),
        };
        View {
            rows,
            base: self.base_description.clone(),
            tracker_profile_dir: self.tracker_profile_dir(),
        }
    }

    /// The credentials this machine holds: the installation, then each login a
    /// task is given.
    ///
    /// Names and places only. A credential is never read, and never shown.
    fn credentials(&self) -> Vec<Credential> {
        let mut credentials = Vec::new();
        let identity = self.config.identity();
        credentials.push(Credential {
            label: "Altitude".to_owned(),
            kind: CredentialKind::Installation {
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
        });
        for profile in self.profiles.iter() {
            let directory = self.config.profile_dir(profile.profile);
            credentials.push(Credential {
                label: profile.display.to_owned(),
                kind: CredentialKind::Profile {
                    profile: profile.profile,
                    directory: directory.display().to_string(),
                    status: self
                        .credential_status
                        .get(profile.profile)
                        .cloned()
                        .unwrap_or_default(),
                    mounts: profile.mounts.clone(),
                },
            });
        }
        credentials
    }

    /// The addresses the settings file configures.
    pub(in crate::tui) fn configured_remotes(&self) -> Vec<String> {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().remotes().to_vec())
            .unwrap_or_default()
    }

    /// How many rows the focused list holds, which is what moving needs.
    pub(in crate::tui) fn row_count(&self) -> usize {
        match self.panel.section {
            Section::Interface => 1,
            Section::Agents => 2,
            Section::Credentials => self.profiles.len() + 1,
            Section::Trackers => 1,
            Section::Environments => self.environments().len(),
            Section::Remotes => self.configured_remotes().len(),
        }
    }

    pub(in crate::tui) fn selected_row(&self) -> Option<Row> {
        self.settings_view()
            .rows
            .into_iter()
            .nth(self.panel.selected)
    }
}
