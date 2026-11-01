use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::tui::widgets::{button, chrome, palette};

const MAX_WIDTH: u16 = 64;
const MIN_WIDTH: u16 = 28;

pub struct Alert {
    pub message: String,
    pub is_problem: bool,
}

impl Alert {
    pub fn note(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_problem: false,
        }
    }

    pub fn problem(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_problem: true,
        }
    }

    fn title(&self) -> &'static str {
        if self.is_problem { "Error" } else { "Notice" }
    }
}

pub fn draw(frame: &mut Frame, area: Rect, alert: &Alert) -> Rect {
    let width = (area.width * 2 / 3)
        .clamp(MIN_WIDTH, MAX_WIDTH)
        .min(area.width);
    // The text is read inside the border and its own padding.
    let text_width = usize::from(width.saturating_sub(4)).max(1);
    let rows = alert
        .message
        .lines()
        .map(|line| line.chars().count().div_ceil(text_width).max(1))
        .sum::<usize>()
        .max(1);
    let height = chrome::modal_height(u16::try_from(rows).unwrap_or(u16::MAX)).min(area.height);
    let [row] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(row);
    frame.render_widget(Clear, popup);
    let block = chrome::modal(alert.title());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let body = chrome::body(inner);
    frame.render_widget(
        Paragraph::new(alert.message.clone())
            .style(if alert.is_problem {
                palette::bad()
            } else {
                palette::primary()
            })
            .wrap(Wrap { trim: true }),
        body.content,
    );
    button::actions(
        frame,
        popup,
        body.actions,
        &[button::Action::primary("dismiss").focused(true)],
    );
    popup
}
