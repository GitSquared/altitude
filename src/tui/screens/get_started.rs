//! The first thing a new installation shows, and the only thing it shows until
//! the work could actually start.
//!
//! A card rather than a wizard: the name and the commit identity are edited where
//! they are read, and a row that asks a question opens its own picker over the
//! card before the sign-in that follows it. The card is not dismissible while a
//! task would not run: a first question that can be answered by accident is not
//! a question.

use std::slice;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
};

use crate::{
    agent::AgentKind,
    tracker::TrackerKind,
    tui::{
        app::{CredentialStatus, TrackerState},
        popups::repositories::Repositories,
        widgets::{
            button::Button,
            chrome::Hint,
            palette,
            select::{Offer, Select},
            text::Field,
            utils::{shimmer, truncate},
        },
    },
    vcs::{Repository, VcsProviderKind},
};

use super::settings::GitIdentity;

/// How wide the card is allowed to get. Content, not a panel: at this width the
/// eye has a column to read, and the task list behind it keeps its own.
const CARD_WIDTH: u16 = 80;

/// The label column, wide enough for "Version control" and a space after it.
const LABEL_WIDTH: usize = 15;

/// The action column, on the right of every row.
const ACTION_WIDTH: usize = 16;

/// The rows the help under the steps keeps, whatever it says.
///
/// The longest help is the remote one, five lines at the card's width; the sixth
/// is spare so a slightly narrower terminal does not clip it. It is a constant,
/// not measured per draw, because its whole job is to stop the footer moving
/// when the words change.
const HELP_ROWS: u16 = 6;

/// What a row's mark says about it.
///
/// Focus is the bold value, not the mark: a mark that changed when the caret
/// moved would say one thing about a row and then another without anything
/// having happened.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    /// Settled: answered, chosen, or deliberately none.
    Done,
    /// Chosen, but whether it is usable is not known yet: a check is out, or no
    /// answer came.
    Blocking,
    /// Nothing has been answered here yet. Whether anything depends on it is
    /// what the row's copy says.
    Unset,
    /// There is an answer to give here that has not been given.
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

/// What the keys are on.
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

const ROWS: [Focus; 8] = [
    Focus::Name,
    Focus::Agent,
    Focus::VersionControl,
    Focus::Commits,
    Focus::Tracker,
    Focus::Remote,
    // The footer reads left to right: Settings is the left button and starts the
    // work is the right one, whatever the primary weight says.
    Focus::Settings,
    Focus::Ready,
];

/// A remote announcing on this network that this installation does not know yet.
#[derive(Clone)]
pub struct RemoteOffer {
    pub label: String,
    pub address: String,
}

/// What the screen has to say about the installation it is setting up.
///
/// Owned values rather than the application's, because the screen is drawn while
/// the foreground is borrowed, and the two things it reads are small.
#[derive(Clone)]
pub struct SetupState {
    pub agent: Option<AgentKind>,
    pub agent_status: CredentialStatus,
    pub vcs: VcsProviderKind,
    pub vcs_status: CredentialStatus,
    /// Whether the repositories behind the VCS login are still being read.
    pub vcs_listing: bool,
    pub repository: Option<String>,
    /// The machine this project runs on, when it is not this one.
    pub host: Option<String>,
    /// A remote announcing here that this installation has not adopted.
    pub offer: Option<RemoteOffer>,
    pub tracker: TrackerState,
    /// The identity and signing the commits made in a task carry.
    pub commits: GitIdentity,
    /// Whether the version-control profile holds a signing key.
    pub has_key: bool,
}

/// The identity and signing as the card edits them.
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

    /// Put back what is saved, and close.
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
    pub name: Field,
    /// Whether the name is taking letters, as opposed to being the row the keys
    /// are on. Typing is a state you enter, not one the caret is always in.
    pub name_editing: bool,
    pub agent: Select<AgentKind>,
    pub vcs: Select<VcsProviderKind>,
    pub tracker: Select<TrackerKind>,
    /// The repository this project works in, shown as the pill it is rather than
    /// as text with a copied mark.
    pub repository: Select<Repository>,
    pub commits: Commits,
    pub repositories: Option<Repositories>,
}

