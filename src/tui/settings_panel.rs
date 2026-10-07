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

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    style::Stylize,
    text::Line,
    widgets::{Block, BorderType, Clear, List, ListItem, Padding, Paragraph, Wrap},
};

use crate::{
    agent::AgentKind,
    isolation::{EnvironmentDescription, REFRESH_AFTER},
    remote_keys::RemoteKey,
    tui::text::{Field, FieldStyle, TextBox},
};

/// The gap between columns, so two focus bars can never touch.
const GAP: u16 = 3;

// ------------------------------------------------------------------ the lists

/// The three lists the panel holds.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Environments,
    Remotes,
    Identity,
}

impl Section {
    pub const ALL: [Section; 3] = [Self::Environments, Self::Remotes, Self::Identity];

    pub fn title(self) -> &'static str {
        match self {
            Self::Environments => "Environments",
            Self::Remotes => "Remotes",
            Self::Identity => "Identity",
        }
    }

    /// Whether the rows of this list can be added to, edited and deleted.
    pub fn editable(self) -> bool {
        !matches!(self, Self::Identity)
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
        harness: AgentKind,
        setup: String,
        is_default: bool,
    },
    Remote {
        address: String,
        /// What that address last answered with, if it has been met.
        remembered: Option<RemoteKey>,
    },
}

impl Row {
    /// Whether this row opens into anything.
    ///
    /// The built-in base does not: there is no form behind it and nothing to
    /// configure, so selecting it is already as far as it goes, and its
    /// description is shown without a second step.
    pub fn is_actionable(&self) -> bool {
        !matches!(self.kind, RowKind::Base)
    }
}

/// Everything the panel draws, assembled by the caller from settings, the
/// remembered keys, and the built environments.
pub struct View {
    pub rows: Vec<Row>,
    pub identity_label: String,
    pub identity_key: String,
    /// What the base environment is made of, once it has been built here.
    pub base: Option<EnvironmentDescription>,
}

// ------------------------------------------------------------------ the state

/// The panel's state: where the focus is, and whatever is drawn over it.
pub struct Panel {
    pub open: bool,
    pub section: Section,
    pub selected: usize,
    pub depth: Depth,
    pub form: Option<Form>,
    pub confirm: Option<Confirm>,
}

impl Panel {
    pub fn new() -> Self {
        Self {
            open: false,
            section: Section::Environments,
            selected: 0,
            depth: Depth::List,
            form: None,
            confirm: None,
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
            &[
                Focus::Primary,
                Focus::Harness,
                Focus::Setup,
                Focus::Cancel,
                Focus::Save,
            ]
        }
    }
}

/// Which part of the form holds the caret.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Primary,
    Harness,
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
    pub harness: AgentKind,
    pub setup: TextBox,
    pub focus: Focus,
}

impl Form {
    pub fn new(target: Target, primary: &str, harness: AgentKind, setup: &str) -> Self {
        Self {
            target,
            primary: Field::with(primary),
            harness,
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

/// A question asked before something is destroyed.
pub struct Confirm {
    pub question: String,
    /// 0 is cancel, 1 is the destructive button.
    pub button: usize,
    /// What would be destroyed if the destructive button is taken.
    pub target: Target,
}

// ----------------------------------------------------------------- the drawing

pub fn draw(frame: &mut Frame, panel: &Panel, view: &View) {
    draw_columns(frame, panel, view);
    if let Some(form) = &panel.form {
        draw_form(frame, form);
    }
    if let Some(confirm) = &panel.confirm {
        draw_confirm(frame, confirm);
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
        Line::from(text).black().on_cyan().bold()
    } else {
        Line::from(text)
    }
}

fn draw_columns(frame: &mut Frame, panel: &Panel, view: &View) {
    let block = Block::bordered()
        .title(" Settings ")
        .title_bottom(hint(panel))
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1));
    let area = frame.area();
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
            ListItem::new(if rail_focused { row } else { row.dim() })
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
            ListItem::new(if list_focused { line } else { line.dim() })
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
    frame.render_widget(if detail_focused { pane } else { pane.dim() }, detail_area);
}

fn hint(panel: &Panel) -> Line<'static> {
    let text = if panel.section.editable() {
        " tab up and down a column · j/k move · enter edit · n new · d delete · esc close "
    } else {
        " tab up and down a column · j/k move · esc close "
    };
    Line::from(text.dim())
}

