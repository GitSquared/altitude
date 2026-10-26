//! Creating a task: what to do, what drives it, and which project it is for.
//!
//! One question rather than three, asked with the configured defaults already in
//! place. The prompt is the question, so it gets the room: an intent is prose,
//! and it is the only thing here that is written rather than chosen. The choices
//! below it are named, because two values on a line say nothing about which is
//! which. Where the task runs is not one of them: its project names a machine, and
//! a task does not choose.
//!
//! The same page can also bind a Linear issue instead of writing an intent. The
//! prompt becomes the search box, the results take the room the prompt had, and
//! the choices stay exactly where they were. It opens that way from `ctrl-l`, or
//! by typing an issue identifier rather than a sentence.
//!
//! Searching is a network call, so this page never makes one. It says when a
//! query is due and what came back, and the loop in `run` does the waiting.
//! Every answer carries the popup and the query it was asked for, so a reply
//! that arrives after the query changed, or after this page closed, is dropped
//! rather than shown under the wrong question.
//!
//! The choices are pills rather than rows: what is chosen is the thing you press,
//! and enter opens it into a list you can narrow by typing. They sit
//! under the prompt, so the list opens up over what has been written rather than
//! down over the actions. Each has a chord of its own, so reaching one does not
//! mean tabbing past the prompt.
//!
//! Enter is a line in the prompt, because an intent is prose. Answering is
//! ctrl-enter, so the keys that start a task are never the keys that write one.
//! It is also the one key here that a terminal has to be told to report as a key
//! of its own, which is why the page asks for that protocol while it is open.
//!
//! Everything drawn here can also be clicked: the pills, the offers under them,
//! the prompt and the two answers. The rectangles come from the last frame, so a
//! click lands on what the operator saw rather than on what the layout would be.
//!
//! The agent's first answer becomes its default. On a fresh installation there
//! is nothing to default to, and guessing on someone's behalf is not Altitude's
//! to do.

use std::{
    mem,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    text::{Line, Span},
    widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::tui::keys;
use crate::tui::widgets::{
    chrome, palette,
    pointer::Targets,
    select::{Offer, Select},
    text::TextBox,
    utils::{shimmer, truncate},
};
use crate::{
    agent::AgentKind,
    project::Project,
    tracker::{IssueReference, IssueSummary},
};

/// The agents on offer, in the order they are shown. One catalog, so the
/// creation question and the settings list cannot disagree about them.
pub(in crate::tui) const AGENTS: [AgentKind; 2] = [AgentKind::Pi, AgentKind::Claude];

/// The page's width, as a share of the terminal. Wider than a question about one
/// value, because the answer to this one is a paragraph.
const WIDTH_PERCENT: u16 = 70;
/// How wide it may get. The prompt is read a line at a time, so past a legible
/// measure a wider window only makes the line harder to follow.
const MAX_WIDTH: u16 = 88;
/// Its height, which is the prompt's room plus the spacing, the pills and the
/// actions. The prompt takes what is left of it.
const HEIGHT: u16 = 17;
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
    Project,
    Agent,
    Cancel,
    Create,
}

const ORDER: [Focus; 5] = [
    Focus::Task,
    Focus::Project,
    Focus::Agent,
    Focus::Cancel,
    Focus::Create,
];

/// What a click on this page can land on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum Hit {
    Prompt,
    /// One of the choices, by the control it belongs to.
    Pill(Focus),
    /// One offer of the open list, by its place among the offers.
    Offer(usize),
    Cancel,
    Create,
}

/// A machine a task could be sent to.
///
/// The creation page no longer chooses one, because a task runs where its project
/// runs; the send chooser still does, for the one move a task has.
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
    pub(in crate::tui) project: Project,
    pub(in crate::tui) agent: AgentKind,
    pub(in crate::tui) remember: Option<AgentKind>,
}

/// The question, and the answers so far.
pub struct NewTask {
    task: TextBox,
    project: Select<Project>,
    agent: Select<AgentKind>,
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
    /// Where the keys were before a chord jumped to a pill, so closing its list
    /// puts them back: a chord is a detour, tabbing is a move.
    focus_return: Option<Focus>,
    /// A snapshot read that failed, kept so the operator can try again.
    read_error: Option<String>,
}

