//! Creating a task: what to do, where it runs, and what drives it.
//!
//! One question rather than three, asked with the configured defaults already in
//! place. The prompt is the question, so it gets the room: an intent is prose,
//! and it is the only thing here that is written rather than chosen. The three
//! decisions below it are named, because three values on a line say nothing
//! about which is which.
//!
//! The same page can also bind a Linear issue instead of writing an intent. The
//! prompt becomes the search box, the results take the room the prompt had, and
//! the three decisions stay exactly where they were. `ctrl-k` moves between the
//! two, so a free prompt is never a mode you have to remember how to leave.
//!
//! Searching is a network call, so this page never makes one. It says when a
//! query is due and what came back, and the loop in `run` does the waiting.
//! Every answer carries the popup and the query it was asked for, so a reply
//! that arrives after the query changed, or after this page closed, is dropped
//! rather than shown under the wrong question.
//!
//! The three decisions are pills rather than rows: what is chosen is the thing
//! you press, and enter opens it into a list you can narrow by typing. They sit
//! under the prompt, so the list opens up over what has been written rather than
//! down over the actions.
//!
//! Enter is a line in the prompt, because an intent is prose. Answering is
//! ctrl-enter, so the keys that start a task are never the keys that write one.
//! It is also the one key here that a terminal has to be told to report as a key
//! of its own, which is why the page asks for that protocol while it is open.
//!
//! The agent's first answer becomes its default. On a fresh installation there
//! is nothing to default to, and guessing on someone's behalf is not Altitude's
//! to do.

use std::{
    mem,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::tui::widgets::{
    chrome, palette,
    select::{Offer, Select},
    text::TextBox,
    utils::{shimmer, truncate},
};
use crate::{
    agent::AgentKind,
    isolation::Environment,
    tracker::{IssueReference, IssueSummary},
};

/// The agents on offer, in the order they are shown. One catalog, so the
/// creation question and the settings list cannot disagree about them.
pub(in crate::tui) const AGENTS: [AgentKind; 2] = [AgentKind::Pi, AgentKind::Claude];

/// The page's width, as a share of the terminal. Wider than a question about one
/// value, because the answer to this one is a paragraph.
const WIDTH_PERCENT: u16 = 70;
/// Its height, which is the prompt's room plus what the pills and the actions
/// need. The prompt takes what is left of it.
const HEIGHT: u16 = 18;
/// Between two pills, so they read as three things rather than one strip.
const PILL_GAP: u16 = 2;
/// How long a query rests before it is searched, so a word typed a letter at a
/// time is one search rather than ten.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Which page this is, so a reply meant for another one is never shown here.
static NEXT_POPUP: AtomicU64 = AtomicU64::new(1);
/// Which read is which, so a snapshot that arrives after the question was
/// dismissed cannot still create a task.
static NEXT_SUBMISSION: AtomicU64 = AtomicU64::new(1);

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

/// What a search reply is about: this popup, and this version of its query.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SearchToken {
    popup: u64,
    generation: u64,
}

/// Where the issue page is.
enum IssuePhase {
    /// The query is empty.
    Idle,
    /// A query is waiting out its debounce or out on the wire.
    Searching,
    /// An answer came back, and the results (possibly none) are shown.
    Ready,
    /// The search could not be answered, and the message says why.
    Failed(String),
}

/// A snapshot being read before the task is founded, so the page can be reset
/// without losing what the operator chose.
pub(in crate::tui) struct Submission {
    pub(in crate::tui) token: u64,
    reference: IssueReference,
    pub(in crate::tui) environment: Environment,
    pub(in crate::tui) agent: AgentKind,
    pub(in crate::tui) host: Option<String>,
    pub(in crate::tui) remember: Option<AgentKind>,
}

/// The question, and the answers so far.
pub struct NewTask {
    task: TextBox,
    environment: Select<Environment>,
    agent: Select<AgentKind>,
    /// This machine first, then the remotes, by where each is reached.
    host: Select<Option<String>>,
    /// Whether the agent had never been chosen, in which case this answer is the
    /// one to keep.
    agent_unchosen: bool,
    /// Whether the question has been answered with nothing to answer yet. The
    /// page says what is missing only then, because until then it is not missing:
    /// it has not been asked for.
    refused: bool,
    focus: Focus,