impl GetStarted {
    /// Open on the first thing still missing, so the card starts where the work
    /// is rather than at the top of a list.
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
            )
            .without_search(),
            vcs: Select::new(
                "version control",
                VcsProviderKind::ALL
                    .iter()
                    .copied()
                    .map(|kind| Offer::new(kind, kind.label()))
                    .collect(),
                0,
            )
            .without_search(),
            tracker: Select::new(
                "task tracker",
                TrackerKind::ALL
                    .iter()
                    .copied()
                    .map(|kind| Offer::new(kind, kind.label()))
                    .collect(),
                0,
            )
            .without_search(),
            repository: repository_select(cached_repositories, repository),
        }
    }

    pub fn move_focus(&mut self, forward: bool) {
        let at = ROWS.iter().position(|row| *row == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % ROWS.len()
        } else {
            (at + ROWS.len() - 1) % ROWS.len()
        };
        self.focus = ROWS[next];
    }

    /// Whether the keys are characters here: the name, or a list long enough
    /// that typing narrows it.
    pub fn is_typing(&self) -> bool {
        self.name_editing
            || (self.commits.open && self.commits.takes_text())
            || self.agent.is_searching()
            || self.vcs.is_searching()
            || self.tracker.is_searching()
            || self
                .repositories
                .as_ref()
                .is_some_and(|browser| browser.select.is_searching())
    }

    /// Take the repository the list just returned, and show it.
    pub fn set_repository(&mut self, repository: &Repository) {
        self.repository = repository_select(slice::from_ref(repository), Some(&repository.url));
    }

    /// Put the name back to what is saved, when a draft is abandoned.
    pub fn reset_name(&mut self, name: &str) {
        self.name = Field::with(name);
    }

    pub fn hints(&self, setup: &SetupState) -> Vec<Hint> {
        if self.commits.open {
            return match self.commits.field {
                CommitsField::Name | CommitsField::Email => vec![
                    Hint::new("type", "the value"),
                    Hint::new("tab", "next"),
                    Hint::new("enter", "save"),
                    Hint::new("esc", "save and leave"),
                ],
                CommitsField::Sign => vec![
                    Hint::new("enter", "turn it on or off"),
                    Hint::new("tab", "next"),
                    Hint::new("esc", "save and leave"),
                ],
                CommitsField::Key => vec![
                    Hint::new("enter", "show or make the key"),
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
                Hint::new("enter", "save"),
                Hint::new("esc", "save and leave"),
            ];
        }
        if self.focus == Focus::Name {
            return vec![
                Hint::new("enter", "edit the name"),
                Hint::new("j/k", "move"),
            ];
        }
        let mut hints = vec![Hint::new("j/k", "move")];
        match self.focus {
            Focus::Agent => match setup.agent {
                Some(_) => {
                    hints.insert(0, Hint::new("enter", "sign in"));
                    hints.insert(1, Hint::new("space", "change"));
                }
                None => hints.insert(0, Hint::new("enter", "choose")),
            },
            Focus::VersionControl => {
                hints.insert(
                    0,
                    Hint::new(
                        "enter",
                        if signed_in(&setup.vcs_status) {
                            "choose repository"
                        } else {
                            "choose provider"
                        },
                    ),
                );
            }
            Focus::Commits => hints.insert(0, Hint::new("enter", "edit")),
            Focus::Tracker => hints.insert(
                0,
                Hint::new(
                    "enter",
                    if setup.tracker == TrackerState::Connected {
                        "manage"
                    } else {
                        "connect"
                    },
                ),
            ),
            Focus::Remote => {
                if setup.host.is_none() && setup.offer.is_some() {
                    hints.insert(0, Hint::new("enter", "add it"));
                }
            }
            Focus::Ready => hints.insert(
                0,
                Hint::new(
                    "enter",
                    if ready(setup, &self.name) {
                        "start"
                    } else {
                        "not yet"
                    },
                ),
            ),
            Focus::Settings => hints.insert(0, Hint::new("enter", "open settings")),
            Focus::Name => {}
        }
        hints
    }
}

/// Whether every required answer is in, and the name has been written down.
pub fn ready(setup: &SetupState, name: &Field) -> bool {
    !name.is_blank()
        && setup.agent.is_some()
        && signed_in(&setup.agent_status)
        && setup.repository.is_some()
        && signed_in(&setup.vcs_status)
        && setup.commits.is_set()
}

