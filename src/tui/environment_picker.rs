use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use super::palette;
use crate::{development::DevelopmentEnvironment, tui::utils::centered_width};

/// Choosing the environment a new task runs in.
///
/// Managing what is configured is the settings panel's job; this is only the
/// question a new task asks, so it is small and has one action.
pub fn draw(
    frame: &mut Frame,
    area: Rect,
    environments: &[DevelopmentEnvironment],
    default_environment: &str,
    selected: usize,
) {
    let area = centered_width(area, 72, 60);
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(" Environment ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [list_area, detail_area] = Layout::vertical([Constraint::Length(8), Constraint::Min(1)])
        .margin(1)
        .areas(inner);
    let items = environments
        .iter()
        .map(|environment| {
            let marker = if environment.name.eq_ignore_ascii_case(default_environment) {
                " (default)"
            } else {
                ""
            };
            ListItem::new(Line::from(format!("{}{marker}", environment.name)))
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .highlight_symbol("❯ ")
        .highlight_style(palette::accent());
    let mut state = ListState::default();
    state.select((!environments.is_empty()).then_some(selected));
    frame.render_stateful_widget(list, list_area, &mut state);
    let detail = environments
        .get(selected)
        .map(|environment| {
            if environment.setup_command.trim().is_empty() {
                "Nothing on top of the base.".to_owned()
            } else {
                environment.setup_command.clone()
            }
        })
        .unwrap_or_else(|| "No environments are configured. Press n to add one.".to_owned());
    frame.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: true }),
        detail_area,
    );
}
