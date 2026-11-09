use super::*;
use crate::brand;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
};

use crate::{
    tracker::TrackerKind,
    tui::{
        app::{CredentialStatus, TrackerState},
        popups::repositories::Repositories,
        widgets::{button::Button, palette, shimmer::shimmer},
    },
};

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
    let subtitle_height = wrapped_height(&subtitle_plain(), width);
    let (rows, row_focuses) = checklist(panel, setup, width, tick);
    let rows_height = rows.len() as u16;
    // The ring is the rows that were drawn, in the order they were drawn: a row
    // added to the checklist is focusable the moment it exists, and one taken
    // away cannot be focused at all.
    panel.focus_ring.clear();
    for focus in row_focuses {
        panel.focus_ring.add(focus);
    }
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

pub(super) fn subtitle() -> Text<'static> {
    Text::from(vec![
        Line::from(vec![
            Span::styled("welcome to ", palette::primary()),
            Span::styled(brand::TITLE, palette::heading()),
            Span::styled(
                ", a tool to pilot agents efficiently and keep your focus on the decisions that matter.",
                palette::primary(),
            ),
        ]),
        Line::from(""),
        Line::styled("let's get you set up:", palette::primary()),
    ])
}

pub(super) fn subtitle_plain() -> String {
    format!(
        "welcome to {}, a tool to pilot agents efficiently and keep your focus on the \
         decisions that matter.\n\nlet's get you set up:",
        brand::TITLE
    )
}

pub(super) fn checklist(
    panel: &GetStarted,
    setup: &SetupState,
    width: u16,
    tick: Option<usize>,
) -> (Vec<Line<'static>>, Vec<Focus>) {
    let mut lines = Vec::new();
    let mut focuses = Vec::new();
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
    focuses.push(Focus::Name);

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
    focuses.push(Focus::Agent);

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
        focuses.push(Focus::VersionControl);
    } else if setup.vcs_status == CredentialStatus::Checking {
        lines.push(row_spans(
            Marker::Blocking,
            "Version control",
            shimmer(setup.vcs_status.summary(), tick),
            "",
            focused == Focus::VersionControl,
            width,
        ));
        focuses.push(Focus::VersionControl);
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
        focuses.push(Focus::VersionControl);
    } else {
        lines.push(row(
            Marker::Unset,
            "Version control",
            "not chosen",
            "enter choose",
            focused == Focus::VersionControl,
            width,
        ));
        focuses.push(Focus::VersionControl);
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
    focuses.push(Focus::Commits);
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
    focuses.push(Focus::Tracker);

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
    focuses.push(Focus::Remote);

    (lines, focuses)
}

pub(super) fn draw_footer(
    frame: &mut Frame,
    area: Rect,
    panel: &mut GetStarted,
    setup: &SetupState,
) {
    panel.focus_ring.add(Focus::Settings);
    panel.focus_ring.add(Focus::Ready);
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
