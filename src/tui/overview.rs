use std::iter;

use ratatui::{
    Frame,
    layout::{Constraint, HorizontalAlignment, Layout, Rect},
    style::{Style, Stylize},
    text::Line,
    widgets::{Block, BorderType, Padding, Paragraph},
};

use crate::isolation::{Sandbox, SandboxStatus};

pub fn draw(frame: &mut Frame, sandboxes: &[Sandbox], selected: usize) {
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
    draw_sandboxes(frame, body, sandboxes, selected);

    frame.render_widget(
        Line::from("c create · tab select · enter shell · delete remove · q quit".dim())
            .alignment(HorizontalAlignment::Right),
        footer,
    );
}

fn draw_sandboxes(frame: &mut Frame, area: Rect, sandboxes: &[Sandbox], selected: usize) {
    if sandboxes.is_empty() {
        frame.render_widget(
            Paragraph::new("No sandboxes running".dim()).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .padding(Padding::uniform(1)),
            ),
            area,
        );
        return;
    }

    let cards = Layout::vertical(
        sandboxes
            .iter()
            .map(|_| Constraint::Length(4))
            .chain(iter::once(Constraint::Min(0))),
    )
    .spacing(1)
    .split(area);

    for (index, (sandbox, area)) in sandboxes.iter().zip(cards.iter()).enumerate() {
        let selected = index == selected;
        let status = match &sandbox.status {
            SandboxStatus::Starting => Line::from("starting").yellow(),
            SandboxStatus::Running => Line::from("running").green(),
            SandboxStatus::Stopping => Line::from("stopping").yellow(),
            SandboxStatus::Stopped => Line::from("stopped").dim(),
            SandboxStatus::Paused => Line::from("paused").yellow(),
            SandboxStatus::Failed(error) => Line::from(format!("failed · {error}")).red(),
        };
        let style = if selected {
            Style::new().white().bold()
        } else {
            Style::new().dim()
        };
        frame.render_widget(
            Paragraph::new(status).style(style).block(
                Block::bordered()
                    .title(format!(" {} ", sandbox.id))
                    .border_type(BorderType::Rounded)
                    .border_style(style)
                    .padding(Padding::horizontal(1)),
            ),
            *area,
        );
    }
}