/// Whether a profile's login is known to work. A file on disk counts until a
/// check says otherwise, because that is what a task would mount.
pub fn signed_in(status: &CredentialStatus) -> bool {
    matches!(
        status,
        CredentialStatus::Checked {
            signed_in: true,
            ..
        } | CredentialStatus::Saved
    )
}

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    panel: &mut GetStarted,
    setup: &SetupState,
    tick: Option<usize>,
) {
    let width = area.width.min(CARD_WIDTH);
    let x = area.x + area.width.saturating_sub(width) / 2;

    let subtitle = subtitle();
    let subtitle_height = wrapped_height(subtitle_plain(), width);
    let rows = checklist(panel, setup, width, tick);
    let rows_height = rows.len() as u16;
    let detail = if panel.commits.open {
        "Commits carry this name and email, and signing marks them as yours. The key lives in \
         your version-control profile, not in a task. Enter saves; Esc puts back what is saved."
            .to_owned()
    } else {
        detail_text(panel.focus, setup)
    };
    let detail_height = HELP_ROWS.min(area.height);
    let height = (6 + subtitle_height + rows_height + detail_height).min(area.height);
    let y = area.y + area.height.saturating_sub(height) / 2;
    let card = Rect {
        x,
        y,
        width,
        height,
    };
    let constraints = [
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(subtitle_height),
        Constraint::Length(1),
        Constraint::Length(rows_height),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ];
    let areas = Layout::vertical(constraints).split(card);

    frame.render_widget(
        Paragraph::new(Line::styled("GET STARTED", palette::heading())),
        areas[0],
    );
    frame.render_widget(
        Paragraph::new(subtitle)
            .wrap(Wrap { trim: true })
            .style(palette::primary()),
        areas[2],
    );
    frame.render_widget(Paragraph::new(rows), areas[4]);
    frame.render_widget(
        Paragraph::new(detail)
            .wrap(Wrap { trim: true })
            .style(palette::muted()),
        areas[6],
    );
    draw_footer(frame, *areas.last().expect("a footer"), panel, setup);

    // A picker is a pop-up over the card, not rows of it: the choice is a
    // question of its own, and it comes before the sign-in that follows it.
    if panel.agent.is_open() {
        panel
            .agent
            .draw_list_below(frame, area, row_anchor(areas[4], 1));
    } else if panel.vcs.is_open() {
        panel
            .vcs
            .draw_list_below(frame, area, row_anchor(areas[4], 2));
    } else if panel.tracker.is_open() {
        panel
            .tracker
            .draw_list_below(frame, area, row_anchor(areas[4], 4));
    } else if let Some(browser) = panel.repositories.as_mut() {
        browser.draw(frame, area, row_anchor(areas[4], 2), tick);
    }
}

fn subtitle() -> Text<'static> {
    Text::from(vec![
        Line::from(vec![
            Span::styled("welcome to ", palette::primary()),
            Span::styled("⛰  altitude", palette::heading()),
            Span::styled(
                ", a tool to pilot agents efficiently and keep your focus on the decisions that matter.",
                palette::primary(),
            ),
        ]),
        Line::from(""),
        Line::styled("let's get you set up:", palette::primary()),
    ])
}

/// The welcome without its styling, for measuring how tall it will be.
fn subtitle_plain() -> &'static str {
    "welcome to ⛰  altitude, a tool to pilot agents efficiently and keep your focus on the \
     decisions that matter.\n\nlet's get you set up:"
}

