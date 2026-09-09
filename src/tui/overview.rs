use std::iter;

use ratatui::{
    Frame,
    layout::{Constraint, HorizontalAlignment, Layout, Rect},
    style::{Style, Stylize},
    text::Line,
    widgets::{Block, BorderType, Padding, Paragraph},
};

use crate::tasks::{Task, TaskStatus};

pub fn draw(frame: &mut Frame, tasks: &[Task], selected: usize) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .margin(1)
    .spacing(1)
    .areas(frame.area());

    let [title, version] =
        Layout::horizontal([Constraint::Min(0), Constraint::Min(0)]).areas(header);
    frame.render_widget(Line::from("⛰  altitude".cyan().bold()), title);
    frame.render_widget(
        Line::from(concat!("v", env!("CARGO_PKG_VERSION")).dim()).right_aligned(),
        version,
    );
    draw_tasks(frame, body, tasks, selected);

    frame.render_widget(
        Line::from("c create · tab select · enter shell · delete remove · q quit".dim())
            .alignment(HorizontalAlignment::Right),
        footer,
    );
}

fn draw_tasks(frame: &mut Frame, area: Rect, tasks: &[Task], selected: usize) {
    if tasks.is_empty() {
        frame.render_widget(
            Paragraph::new("No tasks yet".dim()).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .padding(Padding::uniform(1)),
            ),
            area,
        );
        return;
    }

    let cards = Layout::vertical(
        tasks
            .iter()
            .map(|_| Constraint::Length(4))
            .chain(iter::once(Constraint::Min(0))),
    )
    .spacing(1)
    .split(area);

    for (index, (task, area)) in tasks.iter().zip(cards.iter()).enumerate() {
        let selected = index == selected;
        let status = match &task.status {
            TaskStatus::Pending => Line::from("pending").yellow(),
            TaskStatus::InProgress => Line::from("in progress").green(),
            TaskStatus::Completed => Line::from("completed").dim(),
            TaskStatus::Errored => Line::from(task.last_error.as_deref().map_or_else(
                || "errored".to_owned(),
                |error| format!("errored · {error}"),
            ))
            .red(),
        };
        let style = if selected {
            Style::new().white().bold()
        } else {
            Style::new().dim()
        };
        frame.render_widget(
            Paragraph::new(status).style(style).block(
                Block::bordered()
                    .title(format!(" {} ", task.command))
                    .border_type(BorderType::Rounded)
                    .border_style(style)
                    .padding(Padding::horizontal(1)),
            ),
            *area,
        );
    }
}