    /// Which page this is, so a reply for another one is discarded.
    popup_id: u64,
    /// Bumped whenever the query changes, so a reply about the old query is not
    /// shown under the new one.
    generation: u64,
    /// Whether the prompt is a search box.
    issue: bool,
    phase: IssuePhase,
    results: Vec<IssueSummary>,
    selected_result: usize,
    /// When the query last changed, which the debounce is measured from.
    query_due: Option<Instant>,
    search_in_flight: bool,
    /// The read that is standing between the operator and a created task.
    submission: Option<Submission>,
    /// A submission asked for while the search was still out, waiting for the
    /// first result.
    queued_submit: bool,
    /// A snapshot read that failed, kept so the operator can try again.
    read_error: Option<String>,
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
    /// Read this issue's snapshot before founding the task. The token says which
    /// submission the snapshot belongs to.
    ReadIssue {
        token: u64,
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
        let chosen = environments
            .iter()
            .position(|candidate| candidate.name.eq_ignore_ascii_case(default_environment))
            .unwrap_or(0);
        let environment = Select::new(
            "Environment",
            environments
                .into_iter()
                .map(|environment| {
                    let label = environment.name.clone();
                    Offer::new(environment, label)
                })
                .collect(),
            chosen,
        );
        let agent = Select::new(
            "Agent",
            AGENTS
                .iter()
                .copied()
                .map(|agent| Offer::new(agent, agent.to_string()))
                .collect(),
            default_agent
                .and_then(|chosen| AGENTS.iter().position(|candidate| *candidate == chosen))
                .unwrap_or(0),
        );
        Self {
            task: TextBox::new(""),
            environment,
            agent,
            host: Select::new("Runs on", host_offers(hosts), 0),
            agent_unchosen: default_agent.is_none(),
            refused: false,
            focus: Focus::Task,
            popup_id: NEXT_POPUP.fetch_add(1, Ordering::Relaxed),
            generation: 0,
            issue: false,
            phase: IssuePhase::Idle,
            results: Vec::new(),
            selected_result: 0,
            query_due: None,
            search_in_flight: false,
            submission: None,
            queued_submit: false,
            read_error: None,
        }
    }

    /// Put the machines back in step with what the remotes have answered, keeping
    /// the one the operator chose where it is still offered.
    pub(in crate::tui) fn refresh_hosts(&mut self, hosts: Vec<Host>) {
        let chosen = self.host.chosen_value().cloned().flatten();
        self.host.set_offers(host_offers(hosts));
        // A machine that stopped being ready is not kept chosen: the question
        // would otherwise be answered into a delegation that cannot run.
        let kept = chosen.is_some_and(|address| {
            self.host.choose_offer(|offer| {
                offer.ready && offer.value.as_deref() == Some(address.as_str())
            })
        });
        if !kept {
            self.host.choose(0);
        }
    }

    /// Whether something here is taking characters, in which case `?` and `q`
    /// are characters too.
    pub(in crate::tui) fn is_typing(&self) -> bool {
        self.focus == Focus::Task || self.list_open()
    }

    /// Whether the focused pill has its list open.
    fn list_open(&self) -> bool {
        match self.focus {
            Focus::Environment => self.environment.is_open(),
            Focus::Agent => self.agent.is_open(),
            Focus::Host => self.host.is_open(),
            _ => false,
        }
    }

    /// Whether that list takes a query, rather than only the keys that move
    /// through it.
    fn list_searching(&self) -> bool {
        match self.focus {
            Focus::Environment => self.environment.is_searching(),
            Focus::Agent => self.agent.is_searching(),
            Focus::Host => self.host.is_searching(),
            _ => false,
        }
    }

    /// Whether a search or a snapshot read is still out, which is what keeps the
    /// wait visible.
    pub(in crate::tui) fn is_awaiting(&self) -> bool {
        self.issue && (self.submission.is_some() || matches!(self.phase, IssuePhase::Searching))
    }

