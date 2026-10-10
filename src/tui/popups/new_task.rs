//! Creating a task: what to do, where it runs, and what drives it.
//!
//! One question rather than three, asked with the configured defaults already in
//! place. The prompt is the question, so it gets the room: an intent is prose,
//! and it is the only thing here that is written rather than chosen. The three
//! decisions below it are named, because three values on a line say nothing
//! about which is which.
//!
//! Enter answers the question, and shift-enter is there for a second line of
//! intent. That needs the terminal to report shift-enter as a key of its own, so
//! the protocol that says so is asked for while this page is open and given back
//! with it; a terminal that does not answer leaves enter to do both.
//!
//! The agent's first answer becomes its default. On a fresh installation there
//! is nothing to default to, and guessing on someone's behalf is not Altitude's
//! to do.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::Line,
    widgets::{Block, BorderType, Clear, Padding},
};

use crate::tui::widgets::{chrome, palette, text::TextBox, utils::truncate};
use crate::{agent::AgentKind, isolation::Environment};

/// The agents on offer, in the order they are shown.
const AGENTS: [AgentKind; 2] = [AgentKind::Pi, AgentKind::Claude];

/// The page's width, as a share of the terminal. Wider than a question about one
/// value, because the answer to this one is a paragraph.
const WIDTH_PERCENT: u16 = 64;
/// Its height, which is the prompt's room plus what the choices and the actions
/// need. The prompt takes what is left of it.
const HEIGHT: u16 = 20;
/// The width of the choices' labels, so their values line up.
const LABEL_WIDTH: u16 = 14;

/// What holds the keys, top to bottom.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Task,
    Environment,
    Agent,
    Host,
    Cancel,
    Create,
}

const ORDER: [Focus; 6] = [
    Focus::Task,
    Focus::Environment,
    Focus::Agent,
    Focus::Host,
    Focus::Cancel,
    Focus::Create,
];

/// A machine a task could run on.
pub struct Host {
    /// Where it is reached, or nothing for this machine.
    pub address: Option<String>,
    pub label: String,
    /// Whether it can be given work. One that is syncing or unreachable is shown and
    /// cannot be chosen: a task given to it would not run.
    pub ready: bool,
}

/// The question, and the answers so far.
pub struct NewTask {
    task: TextBox,
    environments: Vec<Environment>,
    environment: usize,
    agent: usize,
    /// This machine first, then the remotes.
    hosts: Vec<Host>,
    host: usize,
    /// Whether the agent had never been chosen, in which case this answer is the
    /// one to keep.
    agent_unchosen: bool,
    /// Whether the question has been answered with nothing to answer yet. The
    /// page says what is missing only then, because until then it is not missing:
    /// it has not been asked for.
    refused: bool,
    focus: Focus,
}

/// What the question ended in.
pub enum Answer {
    Cancel,
    Create {
        task: String,
        environment: Environment,
        agent: AgentKind,
        /// Where to run it: a remote's address, or nothing for this machine.
        host: Option<String>,
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
        hosts: Vec<Host>,
    ) -> Self {
        let environment = environments
            .iter()
            .position(|candidate| candidate.name.eq_ignore_ascii_case(default_environment))
            .unwrap_or(0);
        let agent = default_agent
            .and_then(|chosen| AGENTS.iter().position(|candidate| *candidate == chosen))
            .unwrap_or(0);
        Self {
            task: TextBox::new(""),
            environments,
            environment,
            agent,
            hosts,
            host: 0,
            agent_unchosen: default_agent.is_none(),
            refused: false,
            focus: Focus::Task,
        }
    }

    /// Whether the intent is taking characters, in which case `?` and `q` are
    /// characters too.
    pub(in crate::tui) fn is_typing(&self) -> bool {
        self.focus == Focus::Task
    }

    /// The keys that work here, for the guidance line. They follow the focus,
    /// because a second line of intent is only a key while it can be typed.
    pub fn hints(&self) -> Vec<chrome::Hint> {
        let mut hints = vec![chrome::Hint::new("tab", "field")];
        hints.push(if self.is_typing() {
            chrome::Hint::new("shift-enter", "new line")
        } else {
            chrome::Hint::new("←/→", "change")
        });
        hints.push(chrome::Hint::new("enter", "create"));
        hints.push(chrome::Hint::new("esc", "cancel"));
        hints
    }

    pub fn paste(&mut self, text: &str) {
        if self.is_typing() {
            self.task.paste(text);
        }
    }

