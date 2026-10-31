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
    empty, header,
    list::{self, Row},
    palette,
    pointer::Targets,
    surface::Surface,
    utils::shimmer,
};

use crate::remote::LOCAL_VERSION;

use super::{
    form::draw_form,
    pages::{
        credential_connector, git_connector, project_lines, remote_connector, section_heading,
        tracker_summary,
    },
    panel::{Depth, Panel, Section},
    rows::{RowKind, TrackerBinding, View, mark, tracker_is_set_up},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsHit {
    Section(usize),
    Row(usize),
}

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    panel: &mut Panel,
    view: &View,
    tick: Option<usize>,
    is_discovering: bool,
    targets: &mut Targets<SettingsHit>,
) -> Rect {
    targets.clear();
    let [header_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    header::draw(frame, header_area, "SETTINGS", None, None);
    draw_columns(frame, body, panel, view, tick, is_discovering, targets);
    if let Some(form) = &mut panel.form {
        chrome::dim(frame, area);
        draw_form(frame, area, form, tick);
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
    targets: &mut Targets<SettingsHit>,
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
    // divider and end on the frame.
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
        })
        .collect::<Vec<_>>();
    for (rect, index) in list::draw(
        frame,
        rail,
        &rail_rows,
        0,
        Some(rail_selected),
        rail_focused.then_some(rail_selected),
        1,
    ) {
        targets.add(rect, SettingsHit::Section(index));
    }

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
    let visible = list::content_lines(&list_rows);
    panel
        .cursor
        .follow(&list_rows, list_area.height, tick.is_some());
    for (rect, index) in list::draw(
        frame,
        list_area,
        &list_rows,
        panel.cursor.offset(),
        Some(panel.selected()),
        list_focused.then(|| panel.selected()),
        1,
    ) {
        targets.add(rect, SettingsHit::Row(index));
    }
    if panel.section == Section::Remotes && is_discovering {
        // A scan has no row yet, so it is said under the rows rather than in one.
        let below = visible.saturating_sub(panel.cursor.offset());
        let y = list_area
            .y
            .saturating_add(u16::try_from(below + 1).unwrap_or(u16::MAX))
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

    // An empty list has nothing to describe, so the description column says how
    // to put something in it instead of sitting blank.
    if view.rows.is_empty() {
        draw_empty_detail(frame, detail_area, panel.section);
        return;
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

fn draw_empty_detail(frame: &mut Frame, area: Rect, section: Section) {
    let message = match section {
        Section::Agents | Section::VersionControl | Section::Trackers => {
            "No credentials found, press n to connect one"
        }
        Section::Projects => "No projects yet, press n to add one",
        Section::Remotes => "No remotes yet, press n to add one",
        Section::Interface => return,
    };
    empty::draw(frame, area, message);
}

/// The tracker work this machine gives a remote, as settings rather than prose.
///
/// One label column so the page reads down instead of across, and every value in
/// the accent: these are the names the operator came to the page to read.
fn tracker_setting_lines(binding: &TrackerBinding) -> Vec<Line<'static>> {
    const COLUMN: usize = 10;
    let setting = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{label:<COLUMN$}"), palette::muted()),
            Span::styled(value, palette::accent()),
        ])
    };
    let mut lines = Vec::new();
    if let Some(workspace) = &binding.workspace {
        lines.push(setting("workspace", workspace.clone()));
    }
    if !binding.identities.is_empty() {
        lines.push(setting("identity", binding.identities.join(", ")));
    }
    // The label is repeated rather than left blank on the lines under it: the
    // paragraph trims leading whitespace, so an aligned column of blanks would
    // not survive the wrap.
    for (team, project) in &binding.teams {
        lines.push(setting("team", format!("{team} → {project}")));
    }
    lines
}

fn detail(panel: &Panel, view: &View, tick: Option<usize>) -> Paragraph<'static> {
    let heading = move |text: String| Line::styled(text, palette::heading());
    let Some(row) = panel.selected_row(view) else {
        return Paragraph::new(Span::styled("", palette::muted()));
    };
    let label = row.label.clone();
    let lines = match &row.kind {
        RowKind::Project { project } => project_lines(project, view, heading),
        RowKind::GitIdentity(identity) => git_connector(identity).lines(heading, tick),
        RowKind::Credential(credential) => credential_connector(credential).lines(heading, tick),
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
            binding,
        } => {
            let mut lines =
                remote_connector(label, address, remembered.as_ref(), status, *needs_config)
                    .lines(heading, tick);
            lines.push(Line::from(""));
            lines.push(section_heading("Task tracker"));
            lines.push(tracker_summary(trackers.as_deref(), tick));
            // Everything this machine gives the remote's tracker work, named: the
            // remote holds the app, this side holds the login and the teams. The
            // values are the accent because they are what is being pointed at;
            // nothing here is a choice of this client's, since the app belongs to
            // the machine that runs it.
            if let Some(binding) = binding {
                lines.extend(tracker_setting_lines(binding));
            }
            if !tracker_is_set_up(trackers.as_deref()) {
                lines.push(Line::styled(
                    "Press t for the set up guide.",
                    palette::accent(),
                ));
            }
            lines
        }
        RowKind::Discovered(discovered) => {
            let mut lines = vec![
                heading(label),
                Line::from(""),
                Line::from(format!(
                    "We detected this remote running altitude v{} on your network.",
                    discovered.version
                )),
            ];
            if discovered.compatibility.is_compatible() {
                lines.push(Line::from(""));
                lines.push(Line::styled(
                    "Press enter to start using it",
                    palette::accent(),
                ));
            } else {
                // A mismatch is a known, stable fact rather than a remote that
                // failed: say which two releases disagree, and offer the only
                // action there is rather than a connection that cannot be made.
                lines.push(Line::from(""));
                lines.push(Line::from(format!(
                    "This machine runs altitude v{}.",
                    LOCAL_VERSION
                )));
                lines.push(Line::from(""));
                lines.push(Line::styled(
                    "Update both machines to the same version to use it",
                    palette::accent(),
                ));
            }
            lines
        }
    };
    Paragraph::new(lines).wrap(Wrap { trim: true })
}