/// What the question ended in.
pub enum Answer {
    Cancel,
    Create {
        task: String,
        project: Box<Project>,
        agent: AgentKind,
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
    ///
    /// `default_project` is the project in front: the tabs are the context a task
    /// is created in, so the question opens on the one being looked at.
    pub fn new(
        projects: Vec<Project>,
        default_project: &str,
        default_agent: Option<AgentKind>,
    ) -> Self {
        let chosen = projects
            .iter()
            .position(|candidate| candidate.id == default_project)
            .unwrap_or(0);
        let project = Select::new(
            "project",
            projects
                .into_iter()
                .map(|project| {
                    let label = project.name.clone();
                    Offer::new(project, label)
                })
                .collect(),
            chosen,
        );
        let agent = Select::new(
            "agent",
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
            project,
            agent,
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
            focus_return: None,
            read_error: None,
        }
    }

    /// Ask the issue page instead of the prompt: the same question, reached from
    /// a task that wants a tracker link rather than a free intent.
    pub(in crate::tui) fn linking(mut self) -> Self {
        self.activate_issue();
        self
    }

    /// Put the machines back in step with what the remotes have answered, keeping
    /// the one the operator chose where it is still offered.
    /// Whether something here is taking characters, in which case `?` and `q`
    /// are characters too.
    pub(in crate::tui) fn is_typing(&self) -> bool {
        self.focus == Focus::Task || self.list_open()
    }

    /// Whether the focused pill has its list open.
    fn list_open(&self) -> bool {
        match self.focus {
            Focus::Project => self.project.is_open(),
            Focus::Agent => self.agent.is_open(),
            _ => false,
        }
    }

    /// Whether that list takes a query, rather than only the keys that move
    /// through it.
    fn list_searching(&self) -> bool {
        match self.focus {
            Focus::Project => self.project.is_searching(),
            Focus::Agent => self.agent.is_searching(),
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
        // Only what this page adds. Tab and enter are the same here as anywhere,
        // and a line that repeats them costs a line that could say something the
        // page alone knows.
        let mut hints = vec![chrome::Hint::new("ctrl-e", "project")];
        hints.push(chrome::Hint::new("ctrl-a", "agent"));
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
            Focus::Project => self.project.paste(text),
            Focus::Agent => self.agent.paste(text),
            _ => {}
        }
    }

    /// Read a mouse event against what the last frame drew.
    pub(in crate::tui) fn mouse(
        &mut self,
        mouse: MouseEvent,
        targets: &Targets<Hit>,
    ) -> Option<Answer> {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_list(true);
                None
            }
            MouseEventKind::ScrollDown => {
                self.scroll_list(false);
                None
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.click(targets.at(mouse.column, mouse.row))
            }
            _ => None,
        }
    }

    fn click(&mut self, target: Option<&Hit>) -> Option<Answer> {
        let answer = match target {
            Some(Hit::Prompt) => {
                self.focus = Focus::Task;
                None
            }
            Some(Hit::Pill(pill)) => {
                self.focus = *pill;
                self.focus_return = None;
                self.open_list();
                None
            }
            // A menu row is a choice made: one click, not click then confirm.
            Some(Hit::Offer(index)) => {
                let index = *index;
                self.choose(index);
                None
            }
            Some(Hit::Cancel) => Some(Answer::Cancel),
            Some(Hit::Create) => self.answer(),
            // A click that lands on nothing closes the menu, the way a click
            // outside one does everywhere else.
            None => {
                self.close_lists();
                None
            }
        };
        // Whatever closed the list, a chord-opened one goes back where it came
        // from; a clicked one had no earlier place to go back to.
        if !self.list_open() {
            self.restore_focus();
        }
        answer
    }

    fn choose(&mut self, index: usize) {
        match self.focus {
            Focus::Project => self.project.choose(index),
            Focus::Agent => self.agent.choose(index),
            _ => {}
        }
        self.close_lists();
    }

    fn scroll_list(&mut self, forward: bool) {
        match self.focus {
            Focus::Project => self.project.scroll(forward),
            Focus::Agent => self.agent.scroll(forward),
            _ => {}
        }
    }

    fn close_lists(&mut self) {
        self.project.close();
        self.agent.close();
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
            if !self.list_open() {
                self.restore_focus();
            }
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
        // The choices have chords of their own: reaching for a pill with the keys
        // should not mean tabbing past the prompt to get to it. A chord
        // is a detour rather than a move, so closing the list puts the keys back
        // where they were.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            let pill = match key.code {
                KeyCode::Char('e') => Some(Focus::Project),
                KeyCode::Char('a') => Some(Focus::Agent),
                _ => None,
            };
            if let Some(pill) = pill {
                self.focus_return = Some(self.focus);
                self.focus = pill;
                self.open_list();
                return None;
            }
        }
        // The answer is a chord, so it is never a key that writes the intent:
        // enter adds a line, and this starts the work.
        if keys::is_secondary(key) {
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
                    self.focus_return = None;
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
            Focus::Project => self.project.handle_key(key),
            Focus::Agent => self.agent.handle_key(key),
            _ => false,
        }
    }

    fn open_list(&mut self) {
        self.close_lists();
        match self.focus {
            Focus::Project => self.project.open(),
            Focus::Agent => self.agent.open(),
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

    fn activate_issue(&mut self) {
        if self.issue {
            return;
        }
        self.issue = true;
        self.read_error = None;
        self.query_changed();
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

    /// Put the keys back where a chord found them, if it was a chord that moved
    /// them here.
    fn restore_focus(&mut self) {
        if let Some(focus) = self.focus_return.take() {
            self.focus = focus;
        }
    }

    fn move_focus(&mut self, forward: bool) {
        self.focus_return = None;
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
            // for the project, the agent and the machine.
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
        let (project, agent, remember) = self.selections()?;
        Some(Answer::Create {
            task: self.task.value().trim().to_owned(),
            project: Box::new(project),
            agent,
            remember,
        })
    }

    /// Start reading the chosen issue, remembering what to found when it lands.
    fn begin_submission(&mut self) -> Option<u64> {
        if self.submission.is_some() {
            return None;
        }
        let summary = self.results.get(self.selected_result)?.clone();
        let (project, agent, remember) = self.selections()?;
        let token = NEXT_SUBMISSION.fetch_add(1, Ordering::Relaxed);
        self.read_error = None;
        self.submission = Some(Submission {
            token,
            reference: summary.reference,
            project,
            agent,
            remember,
        });
        Some(token)
    }

    fn selections(&self) -> Option<(Project, AgentKind, Option<AgentKind>)> {
        let project = self.project.chosen_value()?.clone();
        let agent = *self.agent.chosen_value()?;
        Some((project, agent, self.agent_unchosen.then_some(agent)))
    }

    /// The open list, over the prompt above the pill it belongs to, and where
    /// each offer was drawn.
    fn draw_open_list(
        &self,
        frame: &mut Frame,
        area: Rect,
        anchors: [Rect; 2],
    ) -> Vec<(Rect, usize)> {
        match self.focus {
            Focus::Project if self.project.is_open() => {
                self.project.draw_list(frame, area, anchors[0])
            }
            Focus::Agent if self.agent.is_open() => self.agent.draw_list(frame, area, anchors[1]),
            _ => Vec::new(),
        }
    }
}

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    new_task: &NewTask,
    tick: Option<usize>,
    targets: &mut Targets<Hit>,
) -> Rect {
    let popup = chrome::centered_at_most(area, WIDTH_PERCENT, MAX_WIDTH, HEIGHT);
    frame.render_widget(Clear, popup);
    let block = chrome::modal(if new_task.issue {
        "New task — link an issue"
    } else {
        "New task"
    });
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    targets.clear();
    if inner.height < 10 {
        return popup;
    }
    // The pills sit right under the prompt, on the right like a toolbar, and the
    // answers close the window under a rule: the prompt is the question, what it
    // will run as qualifies it, and the two answers end it.
    let body = chrome::body(inner);
    let [label, prompt, pills] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas(body.content);
    frame.render_widget(
        Line::styled(
            if new_task.issue { "Issue" } else { "Prompt" },
            palette::muted(),
        ),
        label,
    );

    let anchors = draw_choices(frame, pills, new_task);
    for (pill, anchor) in [Focus::Project, Focus::Agent].into_iter().zip(anchors) {
        targets.add(anchor, Hit::Pill(pill));
    }

    let prompt_area = if new_task.issue {
        let [query, results] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(prompt);
        new_task.task.draw(frame, query, new_task.is_typing());
        draw_issue(frame, results, new_task, tick);
        query
    } else {
        new_task.task.draw(frame, prompt, new_task.is_typing());
        prompt
    };
    targets.add(prompt_area, Hit::Prompt);

    // Said only once it has been asked for: an empty prompt is not a mistake
    // until an answer says it is. It goes beside the pills, which leave the left
    // of their row free by sitting on the right.
    if new_task.refused
        && let Some(refusal) = new_task.refusal()
    {
        frame.render_widget(
            Line::styled(refusal, palette::attention()),
            Rect {
                x: inner.x,
                y: pills.y,
                width: inner.width,
                height: 1,
            },
        );
    }

    let buttons = chrome::actions(
        frame,
        popup,
        body.actions,
        &[
            chrome::Action::secondary("cancel").focused(new_task.focus == Focus::Cancel),
            chrome::Action::primary(if new_task.issue { "link" } else { "create" })
                .focused(new_task.focus == Focus::Create),
        ],
    );
    if let [cancel, create] = buttons[..] {
        targets.add(cancel, Hit::Cancel);
        targets.add(create, Hit::Create);
    }

    for (row, offer) in new_task.draw_open_list(frame, prompt, anchors) {
        targets.add(row, Hit::Offer(offer));
    }
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

/// What is chosen, as pills: what is chosen is what you press.
fn draw_choices(frame: &mut Frame, area: Rect, new_task: &NewTask) -> [Rect; 2] {
    let named = [
        new_task.project.pill_width(true),
        new_task.agent.pill_width(true),
    ];
    let valued = [
        new_task.project.pill_width(false),
        new_task.agent.pill_width(false),
    ];
    let available = area.width.saturating_sub(PILL_GAP);
    // A pill is its value first: a row too narrow for the names keeps what each
    // one is set to, and the open list says which decision it belongs to.
    let (widths, with_name) = if named.iter().sum::<u16>() <= available {
        (named, true)
    } else if valued.iter().sum::<u16>() <= available {
        (valued, false)
    } else {
        ([available / 2; 2], false)
    };
    let [project, agent] =
        Layout::horizontal([Constraint::Length(widths[0]), Constraint::Length(widths[1])])
            .flex(Flex::End)
            .spacing(PILL_GAP)
            .areas(area);
    let focused = new_task.focus;
    new_task
        .project
        .draw_pill(frame, project, focused == Focus::Project, with_name);
    new_task
        .agent
        .draw_pill(frame, agent, focused == Focus::Agent, with_name);
    [project, agent]
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
