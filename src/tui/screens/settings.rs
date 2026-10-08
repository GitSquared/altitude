//! The settings panel: environments, remotes, and the installation's identity.
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
    widgets::{chrome, palette},
};

use std::{
    fs,
    path::Path,
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
    tui::widgets::text::{Field, FieldStyle, TextBox},
};

/// The gap between columns, so two focus bars can never touch.
const GAP: u16 = 3;

// ------------------------------------------------------------------ the lists

/// The three lists the panel holds.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Environments,
    Agent,
    Remotes,
    Interface,
    Identity,
}

impl Section {
    pub const ALL: [Section; 5] = [
        Self::Environments,
        Self::Agent,
        Self::Remotes,
        Self::Interface,
        Self::Identity,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Environments => "Environments",
            Self::Agent => "Agent",
            Self::Remotes => "Remotes",
            Self::Interface => "Interface",
            Self::Identity => "Identity",
        }
    }

    /// Whether this list can grow and shrink.
    ///
    /// The interface's list is a fixed set of preferences, so it can be changed
    /// but neither added to nor removed from.
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
pub struct Row {
    /// What the list shows: a name where there is one, an address where there is
    /// not yet.
    pub label: String,
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
    /// A coding agent, and whether it is the one in use.
    Agent {
        kind: AgentKind,
        in_use: bool,
    },
    Remote {
        address: String,
        /// What that address last answered with, if it has been met.
        remembered: Option<RemoteKey>,
    },
    Switch {
        setting: Setting,
        on: bool,
    },
}

impl Row {
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
    /// The identities on this machine: this installation, then each login a task
    /// is given.
    pub identities: Vec<Identity>,
    /// What the base environment is made of, once it has been built here.
    pub base: Option<EnvironmentDescription>,
}

/// One identity, as the list shows it.
pub struct Identity {
    pub label: String,
    pub kind: IdentityKind,
}

