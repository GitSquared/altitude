//! How the panel is drawn: the rail, the list, and the column that describes the
//! focused row.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, List, ListItem, Padding, Paragraph, Wrap},
};

use crate::tui::widgets::{chrome, palette, utils::shimmer};

use super::{
    form::draw_form,
    pages::{
        agent_purpose, base_lines, credential_connector, git_connector, profile_connector,
        remote_connector, section_heading, tracker_connector, tracker_summary,
    },
    panel::{Depth, Panel, Section},
    rows::{CredentialKind, RowKind, View, mark},
};

/// The gap between columns, so two focus bars can never touch.
const GAP: u16 = 3;

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    panel: &Panel,
    view: &View,
    tick: Option<usize>,
    is_discovering: bool,
) -> Rect {
    draw_columns(frame, area, panel, view, tick, is_discovering);
    if let Some(form) = &panel.form {
        chrome::dim(frame, area);
        draw_form(frame, area, form);
    }
    area
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

fn draw_columns(
    frame: &mut Frame,
    area: Rect,
    panel: &Panel,
    view: &View,
    tick: Option<usize>,
    is_discovering: bool,
) {
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
    let mut list_items = view
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
    if panel.section == Section::Remotes && is_discovering {
        list_items.push(ListItem::new(Line::default()));
        list_items.push(ListItem::new(
            Line::from(shimmer("scanning…", tick)).centered(),
        ));
    }
    frame.render_widget(List::new(list_items), list_area);

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