fn checklist(
    panel: &GetStarted,
    setup: &SetupState,
    width: u16,
    tick: Option<usize>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let focused = panel.focus;

    let name_value = if panel.name_editing {
        panel.name.marked()
    } else {
        panel.name.value().to_owned()
    };
    lines.push(row(
        if panel.name.is_blank() {
            Marker::Unset
        } else {
            Marker::Done
        },
        "Project name",
        &name_value,
        if panel.name_editing {
            "enter save"
        } else {
            "enter edit"
        },
        focused == Focus::Name,
        width,
    ));

    // A negative answer never reaches here: the card has already put the row
    // back to the picker, so what remains is signed in, being checked, or a
    // check that has not answered yet.
    let agent_tone = if focused == Focus::Agent {
        palette::primary().bold()
    } else {
        palette::primary()
    };
    // Once the login is there, the name is the whole answer: the row is about
    // which agent a task runs, not how its login is kept.
    let agent_value = match setup.agent {
        None => vec![Span::styled("not chosen", agent_tone)],
        Some(kind) if signed_in(&setup.agent_status) => {
            vec![Span::styled(kind.to_string(), agent_tone)]
        }
        Some(kind) => {
            let mut spans = vec![Span::styled(format!("{kind} · "), agent_tone)];
            let status = setup.agent_status.summary();
            if setup.agent_status == CredentialStatus::Checking {
                spans.extend(shimmer(status, tick));
            } else {
                spans.push(Span::styled(status, agent_tone));
            }
            spans
        }
    };
    lines.push(row_spans(
        match (setup.agent, signed_in(&setup.agent_status)) {
            (Some(_), true) => Marker::Done,
            (Some(_), false) => Marker::Blocking,
            (None, _) => Marker::Unset,
        },
        "Coding agent",
        agent_value,
        if setup.agent.is_some() {
            "enter sign in"
        } else {
            "enter choose"
        },
        focused == Focus::Agent,
        width,
    ));

    let vcs_signed_in = signed_in(&setup.vcs_status);
    let listing = panel
        .repositories
        .as_ref()
        .is_some_and(Repositories::is_listing)
        || (setup.vcs_listing && panel.repositories.is_none());
    if listing {
        lines.push(row_spans(
            Marker::Blocking,
            "Version control",
            shimmer("listing repositories…", tick),
            "",
            focused == Focus::VersionControl,
            width,
        ));
    } else if setup.vcs_status == CredentialStatus::Checking {
        lines.push(row_spans(
            Marker::Blocking,
            "Version control",
            shimmer(setup.vcs_status.summary(), tick),
            "",
            focused == Focus::VersionControl,
            width,
        ));
    } else if vcs_signed_in {
        let prefix = 1 + 2 + LABEL_WIDTH + 2;
        let room = usize::from(width).saturating_sub(prefix + ACTION_WIDTH);
        let pill = panel
            .repository
            .pill_spans(focused == Focus::VersionControl, false, room);
        lines.push(row_spans(
            if setup.repository.is_some() {
                Marker::Done
            } else {
                Marker::Unset
            },
            "Version control",
            pill,
            "enter choose",
            focused == Focus::VersionControl,
            width,
        ));
    } else {
        lines.push(row(
            Marker::Unset,
            "Version control",
            "not chosen",
            "enter choose",
            focused == Focus::VersionControl,
            width,
        ));
    }

    // Shown, not gated: the identity is installation-wide and usually already
    // set, and this card has no editor for it. A gate would trap an operator
    // whose profile has no name on a screen that cannot give it one.
    let commits_value = if setup.commits.is_set() {
        let name = setup.commits.name.as_deref().unwrap_or_default();
        if setup.commits.signs_commits {
            format!("{name} · signed")
        } else {
            format!("{name} · not signed")
        }
    } else {
        "not set".to_owned()
    };
    lines.push(row(
        if setup.commits.is_set() {
            Marker::Done
        } else {
            Marker::Unset
        },
        "Commits",
        &commits_value,
        "enter edit",
        focused == Focus::Commits,
        width,
    ));
    if panel.commits.open {
        lines.extend(commits_editor(&panel.commits, setup, width));
    }

    let tracker = setup.tracker;
    let tracker_value = if tracker == TrackerState::Connected {
        format!("{} · connected", TrackerKind::sole().label())
    } else {
        "none · optional".to_owned()
    };
    lines.push(row(
        if tracker == TrackerState::Connected {
            Marker::Done
        } else {
            Marker::Unset
        },
        "Task tracker",
        &tracker_value,
        if tracker == TrackerState::Connected {
            "enter manage"
        } else {
            "enter connect"
        },
        focused == Focus::Tracker,
        width,
    ));

    let remote_value = match (&setup.host, &setup.offer) {
        (Some(host), _) => host.clone(),
        (None, Some(offer)) => format!("found {}", offer.label),
        (None, None) => "none · runs here".to_owned(),
    };
    lines.push(row(
        match (&setup.host, &setup.offer) {
            (Some(_), _) => Marker::Done,
            (None, Some(_)) => Marker::Question,
            (None, None) => Marker::Unset,
        },
        "Remote",
        &remote_value,
        if setup.host.is_none() && setup.offer.is_some() {
            "enter add"
        } else {
            ""
        },
        focused == Focus::Remote,
        width,
    ));

    lines
}