pub enum IdentityKind {
    /// This installation: the key that names it and where its state lives.
    Installation {
        label: String,
        key: String,
        data_dir: String,
    },
    /// A profile a task mounts. Presence and place only: never the contents.
    Profile {
        profile: String,
        directory: String,
        present: bool,
        /// The account name, where one can be read without reading a credential.
        account: Option<String>,
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
}

impl Panel {
    pub fn new() -> Self {
        Self {
            section: Section::Environments,
            selected: 0,
            depth: Depth::List,
            form: None,
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

pub fn draw(frame: &mut Frame, area: Rect, panel: &Panel, view: &View) {
    draw_columns(frame, area, panel, view);
    if let Some(form) = &panel.form {
        draw_form(frame, area, form);
    }
}

/// A row of a list: inverted when it holds the focus, padded so the background
/// reads as a bar rather than as a word.
fn chip(label: &str, width: usize, focused: bool) -> Line<'static> {
    let mut text = label.to_owned();
    let used = Line::from(text.as_str()).width();
    if used < width {
        text.push_str(&" ".repeat(width - used));
    }
    if focused {
        Line::styled(text, palette::focus())
    } else {
        Line::from(text)
    }
}

fn draw_columns(frame: &mut Frame, area: Rect, panel: &Panel, view: &View) {
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
            let row = chip(
                section.title(),
                usize::from(rail.width),
                rail_focused && *section == panel.section,
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
    let list_items = view
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let focused = list_focused && index == panel.selected;
            let line = chip(&row.label, usize::from(list_area.width), focused);
            ListItem::new(if list_focused {
                line
            } else {
                line.style(palette::muted())
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
                .is_some_and(|row| !row.is_actionable()));
    let pane = detail(panel, view, detail_focused);
    frame.render_widget(
        if detail_focused {
            pane
        } else {
            pane.style(palette::muted())
        },
        detail_area,
    );
}

/// The third column: what is known about the highlighted row.
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

    match panel.section {
        Section::Identity => match view.identities.get(panel.selected) {
            Some(identity) => {
                Paragraph::new(identity_lines(identity, heading)).wrap(Wrap { trim: true })
            }
            None => Paragraph::new(Span::styled("", palette::muted())),
        },

        _ => match panel.selected_row(view) {
            Some(Row {
                kind: RowKind::Base,
                ..
            }) => Paragraph::new(base_lines(view.base.as_ref(), heading)).wrap(Wrap { trim: true }),
            Some(Row {
                label,
                kind:
                    RowKind::Environment {
                        setup, is_default, ..
                    },
            }) => {
                let mut lines = vec![heading(label.clone())];
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
                Paragraph::new(lines).wrap(Wrap { trim: true })
            }
            Some(Row {
                label,
                kind: RowKind::Agent { kind, in_use },
            }) => {
                let mut lines = vec![
                    heading(label.clone()),
                    Line::from(""),
                    Line::styled(
                        if *in_use { "in use" } else { "not in use" },
                        if *in_use {
                            palette::good()
                        } else {
                            palette::muted()
                        },
                    ),
                    Line::from(""),
                    Line::from(match kind {
                        AgentKind::Pi => {
                            "Altitude's own agent. It needs no provider login of its own."
                        }
                        AgentKind::Claude => {
                            "Claude Code. Its login lives in Claude's profile on this machine, \
                             and never travels with a task."
                        }
                    }),
                ];
                lines.push(Line::from(""));
                lines.push(
                    Line::from("Choose it to drive new tasks.".to_owned()).style(palette::muted()),
                );
                Paragraph::new(lines).wrap(Wrap { trim: true })
            }
            Some(Row {
                label,
                kind: RowKind::Switch { setting, on },
            }) => Paragraph::new(vec![
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
            ])
            .wrap(Wrap { trim: true }),
            Some(Row {
                label,
                kind:
                    RowKind::Remote {
                        address,
                        remembered,
                    },
            }) => {
                let mut lines = vec![
                    heading(label.clone()),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("address   ", palette::muted()),
                        address.clone().into(),
                    ]),
                    Line::from(""),
                ];
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
                Paragraph::new(lines).wrap(Wrap { trim: true })
            }
            None => Paragraph::new(match panel.section {
                Section::Environments => Span::styled(
                    "Nothing configured. New tasks run in the base environment.",
                    palette::muted(),
                ),
                Section::Remotes => Span::styled(
                    "No remotes are configured. Press n to add one.",
                    palette::muted(),
                ),
                Section::Agent => Span::styled("", palette::muted()),
                Section::Interface => Span::styled("", palette::muted()),
                Section::Identity => unreachable!("identity has no rows"),
            }),
        },
    }
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
            lines.push(Line::from(format!(
                "New tasks are built from Alpine Linux {}, with these packages installed:",
                description.alpine_release
            )));
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
                        .map(|package| Line::from(package.clone())),
                );
            }
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "Built {}, and refreshed automatically every {} hours, so security patches \
                 arrive without you doing anything.",
                met_ago(description.built_at_ms),
                REFRESH_AFTER.as_secs() / 3_600
            )));
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

