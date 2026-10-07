//! Creating a task: where it runs, and what drives it.
//!
//! Both are choices rather than settings, so both are asked for here, at the
//! moment they matter, with the configured defaults already in place. Each is a
//! switch rather than a list: an environment and an agent are one value each, so
//! there is nothing to open, only something to change.
//!
//! The agent's first answer becomes its default, because on a fresh
//! installation there is nothing to default to and the alternative is guessing
//! on someone's behalf.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Block, BorderType, Clear, Padding, Paragraph},
};

use super::{chrome, keys, palette};
use crate::{agent::AgentKind, development::DevelopmentEnvironment};

/// The agents on offer, in the order they are shown.
const AGENTS: [AgentKind; 2] = [AgentKind::Pi, AgentKind::Claude];

/// Which part of the question holds the focus.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Environment,
    Agent,
    Cancel,
    Create,
}

const FOCUS_ORDER: [Focus; 4] = [
    Focus::Environment,
    Focus::Agent,
    Focus::Cancel,
    Focus::Create,
];

/// The question, and the answers so far.
pub struct NewTask {
    /// The prompt that started this, held until the task is created.
    pub command: String,
    environments: Vec<DevelopmentEnvironment>,
    environment: usize,
    agent: usize,
    /// Whether the agent had never been chosen, in which case this answer is
    /// the one to keep.
    agent_unchosen: bool,
    focus: Focus,
}

/// What the question ended in.
pub enum Answer {
    Cancel,
    Create {
        environment: DevelopmentEnvironment,
        agent: AgentKind,
        /// The agent to remember, when this was the first answer.
        remember: Option<AgentKind>,
    },
}

impl NewTask {
    /// Ask, with the configured defaults already chosen.
    pub fn new(
        command: String,
        environments: Vec<DevelopmentEnvironment>,
        default_environment: &str,
        default_agent: Option<AgentKind>,
    ) -> Self {
        let environment = environments
            .iter()
            .position(|candidate| candidate.name.eq_ignore_ascii_case(default_environment))
            .unwrap_or(0);
        let agent = default_agent
            .and_then(|chosen| AGENTS.iter().position(|candidate| *candidate == chosen))
            .unwrap_or(0);
        Self {
            command,
            environments,
            environment,
            agent,
            agent_unchosen: default_agent.is_none(),
            focus: Focus::Environment,
        }
    }

    /// The keys that work here, for the guidance line.
    pub fn hints(&self) -> Vec<chrome::Hint> {
        vec![
            chrome::Hint::new("tab", "field"),
            chrome::Hint::new("←/→", "change"),
            chrome::Hint::new("enter", "create"),
            chrome::Hint::new("esc", "cancel"),
        ]
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Answer> {
        if keys::is_dismiss(key) {
            return Some(Answer::Cancel);
        }
        match key.code {
            KeyCode::Tab => {
                self.move_focus(true);
                None
            }
            KeyCode::BackTab => {
                self.move_focus(false);
                None
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                self.change(key.code);
                None
            }
            KeyCode::Enter => match self.focus {
                Focus::Cancel => Some(Answer::Cancel),
                Focus::Create => self.answer(),
                // Enter on a field moves on rather than doing nothing, so the
                // whole question can be answered without leaving the keyboard.
                Focus::Environment | Focus::Agent => {
                    self.move_focus(true);
                    None
                }
            },
            _ => None,
        }
    }

    fn move_focus(&mut self, forward: bool) {
        let at = FOCUS_ORDER
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % FOCUS_ORDER.len()
        } else {
            (at + FOCUS_ORDER.len() - 1) % FOCUS_ORDER.len()
        };
        self.focus = FOCUS_ORDER[next];
    }

    fn change(&mut self, code: KeyCode) {
        let forward = !matches!(code, KeyCode::Left);
        match self.focus {
            Focus::Environment if !self.environments.is_empty() => {
                let count = self.environments.len();
                self.environment = if forward {
                    (self.environment + 1) % count
                } else {
                    (self.environment + count - 1) % count
                };
            }
            Focus::Agent => {
                self.agent = if forward {
                    (self.agent + 1) % AGENTS.len()
                } else {
                    (self.agent + AGENTS.len() - 1) % AGENTS.len()
                };
            }
            _ => {}
        }
    }

    fn answer(&self) -> Option<Answer> {
        let environment = self.environments.get(self.environment)?.clone();
        let agent = AGENTS[self.agent];
        Some(Answer::Create {
            environment,
            agent,
            remember: self.agent_unchosen.then_some(agent),
        })
    }

    fn environment_name(&self) -> &str {
        self.environments
            .get(self.environment)
            .map_or("", |environment| environment.name.as_str())
    }

    fn agent_name(&self) -> String {
        AGENTS[self.agent].to_string()
    }
}

pub fn draw(frame: &mut Frame, area: Rect, new_task: &NewTask) {
    let popup = chrome::centered(area, 46, 12);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" New task ")
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(2));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [fields, buttons] =
        Layout::vertical([Constraint::Min(6), Constraint::Length(2)]).areas(inner);
    let [environment, agent] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(fields);

    field(
        frame,
        environment,
        "Environment",
        new_task.environment_name(),
        new_task.focus == Focus::Environment,
    );
    field(
        frame,
        agent,
        "Coding agent",
        &new_task.agent_name(),
        new_task.focus == Focus::Agent,
    );

    let [_, cancel, _, create] = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(10),
        Constraint::Length(3),
        Constraint::Length(10),
    ])
    .areas(buttons);
    chrome::button(
        frame,
        cancel,
        "cancel",
        new_task.focus == Focus::Cancel,
        false,
    );
    chrome::button(
        frame,
        create,
        "create",
        new_task.focus == Focus::Create,
        true,
    );
}

/// A field that holds one value, with the arrows that change it.
fn field(frame: &mut Frame, area: Rect, label: &str, value: &str, focused: bool) {
    let [label_area, value_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    frame.render_widget(Line::styled(label, palette::muted()), label_area);
    let style = if focused {
        palette::focus()
    } else {
        palette::accent()
    };
    frame.render_widget(
        Paragraph::new(Line::styled(format!(" ‹ {value} › "), style)),
        value_area,
    );
}