fn draw_footer(frame: &mut Frame, area: Rect, panel: &GetStarted, setup: &SetupState) {
    let settings = Button::secondary("Open Settings").focused(panel.focus == Focus::Settings);
    let go = Button::primary("Ready, set, go")
        .focused(panel.focus == Focus::Ready)
        .enabled(ready(setup, &panel.name));
    let width = settings.width() + 2 + go.width();
    let [note_area, buttons_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::styled(
            "You can tweak these options in Settings later.",
            palette::muted(),
        )),
        note_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            settings.span(),
            Span::raw("  "),
            go.span(),
        ])),
        buttons_area,
    );
}

/// A row whose value is already spans, so a widget can print its own pill or a
/// status that moves.
fn row_spans(
    marker: Marker,
    label: &str,
    value: Vec<Span<'static>>,
    action: &str,
    focused: bool,
    width: u16,
) -> Line<'static> {
    let prefix = format!("{}  {label:<LABEL_WIDTH$}  ", marker.glyph());
    let trailing = format!("{action:<ACTION_WIDTH$}");
    let available = usize::from(width)
        .saturating_sub(prefix.chars().count())
        .saturating_sub(trailing.chars().count());
    let used: usize = value.iter().map(Span::width).sum();
    let pad = available.saturating_sub(used);
    let mut spans = vec![Span::styled(
        prefix,
        if focused {
            palette::primary().bold()
        } else {
            palette::muted()
        },
    )];
    spans.extend(value);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(trailing, palette::muted()));
    Line::from(spans)
}

/// The repository pill's offers: what the cache knows, plus whatever the project
/// already names when the cache has not caught up.
fn repository_select(cached: &[Repository], current: Option<&str>) -> Select<Repository> {
    let mut offers: Vec<Offer<Repository>> = cached
        .iter()
        .map(|repository| Offer::new(repository.clone(), repository.name.clone()))
        .collect();
    match current {
        Some(url) => {
            if !offers.iter().any(|offer| offer.value.url == url) {
                offers.insert(
                    0,
                    Offer::new(
                        Repository {
                            name: url.to_owned(),
                            url: url.to_owned(),
                            is_private: false,
                        },
                        url,
                    ),
                );
            }
        }
        None => {
            // Not ready, so the pill draws it as the question it is: the mark
            // says it opens, the value says it has not been picked.
            let mut offer = Offer::new(
                Repository {
                    name: "repository not picked".to_owned(),
                    url: String::new(),
                    is_private: false,
                },
                "repository not picked",
            );
            offer.ready = false;
            offers.insert(0, offer);
        }
    }
    let chosen = current
        .and_then(|url| offers.iter().position(|offer| offer.value.url == url))
        .unwrap_or(0);
    Select::new("repository", offers, chosen).without_search()
}

fn row(
    marker: Marker,
    label: &str,
    value: &str,
    action: &str,
    focused: bool,
    width: u16,
) -> Line<'static> {
    let prefix = format!("{}  {label:<LABEL_WIDTH$}  ", marker.glyph());
    let trailing = format!("{action:<ACTION_WIDTH$}");
    let available = usize::from(width)
        .saturating_sub(prefix.chars().count())
        .saturating_sub(trailing.chars().count());
    let value = truncate(value, available);
    let style = if focused {
        palette::primary().bold()
    } else {
        palette::primary()
    };
    Line::from(vec![
        Span::styled(
            prefix,
            if focused {
                palette::primary().bold()
            } else {
                palette::muted()
            },
        ),
        Span::styled(format!("{value:<available$}"), style),
        Span::styled(trailing, palette::muted()),
    ])
}