/// What is known about one identity.
fn identity_lines(
    identity: &Identity,
    heading: impl Fn(String) -> Line<'static>,
) -> Vec<Line<'static>> {
    let mut lines = vec![heading(identity.label.clone()), Line::from("")];
    match &identity.kind {
        IdentityKind::Installation {
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
        IdentityKind::Profile {
            profile,
            directory,
            present,
            account,
            mounts,
        } => {
            lines.push(Line::styled(
                if *present { "set up" } else { "not set up yet" },
                if *present {
                    palette::good()
                } else {
                    palette::muted()
                },
            ));
            lines.push(Line::from(""));
            if let Some(account) = account {
                lines.push(Line::from(vec![
                    Span::styled("account   ", palette::muted()),
                    account.clone().into(),
                ]));
            }
            lines.push(Line::from(vec![
                Span::styled("profile   ", palette::muted()),
                profile.clone().into(),
            ]));
            lines.push(Line::from(vec![
                Span::styled("on disk   ", palette::muted()),
                directory.clone().into(),
            ]));
            if !mounts.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::styled("A task sees", palette::muted()));
                lines.extend(
                    mounts
                        .iter()
                        .map(|mount| Line::from(format!("{}  →  {}", mount.guest, mount.path))),
                );
            }
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "A login is kept here and mounted into every task, so it survives between them. \
                 Altitude shows where it lives and never its contents.",
                palette::muted(),
            ));
        }
    }
    lines
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
        2..=59 => format!("{minutes} minutes ago"),
        60..=2879 => format!("{} hours ago", minutes / 60),
        _ => format!("{} days ago", minutes / 1_440),
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
        let default_environment = self.default_environment().to_owned();
        let rows = match self.panel.section {
            Section::Environments => self
                .environments()
                .into_iter()
                .map(|environment| {
                    let built_in = environment.is_base();
                    let is_default = environment.name.eq_ignore_ascii_case(&default_environment);
                    Row {
                        label: match (built_in, is_default) {
                            (true, _) => format!("{} (built in)", environment.name),
                            (false, true) => format!("{} (default)", environment.name),
                            (false, false) => environment.name.clone(),
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
                    Row {
                        // A name is what the remote called itself, so an address
                        // that has never answered shows itself instead.
                        label: match &remembered {
                            Some(key) => key.label.clone(),
                            None => address.clone(),
                        },
                        kind: RowKind::Remote {
                            address,
                            remembered,
                        },
                    }
                })
                .collect(),
            Section::Agent => [AgentKind::Pi, AgentKind::Claude]
                .into_iter()
                .map(|kind| Row {
                    label: kind.to_string(),
                    kind: RowKind::Agent {
                        kind,
                        in_use: self.agent() == Some(kind),
                    },
                })
                .collect(),
            Section::Interface => vec![Row {
                label: Setting::KeyboardHints.label().to_owned(),
                kind: RowKind::Switch {
                    setting: Setting::KeyboardHints,
                    on: self.keyboard_hints(),
                },
            }],
            Section::Identity => Vec::new(),
        };
        let identities = self.identities();
        View {
            rows,
            identities,
            base: self.base_description.clone(),
        }
    }

    /// The identities this machine holds: the installation, then each login a
    /// task is given.
    ///
    /// Names and places only. A credential is never read, and never shown.
    fn identities(&self) -> Vec<Identity> {
        let mut identities = Vec::new();
        let identity = self.config.identity();
        identities.push(Identity {
            label: "Altitude".to_owned(),
            kind: IdentityKind::Installation {
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
            identities.push(Identity {
                label: profile_label(profile.profile).to_owned(),
                kind: IdentityKind::Profile {
                    profile: profile.profile.to_owned(),
                    present: directory.is_dir(),
                    account: account_name(&directory),
                    directory: directory.display().to_string(),
                    mounts: profile.mounts.clone(),
                },
            });
        }
        identities
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
            Section::Environments => self.environments().len(),
            Section::Agent => 2,
            Section::Remotes => self.configured_remotes().len(),
            Section::Interface => 1,
            Section::Identity => self.profiles.len() + 1,
        }
    }

    pub(in crate::tui) fn selected_row(&self) -> Option<Row> {
        self.settings_view()
            .rows
            .into_iter()
            .nth(self.panel.selected)
    }
}

/// A profile's name, for a list that has no room for its path.
fn profile_label(profile: &str) -> &str {
    match profile.rsplit('/').next().unwrap_or(profile) {
        "claude" => "Claude Code",
        "pi" => "Pi",
        "github" => "GitHub",
        other => other,
    }
}

/// The account a profile is signed in as, where a name can be read without
/// reading a credential.
///
/// The GitHub CLI keeps the account name beside the token in its hosts file, and
/// only the name is read.
fn account_name(directory: &Path) -> Option<String> {
    let hosts = fs::read_to_string(directory.join("gh/hosts.yml")).ok()?;
    hosts.lines().find_map(|line| {
        let value = line.trim().strip_prefix("user:")?;
        let value = value.trim().trim_matches('"');
        (!value.is_empty()).then(|| value.to_owned())
    })
}
