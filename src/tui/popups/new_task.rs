//! Creating a task: what to do, where it runs, and what drives it.
//!
//! One question rather than three, asked with the configured defaults already in
//! place. The two choices are switches rather than lists, because an environment
//! and an agent are one value each: there is nothing to open, only something to
//! change.
//!
//! The agent's first answer becomes its default. On a fresh installation there
//! is nothing to default to, and guessing on someone's behalf is not Altitude's
//! to do.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Block, BorderType, Clear, Padding},
};

use crate::tui::widgets::{
    chrome, keys, palette,
    text::{Field, FieldStyle},
};
use crate::{agent::AgentKind, isolation::Environment};

/// The agents on offer, in the order they are shown.
const AGENTS: [AgentKind; 2] = [AgentKind::Pi, AgentKind::Claude];

/// What holds the keys, top to bottom.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Task,
    Environment,
    Agent,
    Cancel,
    Create,
}

const ORDER: [Focus; 5] = [
    Focus::Task,
    Focus::Environment,
    Focus::Agent,
    Focus::Cancel,
    Focus::Create,
];

/// The question, and the answers so far.
pub struct NewTask {
    task: Field,
    environments: Vec<Environment>,
    environment: usize,
    agent: usize,
    /// Whether the agent had never been chosen, in which case this answer is the
    /// one to keep.
    agent_unchosen: bool,
    focus: Focus,
}

/// What the question ended in.
pub enum Answer {
    Cancel,
    Create {
        task: String,
        environment: Environment,
        agent: AgentKind,
        /// The agent to remember, when this was the first answer.
        remember: Option<AgentKind>,
    },
}

impl NewTask {
    /// Ask, with the configured defaults already chosen.
    pub fn new(
        environments: Vec<Environment>,
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
            task: Field::new(),
            environments,
            environment,
            agent,
            agent_unchosen: default_agent.is_none(),
            focus: Focus::Task,
        }
    }

    /// The keys that work here, for the guidance line.
    /// Whether the intent is taking characters, in which case `?` is one.
    pub(in crate::tui) fn is_typing(&self) -> bool {
        self.focus == Focus::Task
    }

    pub fn hints(&self) -> Vec<chrome::Hint> {
        vec![
            chrome::Hint::new("tab", "field"),
            chrome::Hint::new("←/→", "change"),
            chrome::Hint::new("enter", "create"),
            chrome::Hint::new("esc", "cancel"),
        ]
    }

    pub fn paste(&mut self, text: &str) {
        if self.focus == Focus::Task {
            self.task.paste(text);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Answer> {
        if keys::is_dismiss(key) {
            return Some(Answer::Cancel);
        }
        // A field that takes text owns the keys that would otherwise move: the
        // letters are what the operator is typing.
        if self.focus == Focus::Task {
            return match key.code {
                KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => {
                    self.move_focus(true);
                    None
                }
                KeyCode::BackTab | KeyCode::Up => {
                    self.move_focus(false);
                    None
                }
                KeyCode::Enter => {
                    self.move_focus(true);
                    None
                }
                _ => {
                    self.task.handle_key(key);
                    None
                }
            };
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => {
                self.move_focus(true);
                None
            }
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k') => {
                self.move_focus(false);
                None
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
                self.change(matches!(key.code, KeyCode::Right | KeyCode::Char('l')));
                None
            }
            KeyCode::Enter => match self.focus {
                Focus::Cancel => Some(Answer::Cancel),
                Focus::Create => self.answer(),
                _ => {
                    self.move_focus(true);
                    None
                }
            },
            _ => None,
        }
    }

    fn move_focus(&mut self, forward: bool) {
        let at = ORDER
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % ORDER.len()
        } else {
            (at + ORDER.len() - 1) % ORDER.len()
        };
        self.focus = ORDER[next];
    }

    fn change(&mut self, forward: bool) {
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

    fn answer(&mut self) -> Option<Answer> {
        // A task with nothing to do is not a task. Asking again is the whole of
        // the answer.
        if self.task.is_blank() {
            self.focus = Focus::Task;
            return None;
        }
        let environment = self.environments.get(self.environment)?.clone();
        let agent = AGENTS[self.agent];
        Some(Answer::Create {
            task: self.task.value().trim().to_owned(),
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
    let popup = chrome::centered(area, 52, 13);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" New task ")
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(2));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 6 {
        return;
    }
    let [task, choices, buttons] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(2),
        Constraint::Length(2),
    ])
    .areas(inner);

    new_task.task.draw(
        frame,
        task,
        &FieldStyle {
            focused: new_task.focus == Focus::Task,
            title: Some("Task"),
            prefix: "",
        },
    );
    choices_line(frame, choices, new_task);

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
        !new_task.task.is_blank(),
    );
}

/// Where it runs, and what drives it, on one line.
fn choices_line(frame: &mut Frame, area: Rect, new_task: &NewTask) {
    let [_, line, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);
    let [environment, separator, agent] = Layout::horizontal([
        Constraint::Length(14),
        Constraint::Length(3),
        Constraint::Min(1),
    ])
    .areas(line);
    selector(
        frame,
        environment,
        new_task.environment_name(),
        new_task.focus == Focus::Environment,
    );
    frame.render_widget(Line::styled(" / ", palette::muted()), separator);
    selector(
        frame,
        agent,
        &new_task.agent_name(),
        new_task.focus == Focus::Agent,
    );
}

/// One value, with the marks that say it can be changed.
fn selector(frame: &mut Frame, area: Rect, value: &str, focused: bool) {
    let style = if focused {
        palette::focus()
    } else {
        palette::accent()
    };
    frame.render_widget(Line::styled(format!(" ‹ {value} › "), style), area);
}