/// The identity, as four rows under the one that opens it.
fn commits_editor(commits: &Commits, setup: &SetupState, width: u16) -> Vec<Line<'static>> {
    let indent = " ".repeat(LABEL_WIDTH + 7);
    let name = if commits.field == CommitsField::Name {
        commits.name.marked()
    } else {
        commits.name.value().to_owned()
    };
    let email = if commits.field == CommitsField::Email {
        commits.email.marked()
    } else {
        commits.email.value().to_owned()
    };
    let sign = if commits.signs {
        "● on".to_owned()
    } else {
        "○ off".to_owned()
    };
    let key = if setup.has_key {
        "present · enter show".to_owned()
    } else {
        "none · enter generate".to_owned()
    };
    vec![
        editor_line(
            &indent,
            "Name",
            &name,
            commits.field == CommitsField::Name,
            width,
        ),
        editor_line(
            &indent,
            "Email",
            &email,
            commits.field == CommitsField::Email,
            width,
        ),
        editor_line(
            &indent,
            "Sign commits",
            &sign,
            commits.field == CommitsField::Sign,
            width,
        ),
        editor_line(
            &indent,
            "Key",
            &key,
            commits.field == CommitsField::Key,
            width,
        ),
    ]
}

fn editor_line(indent: &str, label: &str, value: &str, focused: bool, width: u16) -> Line<'static> {
    let prefix = format!("{indent}{label:<12} ");
    let available = usize::from(width).saturating_sub(prefix.chars().count());
    let value = truncate(value, available);
    let style = if focused {
        palette::primary().bold()
    } else {
        palette::primary()
    };
    Line::from(vec![
        Span::styled(
            prefix,
            if focused {
                palette::primary().bold()
            } else {
                palette::muted()
            },
        ),
        Span::styled(format!("{value:<available$}"), style),
    ])
}

fn row_anchor(rows: Rect, index: u16) -> Rect {
    Rect {
        y: rows.y.saturating_add(index),
        height: 1,
        ..rows
    }
}

fn detail_text(focus: Focus, setup: &SetupState) -> String {
    let vcs = setup.vcs.label();
    match focus {
        Focus::Name => {
            "Projects bind together agents, code and task trackers into a single setup. You can \
             have multiple projects set up in altitude, for instance one for your day job and one \
             for your weekend side-project. Let's name your first project now."
                .to_owned()
        }
        Focus::Agent if setup.agent.is_none() => {
            "altitude spins up your usual coding agent and drives it autonomously to complete \
             work for you. You can always take back manual control and interact with the agent's \
             own interface directly when needed."
                .to_owned()
        }
        Focus::Agent => {
            format!(
                "Signing in happens through {}, in a sandbox of its own. What it writes is kept \
                 here and mounted into every task, so the agent is signed in once.",
                setup
                    .agent
                    .map(|agent| agent.to_string())
                    .unwrap_or_else(|| "the agent".to_owned())
            )
        }
        Focus::VersionControl if !signed_in(&setup.vcs_status) => {
            format!(
                "altitude runs your agents in isolated sandboxes. Pick a repository so they can \
                 pull your project's code and send back change suggestions. This needs a {vcs} \
                 sign-in first."
            )
        }
        Focus::VersionControl if setup.repository.is_none() => {
            "altitude runs your agents in isolated sandboxes. Pick a repository so they can pull \
             your project's code and send back change suggestions."
                .to_owned()
        }
        Focus::VersionControl => {
            "The repository a task starts from and pushes back to. Changing it changes where the \
             next task's work goes."
                .to_owned()
        }
        Focus::Commits if setup.commits.is_set() => {
            "Commits carry your name and email, and a signature proves they came from you. Enter \
             edits them here; the key is made once and stays in your version-control profile, so \
             every task commits the same way."
                .to_owned()
        }
        Focus::Commits => {
            "A commit needs a name and an email before a task can deliver its work. Enter edits \
             them here; signing is set up from the same place, with a key that stays in your \
             version-control profile."
                .to_owned()
        }
        Focus::Tracker => {
            "Optional: connect your task tracker to link issues directly to altitude agents and \
             sync status back automatically."
                .to_owned()
        }
        Focus::Remote => {
            "Optional: unlock better workflows, like running tasks when you're away and piloting \
             them entirely from your task tracker, by setting up an altitude remote server and \
             sharing it with your team.\n\
             If a remote is accessible on your network, it will show up here. Learn more about \
             setting up a remote for your team in Settings."
                .to_owned()
        }
        Focus::Ready | Focus::Settings => String::new(),
    }
}

/// How many rows a paragraph takes at this width, so the card can be as tall as
/// its content and no taller.
fn wrapped_height(text: &str, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    Paragraph::new(text.to_owned())
        .wrap(Wrap { trim: true })
        .line_count(width) as u16
}
