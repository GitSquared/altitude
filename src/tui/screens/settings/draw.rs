//! How the panel is drawn: the rail, the list, and the column that describes the
//! focused row.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use crate::tui::widgets::{
    chrome,
    columns::Columns,
    header,
    list::{self, Row},
    palette,
    surface::Surface,
    utils::shimmer,
};

use super::{
    form::draw_form,
    pages::{
        agent_purpose, base_lines, credential_connector, git_connector, profile_connector,
        remote_connector, section_heading, tracker_connector, tracker_summary,
    },
    panel::{Depth, Panel, Section},
    rows::{CredentialKind, RowKind, View, mark},
};

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    panel: &mut Panel,
    view: &View,
    tick: Option<usize>,
    is_discovering: bool,
) -> Rect {
    let [header_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    header::draw(frame, header_area, "SETTINGS", None, None);
    draw_columns(frame, body, panel, view, tick, is_discovering);
    if let Some(form) = &panel.form {
        chrome::dim(frame, area);
        draw_form(frame, area, form);
    }
    area
}

fn draw_columns(
    frame: &mut Frame,
    area: Rect,
    panel: &mut Panel,
    view: &View,
    tick: Option<usize>,
    is_discovering: bool,
) {
    let inner = Surface::new().opaque().draw(frame, area);
    // The rail picks a list, the list picks a row, and the detail describes it:
    // the same columns the signals screen uses, one more of them.
    const RAIL: u16 = 32;
    const LIST: u16 = 34;
    let columns = Columns::new(&[RAIL, LIST]).draw(frame, inner);
    let rail = columns[0];
    let list_area = columns[1];
    // A gutter on both sides of the description, so it does not start on the
    // rule and end on the frame.
    let detail_area = Rect {
        x: columns[2].x.saturating_add(1),
        width: columns[2].width.saturating_sub(2),
        ..columns[2]
    };

    let rail_focused = panel.depth == Depth::Rail;
    let rail_selected = Section::ALL
        .iter()
        .position(|section| *section == panel.section)
        .unwrap_or(0);
    let rail_rows = Section::ALL
        .iter()
        .map(|section| {
            let selected = *section == panel.section;
            let tone = if selected || rail_focused {
                palette::primary()
            } else {
                palette::muted()
            };
            // Fat rows, like the signal list: the name, then what the section
            // is for.
            Row::new(section.title())
                .detail(section.description())
                .tone(tone)
                .gap(true)
        })
        .collect::<Vec<_>>();
    list::draw(
        frame,
        rail,
        &rail_rows,
        0,
        Some(rail_selected),
        rail_focused.then_some(rail_selected),
        1,
    );

    let list_focused = panel.depth == Depth::List;
    let list_rows = view
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            // A discovered remote waits at the end of the list until it is
            // adopted, so it stays dim even while the list has the focus.
            let tone = if matches!(row.kind, RowKind::Discovered(_)) {
                palette::muted()
            } else if list_focused || index == panel.selected() {
                palette::primary()
            } else {
                palette::muted()
            };
            let mut line = Row::new(row.label.clone()).tone(tone);
            if let Some(detail) = &row.detail {
                line = line.detail(detail.clone());
            }
            if let Some(dot) = row.dot() {
                line = line.mark(dot.spans(tick));
            }
            line
        })
        .collect::<Vec<_>>();
    let visible = list::Cursor::visible(list_area.height, list::row_height(&list_rows));
    panel
        .cursor
        .follow(list_rows.len(), visible, tick.is_some());
    list::draw(
        frame,
        list_area,
        &list_rows,
        panel.cursor.offset(),
        Some(panel.selected()),
        list_focused.then(|| panel.selected()),
        1,
    );
    if panel.section == Section::Remotes && is_discovering {
        // A scan has no row yet, so it is said under the rows rather than in one.
        let y = (list_area.y + list::row_height(&list_rows) * list_rows.len() as u16 + 1)
            .min(list_area.bottom().saturating_sub(1));
        frame.render_widget(
            Paragraph::new(Line::from(shimmer("scanning…", tick)).centered()),
            Rect {
                y,
                height: 1,
                ..list_area
            },
        );
    }

    // The third column follows the selection rather than the focus, so it is
    // drawn at full strength whether or not the list is being used.
    let pane = detail(panel, view, tick);
    let last_row = pane
        .line_count(detail_area.width)
        .saturating_sub(usize::from(detail_area.height));
    let pane = pane.scroll((
        panel
            .detail_scroll
            .min(last_row.min(u16::MAX as usize) as u16),
        0,
    ));
    frame.render_widget(pane, detail_area);
}

/// The third column. `focused` is decided by the caller, because whether this
/// column holds the focus is not the same question as how deep the focus is: a
/// row with nothing inside it is already here.
/// The third column: what is known about the row the list has selected.
fn detail(panel: &Panel, view: &View, tick: Option<usize>) -> Paragraph<'static> {
    let heading = move |text: String| Line::styled(text, palette::heading());
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
        RowKind::Switch { setting, on } => {
            let mut status = mark(*on, false).spans(tick);
            status.push(Span::styled(
                if *on { "on" } else { "off" },
                // Both accented, only the live state bold, so on and off differ in
                // weight rather than colour.
                if *on {
                    palette::accent().bold()
                } else {
                    palette::accent()
                },
            ));
            vec![
                heading(label.clone()),
                Line::from(""),
                Line::from(status),
                Line::from(""),
                Line::from(setting.description()),
            ]
        }
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
