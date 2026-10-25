//! What the interface has to say, in front of the work until it is read.
//!
//! A message here is not furniture: it is the answer to something the operator
//! pressed. It therefore sits where the answer is and waits to be dismissed,
//! rather than taking a line of the frame that every screen would have to keep
//! clear for it.

use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::tui::widgets::{chrome, palette};

/// How wide a message box is willing to get. A sentence is readable across a
/// third of a wide terminal and does not stretch to its edge.
const MAX_WIDTH: u16 = 64;
const MIN_WIDTH: u16 = 28;

/// Something to read, and one way to be done with it.
pub struct Alert {
    pub message: String,
    /// Whether this is something that went wrong. A problem reads in the colour
    /// that means a problem everywhere else in this interface.
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
        if self.is_problem {
            "That did not work"
        } else {
            "Notice"
        }
    }
}

/// Draw the message, and answer with the box it occupies.
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
    let height = (rows as u16 + 5).min(area.height);
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
    let [body, actions] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);
    frame.render_widget(
        Paragraph::new(alert.message.clone())
            .style(if alert.is_problem {
                palette::bad()
            } else {
                palette::primary()
            })
            .wrap(Wrap { trim: true }),
        body,
    );
    chrome::actions(
        frame,
        popup,
        actions,
        &[chrome::Action::primary("dismiss").focused(true)],
    );
    popup
}