/// The third column: what is known about the highlighted row.
/// The third column. `focused` is decided by the caller, because whether this
/// column holds the focus is not the same question as how deep the focus is: a
/// row with nothing inside it is already here.
fn detail(panel: &Panel, view: &View, focused: bool) -> Paragraph<'static> {
    // The heading is the column's focus indicator: it has no rows to bar.
    let heading = move |text: String| {
        if focused {
            Line::from(text.cyan().bold())
        } else {
            Line::from(text.bold())
        }
    };

    match panel.section {
        Section::Identity => Paragraph::new(vec![
            heading("This installation".to_owned()),
            Line::from(""),
            Line::from(vec!["label     ".dim(), view.identity_label.clone().into()]),
            Line::from(vec!["key       ".dim(), view.identity_key.clone().into()]),
            Line::from(""),
            Line::from("Read-only: the key names this machine, and cannot be edited here.".dim()),
        ])
        .wrap(Wrap { trim: true }),

        _ => match panel.selected_row(view) {
            Some(Row {
                kind: RowKind::Base,
                ..
            }) => Paragraph::new(base_lines(view.base.as_ref(), heading)).wrap(Wrap { trim: true }),
            Some(Row {
                label,
                kind:
                    RowKind::Environment {
                        harness,
                        setup,
                        is_default,
                        ..
                    },
            }) => {
                let mut lines = vec![
                    heading(label.clone()),
                    Line::from(""),
                    Line::from(vec!["harness   ".dim(), harness.to_string().into()]),
                ];
                if *is_default {
                    lines.push(Line::from(""));
                    lines.push(Line::from(
                        "New tasks use this environment by default.".green(),
                    ));
                }
                lines.push(Line::from(""));
                lines.push(Line::from("setup command".dim()));
                if setup.trim().is_empty() {
                    lines.push(Line::from(
                        "Nothing on top of the base. This environment is the base, by another name."
                            .dim(),
                    ));
                } else {
                    lines.push(Line::from(setup.clone()));
                }
                Paragraph::new(lines).wrap(Wrap { trim: true })
            }
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
                    Line::from(vec!["address   ".dim(), address.clone().into()]),
                    Line::from(""),
                ];
                match remembered {
                    Some(key) => {
                        lines.push(Line::from(vec!["key       ".dim(), key.key.clone().into()]));
                        lines.push(Line::from(vec![
                            "met       ".dim(),
                            met_ago(key.seen_at_ms).into(),
                        ]));
                    }
                    None => {
                        lines.push(Line::from("never met".dim()));
                        lines.push(Line::from(
                            "No key is remembered for this address yet. Connecting will record \
                             the one it answers with."
                                .dim(),
                        ));
                    }
                }
                Paragraph::new(lines).wrap(Wrap { trim: true })
            }
            None => Paragraph::new(match panel.section {
                Section::Environments => {
                    "Nothing configured. New tasks run in the base environment.".dim()
                }
                Section::Remotes => "No remotes are configured. Press n to add one.".dim(),
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
            lines.push(Line::from(if description.packages.is_empty() {
                "nothing beyond the base image".to_owned()
            } else {
                description.packages.join(" ")
            }));
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
    lines.push(Line::from(""));
    lines.push(Line::from("Built in: it cannot be edited or deleted.").dim());
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

fn draw_form(frame: &mut Frame, form: &Form) {
    let block = Block::bordered()
        .title(format!(" {} ", form.target.title()))
        .title_bottom(
            " tab/shift-tab move · enter validates, or adds a line in the command · esc leaves the \
             box, then the page "
                .dim(),
        )
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(2));
    let area = frame.area();
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
        let [primary, harness, setup] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Min(5),
        ])
        .areas(fields);

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

        draw_label(frame, harness, "Harness");
        frame.render_widget(
            Paragraph::new(harness_line(form.harness, form.focus == Focus::Harness)),
            field_box(harness),
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
    draw_button(frame, cancel, "cancel", form.focus == Focus::Cancel, false);
    draw_button(frame, save, "save", form.focus == Focus::Save, true);
}

fn draw_label(frame: &mut Frame, area: Rect, text: &str) {
    let [label, _] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(Line::from(text).dim()), label);
}

/// The box a field's input is drawn in, one row below its label.
fn field_box(area: Rect) -> Rect {
    let [_, input] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    input
}

fn harness_line(harness: AgentKind, focused: bool) -> Line<'static> {
    let mut spans = Vec::new();
    for choice in [AgentKind::Pi, AgentKind::Claude] {
        let picked = choice == harness;
        let text = format!(" {} {} ", if picked { "◉" } else { "○" }, choice);
        spans.push(if focused && picked {
            text.black().on_cyan().bold()
        } else if picked {
            text.cyan()
        } else {
            text.dim()
        });
        spans.push("   ".into());
    }
    Line::from(spans)
}

fn draw_button(frame: &mut Frame, area: Rect, label: &str, focused: bool, primary: bool) {
    let text = format!(" {label} ");
    let style = if focused {
        text.black().on_cyan().bold()
    } else if primary {
        text.cyan()
    } else {
        text.dim()
    };
    let [_, line, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(Line::from(style)), line);
}

fn draw_confirm(frame: &mut Frame, confirm: &Confirm) {
    // A confirmation is a question, not a page: small, centred, and over.
    let area = centered(60, 7, frame.area());
    let block = Block::bordered()
        .title(" Are you sure? ")
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(2));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let [question, buttons] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(inner);
    frame.render_widget(
        Paragraph::new(confirm.question.clone()).wrap(Wrap { trim: true }),
        question,
    );
    let [_, cancel, _, delete] = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(10),
        Constraint::Length(3),
        Constraint::Length(10),
    ])
    .areas(buttons);
    draw_button(frame, cancel, "cancel", confirm.button == 0, false);
    draw_button(frame, delete, "delete", confirm.button == 1, false);
}

fn centered(percent_x: u16, height: u16, area: Rect) -> Rect {
    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .areas(vertical);
    horizontal
}
