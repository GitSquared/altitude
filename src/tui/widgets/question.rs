use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::tui::widgets::{button, chrome, palette};

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    question: &str,
    answers: &[&str],
    chosen: usize,
) -> Rect {
    let [row] = Layout::horizontal([Constraint::Percentage(60)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::vertical([Constraint::Length(10)])
        .flex(Flex::Center)
        .areas(row);
    frame.render_widget(Clear, popup);
    let block = chrome::modal(title.to_owned());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [body, actions] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(inner);
    frame.render_widget(
        Paragraph::new(question.to_owned())
            .style(palette::primary())
            .wrap(Wrap { trim: true }),
        body,
    );
    button::actions(
        frame,
        popup,
        actions,
        &answers
            .iter()
            .enumerate()
            .map(|(index, answer)| {
                let action = if index == 0 {
                    button::Action::secondary(answer)
                } else {
                    button::Action::primary(answer)
                };
                action.focused(index == chosen)
            })
            .collect::<Vec<_>>(),
    );
    popup
}
