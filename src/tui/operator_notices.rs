use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::Style,
    text::Line,
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{tasks::TaskOperatorNotice, tui::utils::centered_width};

pub fn draw(frame: &mut Frame, operator_notices: &[TaskOperatorNotice], selected: usize) {
    let area = centered_width(frame.area(), 72, 65);
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(" Info notices · enter acknowledge · esc close ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [list_area, detail_area] = Layout::vertical([Constraint::Length(8), Constraint::Min(1)])
        .margin(1)
        .areas(inner);
    let items = operator_notices
        .iter()
        .map(|operator_notice| ListItem::new(Line::from(operator_notice.title.clone())))
        .collect::<Vec<_>>();
    let list = List::new(items)
        .highlight_symbol("❯ ")
        .highlight_style(Style::new().cyan());
    let mut state = ListState::default();
    state.select((!operator_notices.is_empty()).then_some(selected));
    frame.render_stateful_widget(list, list_area, &mut state);
    let detail = operator_notices
        .get(selected)
        .map(|operator_notice| operator_notice.body.as_str())
        .unwrap_or("No unacknowledged Info notices.");
    frame.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: true }),
        detail_area,
    );
}