    /// The keys that work here, for the guidance line. They follow the focus,
    /// because enter is only a new line while the prompt has the caret.
    pub fn hints(&self) -> Vec<chrome::Hint> {
        // An open list explains itself, because its keys are not the page's.
        if self.list_open() {
            let mut hints = Vec::new();
            if self.list_searching() {
                hints.push(chrome::Hint::new("type", "narrow"));
            }
            hints.push(chrome::Hint::new(
                if self.list_searching() {
                    "↑/↓"
                } else {
                    "j/k · ↑/↓"
                },
                "choose",
            ));
            hints.push(chrome::Hint::new("enter", "take it"));
            hints.push(chrome::Hint::new("esc", "close"));
            return hints;
        }
        let mut hints = vec![chrome::Hint::new("tab", "field")];
        // Enter does what the thing holding the focus does, so the guidance says
        // that rather than "activate". In the prompt it needs no saying: it is a
        // line, and that is what the caret does.
        match self.focus {
            Focus::Task => {}
            Focus::Cancel => hints.push(chrome::Hint::new("enter", "cancel")),
            Focus::Create => hints.push(chrome::Hint::new("enter", "create")),
            _ => hints.push(chrome::Hint::new("enter", "open")),
        }
        hints.push(chrome::Hint::new("ctrl-k", "issue / prompt"));
        if self.issue {
            hints.push(chrome::Hint::new("↑/↓", "result"));
        }
        hints.push(chrome::Hint::new(
            "ctrl-enter",
            if self.issue { "link" } else { "create" },
        ));
        hints.push(chrome::Hint::new("esc", "cancel"));
        hints
    }

    pub fn paste(&mut self, text: &str) {
        // A pasted word narrows the open list rather than landing behind it.
        if self.list_open() {
            self.paste_into_list(text);
            return;
        }
        if !self.is_typing() {
            return;
        }
        let before = self.task.value();
        self.task.paste(text);
        if self.task.value() != before {
            self.after_edit();
        }
    }

    fn paste_into_list(&mut self, text: &str) {
        match self.focus {
            Focus::Environment => self.environment.paste(text),
            Focus::Agent => self.agent.paste(text),
            Focus::Host => self.host.paste(text),
            _ => {}
        }
    }