    /// Whether the question can be answered, and what is missing when it cannot.
    pub fn refusal(&self) -> Option<&'static str> {
        self.task
            .value()
            .trim()
            .is_empty()
            .then_some("a task needs something to do")
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Answer> {
        // Escape leaves the question from anywhere. `q` is a letter while the
        // prompt has the keys, and the answer to the question once it does not.
        if key.code == KeyCode::Esc || (key.code == KeyCode::Char('q') && !self.is_typing()) {
            return Some(Answer::Cancel);
        }
        if self.is_typing() {
            return self.prompt_key(key);
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
                _ => self.answer(),
            },
            _ => None,
        }
    }

    /// A key while the prompt has the caret. Every letter is a letter: the only
    /// keys that do something else here are the ones that cannot be typed.
    fn prompt_key(&mut self, key: KeyEvent) -> Option<Answer> {
        match key.code {
            // A second line, where the terminal can report the modifier. Alt
            // arrives as an escape prefix in every terminal, so it means the
            // same thing and is the one that works when shift does not.
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.task.handle_key(key);
                None
            }
            // Enter answers the question, because the prompt is the question.
            KeyCode::Enter => self.answer(),
            KeyCode::Tab => {
                self.move_focus(true);
                None
            }
            KeyCode::BackTab => {
                self.move_focus(false);
                None
            }
            _ => {
                self.task.handle_key(key);
                None
            }
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
            // A machine that cannot be given work is stepped over rather than shown
            // as chosen, because choosing it would mean nothing happens.
            Focus::Host if self.hosts.len() > 1 => {
                let count = self.hosts.len();
                for step in 1..=count {
                    let candidate = if forward {
                        (self.host + step) % count
                    } else {
                        (self.host + count - step) % count
                    };
                    if self.hosts[candidate].ready {
                        self.host = candidate;
                        break;
                    }
                }
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
        // A task with nothing to do is not a task, so the question is asked
        // again, and this time the page says what is missing.
        if self.refusal().is_some() {
            self.refused = true;
            self.focus = Focus::Task;
            return None;
        }
        let environment = self.environments.get(self.environment)?.clone();
        let agent = AGENTS[self.agent];
        let host = self.hosts.get(self.host)?.address.clone();
        Some(Answer::Create {
            task: self.task.value().trim().to_owned(),
            environment,
            agent,
            host,
            remember: self.agent_unchosen.then_some(agent),
        })
    }

    /// What the machine this task would run on is called, as the operator reads it.
    fn host_label(&self) -> String {
        self.hosts
            .get(self.host)
            .map_or_else(|| "this machine".to_owned(), |host| host.label.clone())
    }

    /// Whether that machine can be given work, which is what its value's colour
    /// says.
    fn host_is_ready(&self) -> bool {
        self.hosts.get(self.host).is_some_and(|host| host.ready)
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
    let popup = chrome::centered(area, WIDTH_PERCENT, HEIGHT);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" New task ")
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(2));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 8 {
        return;
    }
    let [label, prompt, choices, actions] = Layout::vertical([
        Constraint::Length(1),
        // The prompt takes what is left, because it is the part that is written.
        Constraint::Min(3),
        Constraint::Length(6),
        Constraint::Length(3),
    ])
    .areas(inner);

    frame.render_widget(Line::styled("Task", palette::muted()), label);
    new_task.task.draw(frame, prompt, new_task.is_typing());
    draw_choices(frame, choices, new_task);
    draw_actions(frame, actions, new_task);
}

/// Where it runs, and what drives it: one decision a line, each named.
fn draw_choices(frame: &mut Frame, area: Rect, new_task: &NewTask) {
    let [_, rows, _] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .areas(area);
    let [environment, agent, host] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(rows);

    choice(
        frame,
        environment,
        "Environment",
        new_task.environment_name(),
        new_task.focus == Focus::Environment,
        palette::accent(),
    );
    choice(
        frame,
        agent,
        "Agent",
        &new_task.agent_name(),
        new_task.focus == Focus::Agent,
        palette::accent(),
    );
    // A machine that cannot be given work is drawn as it is rather than hidden: the
    // reason it is not offered is worth reading.
    let unfocused = if new_task.host_is_ready() {
        palette::accent()
    } else {
        palette::attention()
    };
    choice(
        frame,
        host,
        "Runs on",
        &new_task.host_label(),
        new_task.focus == Focus::Host,
        unfocused,
    );
}

/// One decision: what it is called, and what it is set to.
fn choice(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    value: &str,
    focused: bool,
    unfocused: Style,
) {
    let [name, held] =
        Layout::horizontal([Constraint::Length(LABEL_WIDTH), Constraint::Min(0)]).areas(area);
    frame.render_widget(Line::styled(label, palette::muted()), name);
    let style = if focused { palette::focus() } else { unfocused };
    let value = truncate(&format!(" ‹ {value} › "), usize::from(held.width));
    frame.render_widget(Line::styled(value, style), held);
}

/// The actions, and what is missing when they cannot be taken.
fn draw_actions(frame: &mut Frame, area: Rect, new_task: &NewTask) {
    // A button is drawn on the second of the two rows it is given, so the reason
    // sits above them both.
    let [reason, buttons] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(2)]).areas(area);
    // Said only once it has been asked for: an empty prompt is not a mistake
    // until enter says it is.
    if new_task.refused
        && let Some(refusal) = new_task.refusal()
    {
        frame.render_widget(Line::styled(refusal, palette::attention()), reason);
    }
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
        new_task.refusal().is_none(),
    );
}
