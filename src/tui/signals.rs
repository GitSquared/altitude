use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::Style,
    text::Line,
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{tasks::TaskSignal, tui::utils::centered_width};

pub fn draw(frame: &mut Frame, signals: &[TaskSignal], selected: usize) {
    let area = centered_width(frame.area(), 72, 65);
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(" Info signals · enter acknowledge · esc close ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [list_area, detail_area] = Layout::vertical([Constraint::Length(8), Constraint::Min(1)])
        .margin(1)
        .areas(inner);
    let items = signals
        .iter()
        .map(|signal| ListItem::new(Line::from(signal.title.clone())))
        .collect::<Vec<_>>();
    let list = List::new(items)
        .highlight_symbol("❯ ")
        .highlight_style(Style::new().cyan());
    let mut state = ListState::default();
    state.select((!signals.is_empty()).then_some(selected));
    frame.render_stateful_widget(list, list_area, &mut state);
    let detail = signals
        .get(selected)
        .map(|signal| signal.body.as_str())
        .unwrap_or("No unacknowledged Info signals.");
    frame.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: true }),
        detail_area,
    );
}
