use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::Style,
    text::Line,
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{development::DevelopmentEnvironment, tui::utils::centered_width};

/// Why the environment list is on screen.
///
/// Both purposes list the same configured environments; only the action on
/// Enter differs, so the overlay is shared and the title carries the keys.
#[derive(Clone, Copy)]
pub enum Purpose {
    /// Choose the environment a new task runs in.
    NewTask,
    /// Manage what is configured.
    Manage,
}

impl Purpose {
    fn title(self) -> &'static str {
        match self {
            Self::NewTask => " Environment · enter start · esc cancel ",
            Self::Manage => " Environments · enter set default · n add · esc close ",
        }
    }
}

pub fn draw(
    frame: &mut Frame,
    purpose: Purpose,
    environments: &[DevelopmentEnvironment],
    default_environment: &str,
    selected: usize,
) {
    let area = centered_width(frame.area(), 72, 60);
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(purpose.title());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [list_area, detail_area] = Layout::vertical([Constraint::Length(8), Constraint::Min(1)])
        .margin(1)
        .areas(inner);
    let items = environments
        .iter()
        .map(|environment| {
            let marker = if environment.name == default_environment {
                " (default)"
            } else {
                ""
            };
            ListItem::new(Line::from(format!("{}{marker}", environment.name)))
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .highlight_symbol("❯ ")
        .highlight_style(Style::new().cyan());
    let mut state = ListState::default();
    state.select((!environments.is_empty()).then_some(selected));
    frame.render_stateful_widget(list, list_area, &mut state);
    let setup_command = environments
        .get(selected)
        .map(|environment| environment.setup_command.as_str())
        .unwrap_or("No environments are configured. Press n to add one.");
    frame.render_widget(
        Paragraph::new(setup_command).wrap(Wrap { trim: true }),
        detail_area,
    );
}