    /// Whether the question can be answered, and what is missing when it cannot.
    pub fn refusal(&self) -> Option<&'static str> {
        // A read under way, or a submission waiting for the first result, is not
        // a second answer.
        if self.submission.is_some() || self.queued_submit {
            return None;
        }
        if self.issue {
            return match self.phase {
                IssuePhase::Ready if !self.results.is_empty() => None,
                IssuePhase::Ready => Some("no issue to link"),
                IssuePhase::Failed(_) => Some("issue search failed"),
                IssuePhase::Idle | IssuePhase::Searching => Some("search for an issue to link"),
            };
        }
        self.task
            .value()
            .trim()
            .is_empty()
            .then_some("a task needs something to do")
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Answer> {
        // An open list holds every key, escape included: escape closes the list
        // rather than the page, which is what a dropdown does everywhere else.
        if self.list_key(key) {
            return None;
        }
        // Escape leaves the question from anywhere. `q` is a letter while the
        // prompt has the keys, and the answer to the question once it does not.
        // A read that has not created anything yet is cancelled with it.
        if key.code == KeyCode::Esc || (key.code == KeyCode::Char('q') && !self.is_typing()) {
            self.submission = None;
            self.queued_submit = false;
            return Some(Answer::Cancel);
        }
        // The mode toggle works whether or not the prompt has the caret, because
        // it is about what the caret is for.
        if key.code == KeyCode::Char('k')
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
        {
            self.toggle_mode();
            return None;
        }
        // The answer is a chord, so it is never a key that writes the intent.
        if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.answer();
        }
        if self.is_typing() {
            return self.prompt_key(key);
        }
        match key.code {
            KeyCode::Tab
            | KeyCode::Down
            | KeyCode::Char('j')
            | KeyCode::Right
            | KeyCode::Char('l') => {
                self.move_focus(true);
                None
            }
            KeyCode::BackTab
            | KeyCode::Up
            | KeyCode::Char('k')
            | KeyCode::Left
            | KeyCode::Char('h') => {
                self.move_focus(false);
                None
            }
            KeyCode::Enter => match self.focus {
                Focus::Cancel => Some(Answer::Cancel),
                Focus::Create => self.answer(),
                // A pill opens rather than choosing: the choice is the list's.
                _ => {
                    self.open_list();
                    None
                }
            },
            _ => None,
        }
    }

    /// Give the key to the pill whose list is open, answering whether one was.
    fn list_key(&mut self, key: KeyEvent) -> bool {
        match self.focus {
            Focus::Environment => self.environment.handle_key(key),
            Focus::Agent => self.agent.handle_key(key),
            Focus::Host => self.host.handle_key(key),
            _ => false,
        }
    }

    fn open_list(&mut self) {
        match self.focus {
            Focus::Environment => self.environment.open(),
            Focus::Agent => self.agent.open(),
            Focus::Host => self.host.open(),
            _ => {}
        }
    }

    /// A key while the prompt has the caret. Every letter is a letter: the only
    /// keys that do something else here are the ones that cannot be typed.
    fn prompt_key(&mut self, key: KeyEvent) -> Option<Answer> {
        match key.code {
            // The issue page's prompt is one line of query, so enter is not a
            // second line there: it does nothing rather than writing a query
            // Linear cannot search for. Linking is the answer chord, as creating
            // is on the prompt.
            KeyCode::Enter if self.issue => None,
            KeyCode::Tab => {
                self.move_focus(true);
                None
            }
            KeyCode::BackTab => {
                self.move_focus(false);
                None
            }
            // In the issue page the prompt is one line of query, so up and down
            // belong to the results rather than to the caret.
            KeyCode::Up if self.issue => {
                self.select_result(false);
                None
            }
            KeyCode::Down if self.issue => {
                self.select_result(true);
                None
            }
            _ => {
                let before = self.task.value();
                self.task.handle_key(key);
                if self.task.value() != before {
                    self.after_edit();
                }
                None
            }
        }
    }

    /// What changed after a keystroke or a paste: a query to search, or a prompt
    /// that has just become an issue on its own.
    fn after_edit(&mut self) {
        if !self.issue && looks_like_issue(self.task.value().trim()) {
            self.activate_issue();
        } else if self.issue {
            self.query_changed();
        }
    }

    fn toggle_mode(&mut self) {
        if self.issue {
            self.leave_issue();
        } else {
            self.activate_issue();
        }
    }

    fn activate_issue(&mut self) {
        if self.issue {
            return;
        }
        self.issue = true;
        self.read_error = None;
        self.query_changed();
    }

    fn leave_issue(&mut self) {
        if !self.issue {
            return;
        }
        self.issue = false;
        self.results.clear();
        self.selected_result = 0;
        self.read_error = None;
        self.submission = None;
        self.queued_submit = false;
        self.generation = self.generation.wrapping_add(1);
        self.query_due = None;
        self.search_in_flight = false;
        self.phase = IssuePhase::Idle;
    }

    /// A new query invalidates every answer that is out about the old one,
    /// including a snapshot read the operator has already asked for: an edit
    /// means the issue they meant to link is not the one in flight, so it is
    /// dropped rather than founded when it lands.
    fn query_changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.search_in_flight = false;
        self.results.clear();
        self.selected_result = 0;
        self.read_error = None;
        self.submission = None;
        self.queued_submit = false;
        let has_query = !self.task.value().trim().is_empty();
        self.phase = if has_query {
            IssuePhase::Searching
        } else {
            IssuePhase::Idle
        };
        self.query_due = has_query.then(Instant::now);
    }

    /// The search the page owes, once its query has rested long enough.
    pub(in crate::tui) fn due_search(&mut self, now: Instant) -> Option<(SearchToken, String)> {
        if !self.issue || self.search_in_flight {
            return None;
        }
        let due = self.query_due?;
        if now.duration_since(due) < SEARCH_DEBOUNCE {
            return None;
        }
        let query = self.task.value().trim().to_owned();
        self.query_due = None;
        if query.is_empty() {
            return None;
        }
        self.search_in_flight = true;
        self.phase = IssuePhase::Searching;
        Some((
            SearchToken {
                popup: self.popup_id,
                generation: self.generation,
            },
            query,
        ))
    }

    /// Whether a reply is still about the query this page is showing.
    fn is_current(&self, token: SearchToken) -> bool {
        self.issue && token.popup == self.popup_id && token.generation == self.generation
    }

    /// Take a search answer, or drop it if the query has moved on. When a
    /// submission was queued while the search was out, its read starts here and
    /// the token comes back so the loop starts it exactly once.
    pub(in crate::tui) fn apply_search(
        &mut self,
        token: SearchToken,
        result: Result<Vec<IssueSummary>, String>,
    ) -> Option<u64> {
        if !self.is_current(token) {
            return None;
        }
        self.search_in_flight = false;
        match result {
            Ok(results) => {
                self.results = results;
                self.selected_result = 0;
                self.phase = IssuePhase::Ready;
                self.take_queued_submit()
            }
            Err(error) => {
                self.phase = IssuePhase::Failed(error);
                self.queued_submit = false;
                None
            }
        }
    }

    /// Whether the query is still resting before a search, or out on the wire,
    /// so an answer has to wait for its first result.
    fn is_search_pending(&self) -> bool {
        matches!(self.phase, IssuePhase::Searching) || self.query_due.is_some()
    }

    /// Answer a submission queued while the search was out, now that there is a
    /// first result. Nothing is read when the search found nothing.
    fn take_queued_submit(&mut self) -> Option<u64> {
        if !mem::take(&mut self.queued_submit) || self.results.is_empty() {
            return None;
        }
        self.begin_submission()
    }

    /// Say that there is no credential to search with, which is a state of its
    /// own rather than an empty result.
    pub(in crate::tui) fn search_unavailable(&mut self, token: SearchToken, message: String) {
        if !self.is_current(token) {
            return;
        }
        self.search_in_flight = false;
        self.queued_submit = false;
        self.phase = IssuePhase::Failed(message);
    }

    /// The reference a submission is reading, if the token is still the one in
    /// front of the operator.
    pub(in crate::tui) fn submission_reference(&self, token: u64) -> Option<IssueReference> {
        self.submission
            .as_ref()
            .filter(|submission| submission.token == token)
            .map(|submission| submission.reference.clone())
    }

    /// Take the submission once its snapshot has arrived, or nothing if the
    /// question was dismissed while it was out.
    pub(in crate::tui) fn take_submission(&mut self, token: u64) -> Option<Submission> {
        match &self.submission {
            Some(submission) if submission.token == token => self.submission.take(),
            _ => None,
        }
    }

    /// A snapshot read that produced none, whether it failed or could not start,
    /// leaving the results in place to try again.
    pub(in crate::tui) fn read_failed(&mut self, token: u64, error: String) {
        if self.submission.as_ref().is_some_and(|s| s.token == token) {
            self.submission = None;
            self.read_error = Some(error);
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

    fn select_result(&mut self, forward: bool) {
        let count = self.results.len();
        if count == 0 {
            return;
        }
        self.selected_result = if forward {
            (self.selected_result + 1) % count
        } else {
            (self.selected_result + count - 1) % count
        };
    }

    fn answer(&mut self) -> Option<Answer> {
        if self.issue {
            // A search that is still resting or out is not a lost answer: the
            // submission waits for the first result, keeping the choices made
            // for the environment, the agent and the machine.
            if self.queued_submit {
                return None;
            }
            if self.is_search_pending() {
                self.queued_submit = true;
                return None;
            }
        }
        // A task with nothing to do is not a task, so the question is asked
        // again, and this time the page says what is missing.
        if self.refusal().is_some() {
            self.refused = true;
            self.focus = Focus::Task;
            return None;
        }
        if self.issue {
            let token = self.begin_submission()?;
            return Some(Answer::ReadIssue { token });
        }
        let (environment, agent, host, remember) = self.selections()?;
        Some(Answer::Create {
            task: self.task.value().trim().to_owned(),
            environment,
            agent,
            host,
            remember,
        })
    }

    /// Start reading the chosen issue, remembering what to found when it lands.
    fn begin_submission(&mut self) -> Option<u64> {
        if self.submission.is_some() {
            return None;
        }
        let summary = self.results.get(self.selected_result)?.clone();
        let (environment, agent, host, remember) = self.selections()?;
        let token = NEXT_SUBMISSION.fetch_add(1, Ordering::Relaxed);
        self.read_error = None;
        self.submission = Some(Submission {
            token,
            reference: summary.reference,
            environment,
            agent,
            host,
            remember,
        });
        Some(token)
    }

    fn selections(&self) -> Option<(Environment, AgentKind, Option<String>, Option<AgentKind>)> {
        let environment = self.environment.chosen_value()?.clone();
        let agent = *self.agent.chosen_value()?;
        let host = self.host.chosen_value()?.clone();
        Some((
            environment,
            agent,
            host,
            self.agent_unchosen.then_some(agent),
        ))
    }

    /// The open list, over the prompt above the pill it belongs to.
    fn draw_open_list(&self, frame: &mut Frame, area: Rect, anchors: [Rect; 3]) {
        match self.focus {
            Focus::Environment if self.environment.is_open() => {
                self.environment.draw_list(frame, area, anchors[0]);
            }
            Focus::Agent if self.agent.is_open() => {
                self.agent.draw_list(frame, area, anchors[1]);
            }
            Focus::Host if self.host.is_open() => {
                self.host.draw_list(frame, area, anchors[2]);
            }
            _ => {}
        }
    }
}

/// The machines, as offers: one that cannot be given work is listed all the
/// same, because the reason it cannot is worth reading on its own pill.
fn host_offers(hosts: Vec<Host>) -> Vec<Offer<Option<String>>> {
    hosts
        .into_iter()
        .map(|host| Offer {
            value: host.address,
            label: host.label,
            ready: host.ready,
        })
        .collect()
}

pub fn draw(frame: &mut Frame, area: Rect, new_task: &NewTask, tick: Option<usize>) -> Rect {
    let popup = chrome::centered(area, WIDTH_PERCENT, HEIGHT);
    frame.render_widget(Clear, popup);
    let block = chrome::modal(if new_task.issue {
        "New task — link an issue"
    } else {
        "New task"
    });
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 8 {
        return popup;
    }
    // The pills sit under the prompt rather than over it: the question is
    // written first, and what it will run as reads as a footer to it.
    let [label, body, _, pills, actions] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(3),
    ])
    .areas(inner);
    frame.render_widget(
        Line::styled(
            if new_task.issue { "Issue" } else { "Prompt" },
            palette::muted(),
        ),
        label,
    );
    let anchors = draw_choices(frame, pills, new_task);
    if new_task.issue {
        let [query, results] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(body);
        new_task.task.draw(frame, query, new_task.is_typing());
        draw_issue(frame, results, new_task, tick);
    } else {
        new_task.task.draw(frame, body, new_task.is_typing());
    }
    draw_actions(frame, actions, new_task);
    new_task.draw_open_list(frame, body, anchors);
    popup
}

