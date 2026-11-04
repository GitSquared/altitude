use super::*;


use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    text::{Line, Span},
    widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::tui::widgets::{
    button, chrome, empty,
    format::truncate,
    palette,
    pointer::Targets,
    shimmer::shimmer,
};
use crate::tracker::IssueSummary;

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    new_task: &NewTask,
    tick: Option<usize>,
    targets: &mut Targets<Hit>,
) -> Rect {
    let popup = chrome::centered_at_most(area, WIDTH_PERCENT, MAX_WIDTH, HEIGHT);
    frame.render_widget(Clear, popup);
    let block = chrome::modal(if new_task.issue {
        "New task — link an issue"
    } else {
        "New task"
    });
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    targets.clear();
    if inner.height < 10 {
        return popup;
    }
    // The pill sits right under the prompt, on the right like a toolbar, and the
    // answers close the window under a rule: the prompt is the question, what the
    // project has already answered qualifies it, and the two answers end it.
    let body = chrome::body(inner);
    let [label, prompt, choice, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(body.content);
    frame.render_widget(
        Line::styled(
            if new_task.issue { "Issue" } else { "Prompt" },
            palette::muted(),
        ),
        label,
    );

    let pill = draw_choices(frame, choice, new_task);
    targets.add(pill, Hit::Pill);

    // A row below the pill rather than against it, and right-aligned with it: a
    // caption to the answer above, not another thing to choose. It takes the
    // modal's note row, which is the last one before the rule, so the row of air
    // is the pill's side of the caption rather than the answers'.
    let (machine_label, reachable) = new_task.machine();
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!("runs on {machine_label}"),
            if reachable {
                palette::muted()
            } else {
                palette::attention()
            },
        ))
        .alignment(Alignment::Right),
        body.note,
    );

    let prompt_area = if new_task.issue {
        let [query, results] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(prompt);
        new_task.task.draw(frame, query, new_task.is_typing());
        draw_issue(frame, results, new_task, tick);
        query
    } else {
        new_task.task.draw(frame, prompt, new_task.is_typing());
        prompt
    };
    targets.add(prompt_area, Hit::Prompt);

    // Said only once it has been asked for: an empty prompt is not a mistake
    // until an answer says it is. It goes beside the pill, which leaves the left
    // of its row free by sitting on the right.
    if new_task.refused
        && let Some(refusal) = new_task.refusal()
    {
        frame.render_widget(
            Line::styled(refusal, palette::attention()),
            Rect {
                x: inner.x,
                y: choice.y,
                width: inner.width,
                height: 1,
            },
        );
    }

    let buttons = button::actions(
        frame,
        popup,
        body.actions,
        &[
            button::Action::secondary("cancel").focused(new_task.focus == Focus::Cancel),
            button::Action::primary(if new_task.issue { "link" } else { "create" })
                .focused(new_task.focus == Focus::Create),
        ],
    );
    if let [cancel, create] = buttons[..] {
        targets.add(cancel, Hit::Cancel);
        targets.add(create, Hit::Create);
    }

    for (row, offer) in new_task.draw_open_list(frame, prompt, pill) {
        targets.add(row, Hit::Offer(offer));
    }
    popup
}

pub fn draw_issue(frame: &mut Frame, area: Rect, new_task: &NewTask, tick: Option<usize>) {
    let note = new_task
        .read_error
        .as_deref()
        .map(|error| Line::styled(error.to_owned(), palette::bad()))
        .or_else(|| {
            new_task
                .queued_submit
                .then(|| Line::styled("linking the first result…", palette::attention()))
        });
    let [note_area, body] = Layout::vertical([
        Constraint::Length(u16::from(note.is_some())),
        Constraint::Min(0),
    ])
    .areas(area);
    if let Some(note) = note {
        frame.render_widget(Paragraph::new(note).wrap(Wrap { trim: true }), note_area);
    }
    if new_task.submission.is_some() {
        let label = "reading issue…";
        frame.render_widget(Paragraph::new(Line::from_iter(shimmer(label, tick))), body);
        return;
    }
    match &new_task.phase {
        IssuePhase::Idle => frame.render_widget(
            Paragraph::new(Line::styled(
                "Type an issue ID, paste a Linear URL, or search terms.",
                palette::muted(),
            ))
            .wrap(Wrap { trim: true }),
            body,
        ),
        IssuePhase::Searching => {
            let label = "searching Linear…";
            frame.render_widget(Paragraph::new(Line::from_iter(shimmer(label, tick))), body);
        }
        IssuePhase::Failed(message) => frame.render_widget(
            Paragraph::new(Line::styled(message.clone(), palette::bad())).wrap(Wrap { trim: true }),
            body,
        ),
        IssuePhase::Ready if new_task.results.is_empty() => {
            empty::draw(frame, body, "No matching issues")
        }
        IssuePhase::Ready => {
            let width = usize::from(body.width);
            let items: Vec<ListItem> = new_task
                .results
                .iter()
                .enumerate()
                .map(|(index, summary)| {
                    let focused = index == new_task.selected_result;
                    ListItem::new(result_line(summary, width, focused))
                })
                .collect();
            let mut state = ListState::default();
            state.select(Some(new_task.selected_result));
            frame.render_stateful_widget(List::new(items), body, &mut state);
        }
    }
}

pub fn result_line(summary: &IssueSummary, width: usize, focused: bool) -> Line<'static> {
    let text = match &summary.project {
        Some(project) => format!(
            "{}  {}  · {project}",
            summary.reference.identifier, summary.title
        ),
        None => format!("{}  {}", summary.reference.identifier, summary.title),
    };
    if focused {
        return Line::styled(truncate(&text, width), palette::focus());
    }
    let mut spans = vec![
        Span::styled(summary.reference.identifier.clone(), palette::accent()),
        Span::raw(format!("  {}", summary.title)),
    ];
    if let Some(project) = &summary.project {
        spans.push(Span::styled(format!("  · {project}"), palette::muted()));
    }
    Line::from(spans)
}

pub fn draw_choices(frame: &mut Frame, area: Rect, new_task: &NewTask) -> Rect {
    let named = new_task.project.pill_width(true);
    let valued = new_task.project.pill_width(false);
    // A pill is its value first: a row too narrow for the name keeps what it is
    // set to, and the open list says which decision it belongs to.
    let (width, with_name) = if named <= area.width {
        (named, true)
    } else if valued <= area.width {
        (valued, false)
    } else {
        (area.width, false)
    };
    let [project] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::End)
        .areas(area);
    new_task
        .project
        .draw_pill(frame, project, new_task.focus == Focus::Project, with_name);
    project
}

pub fn looks_like_issue(value: &str) -> bool {
    if value.starts_with("https://linear.app/") && value.contains("/issue/") {
        return true;
    }
    let Some((key, number)) = value.rsplit_once('-') else {
        return false;
    };
    !key.is_empty()
        && key.len() <= 10
        && key.chars().all(|character| character.is_ascii_uppercase())
        && !number.is_empty()
        && number.chars().all(|character| character.is_ascii_digit())
}