/// The results, or the reason there are none yet.
fn draw_issue(frame: &mut Frame, area: Rect, new_task: &NewTask, tick: Option<usize>) {
    let note = new_task
        .read_error
        .as_deref()
        .map(|error| Line::styled(error.to_owned(), palette::bad()))
        .or_else(|| {
            new_task
                .queued_submit
                .then(|| Line::styled("linking the first result…", palette::attention()))
        });
    let [note_area, body] = Layout::vertical([
        Constraint::Length(u16::from(note.is_some())),
        Constraint::Min(0),
    ])
    .areas(area);
    if let Some(note) = note {
        frame.render_widget(Paragraph::new(note).wrap(Wrap { trim: true }), note_area);
    }
    if new_task.submission.is_some() {
        let label = "reading issue…";
        frame.render_widget(Paragraph::new(Line::from_iter(shimmer(label, tick))), body);
        return;
    }
    match &new_task.phase {
        IssuePhase::Idle => frame.render_widget(
            Paragraph::new(Line::styled(
                "Type an issue ID, paste a Linear URL, or search terms.",
                palette::muted(),
            ))
            .wrap(Wrap { trim: true }),
            body,
        ),
        IssuePhase::Searching => {
            let label = "searching Linear…";
            frame.render_widget(Paragraph::new(Line::from_iter(shimmer(label, tick))), body);
        }
        IssuePhase::Failed(message) => frame.render_widget(
            Paragraph::new(Line::styled(message.clone(), palette::bad())).wrap(Wrap { trim: true }),
            body,
        ),
        IssuePhase::Ready if new_task.results.is_empty() => frame.render_widget(
            Paragraph::new(Line::styled("No matching issues.", palette::muted())),
            body,
        ),
        IssuePhase::Ready => {
            let width = usize::from(body.width);
            let items: Vec<ListItem> = new_task
                .results
                .iter()
                .enumerate()
                .map(|(index, summary)| {
                    let focused = index == new_task.selected_result;
                    ListItem::new(result_line(summary, width, focused))
                })
                .collect();
            let mut state = ListState::default();
            state.select(Some(new_task.selected_result));
            frame.render_stateful_widget(List::new(items), body, &mut state);
        }
    }
}

/// One search result: what it is called, what it is about, and where it lives.
fn result_line(summary: &IssueSummary, width: usize, focused: bool) -> Line<'static> {
    let text = match &summary.project {
        Some(project) => format!(
            "{}  {}  · {project}",
            summary.reference.identifier, summary.title
        ),
        None => format!("{}  {}", summary.reference.identifier, summary.title),
    };
    if focused {
        return Line::styled(truncate(&text, width), palette::focus());
    }
    let mut spans = vec![
        Span::styled(summary.reference.identifier.clone(), palette::accent()),
        Span::raw(format!("  {}", summary.title)),
    ];
    if let Some(project) = &summary.project {
        spans.push(Span::styled(format!("  · {project}"), palette::muted()));
    }
    Line::from(spans)
}

/// The three decisions, as pills: what is chosen is what you press.
fn draw_choices(frame: &mut Frame, area: Rect, new_task: &NewTask) -> [Rect; 3] {
    let named = [
        new_task.environment.pill_width(true),
        new_task.agent.pill_width(true),
        new_task.host.pill_width(true),
    ];
    let valued = [
        new_task.environment.pill_width(false),
        new_task.agent.pill_width(false),
        new_task.host.pill_width(false),
    ];
    let available = area.width.saturating_sub(PILL_GAP * 2);
    // A pill is its value first: a row too narrow for the names keeps what each
    // one is set to, and the open list says which decision it belongs to.
    let (widths, with_name) = if named.iter().sum::<u16>() <= available {
        (named, true)
    } else if valued.iter().sum::<u16>() <= available {
        (valued, false)
    } else {
        ([available / 3; 3], false)
    };
    let [environment, agent, host] = Layout::horizontal([
        Constraint::Length(widths[0]),
        Constraint::Length(widths[1]),
        Constraint::Length(widths[2]),
    ])
    .spacing(PILL_GAP)
    .areas(area);
    let focused = new_task.focus;
    new_task
        .environment
        .draw_pill(frame, environment, focused == Focus::Environment, with_name);
    new_task
        .agent
        .draw_pill(frame, agent, focused == Focus::Agent, with_name);
    new_task
        .host
        .draw_pill(frame, host, focused == Focus::Host, with_name);
    [environment, agent, host]
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
        if new_task.issue { "link" } else { "create" },
        new_task.focus == Focus::Create,
        new_task.refusal().is_none(),
    );
}

/// Whether text is shaped like something Linear can be asked about directly: an
/// issue URL, or an issue identifier. A prompt that happens to contain either is
/// not one of these, so the page only changes mode when the whole field is.
fn looks_like_issue(value: &str) -> bool {
    if value.starts_with("https://linear.app/") && value.contains("/issue/") {
        return true;
    }
    let Some((key, number)) = value.rsplit_once('-') else {
        return false;
    };
    !key.is_empty()
        && key.len() <= 10
        && key.chars().all(|character| character.is_ascii_uppercase())
        && !number.is_empty()
        && number.chars().all(|character| character.is_ascii_digit())
}
