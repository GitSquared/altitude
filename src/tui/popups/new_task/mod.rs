use std::{
    mem,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Frame, layout::Rect};

use crate::tui::keys;
use crate::tui::widgets::{
    chrome,
    pointer::Targets,
    select::{Offer, Select},
    text::TextBox,
};
use crate::{
    agent::AgentKind,
    project::Project,
    tracker::{IssueReference, IssueSummary},
};

mod draw;

pub use draw::draw;
use draw::looks_like_issue;

const WIDTH_PERCENT: u16 = 70;
const MAX_WIDTH: u16 = 88;
const HEIGHT: u16 = 18;
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);

static NEXT_POPUP: AtomicU64 = AtomicU64::new(1);
static NEXT_SUBMISSION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Task,
    Project,
    Cancel,
    Create,
}

const ORDER: [Focus; 4] = [Focus::Task, Focus::Project, Focus::Cancel, Focus::Create];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum Hit {
    Prompt,
    Pill,
    Offer(usize),
    Cancel,
    Create,
}

pub struct Host {
    pub address: Option<String>,
    pub label: String,
    pub ready: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SearchToken {
    popup: u64,
    generation: u64,
}

enum IssuePhase {
    Idle,
    Searching,
    Ready,
    Failed(String),
}

pub(in crate::tui) struct Submission {
    pub(in crate::tui) token: u64,
    reference: IssueReference,
    pub(in crate::tui) project: Project,
    pub(in crate::tui) agent: AgentKind,
}

pub struct NewTask {
    task: TextBox,
    project: Select<Project>,
    hosts: Vec<Host>,
    refused: bool,
    focus: Focus,

    popup_id: u64,
    generation: u64,
    issue: bool,
    phase: IssuePhase,
    results: Vec<IssueSummary>,
    selected_result: usize,
    query_due: Option<Instant>,
    search_in_flight: bool,
    submission: Option<Submission>,
    queued_submit: bool,
    focus_return: Option<Focus>,
    read_error: Option<String>,
}

pub enum Answer {
    Cancel,
    Create {
        task: String,
        project: Box<Project>,
        agent: AgentKind,
    },
    ReadIssue {
        token: u64,
    },
}

impl NewTask {
    pub fn new(projects: Vec<Project>, default_project: &str, hosts: Vec<Host>) -> Self {
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
        Self {
            task: TextBox::new(""),
            project,
            hosts,
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

    fn machine(&self) -> (String, bool) {
        let Some(project) = self.project.chosen_value() else {
            return ("this machine".to_owned(), true);
        };
        let Some(address) = &project.host else {
            return ("this machine".to_owned(), true);
        };
        match self
            .hosts
            .iter()
            .find(|host| host.address.as_deref() == Some(address.as_str()))
        {
            Some(host) => (host.label.clone(), host.ready),
            None => (format!("{address} (not a remote here)"), false),
        }
    }

    pub(in crate::tui) fn linking(mut self) -> Self {
        self.activate_issue();
        self
    }

    pub(in crate::tui) fn is_typing(&self) -> bool {
        self.focus == Focus::Task || self.list_open()
    }

    fn list_open(&self) -> bool {
        self.focus == Focus::Project && self.project.is_open()
    }

    pub(in crate::tui) fn is_awaiting(&self) -> bool {
        self.issue && (self.submission.is_some() || matches!(self.phase, IssuePhase::Searching))
    }

    pub fn hints(&self) -> Vec<chrome::Hint> {
        // An open list explains itself, and the widget is the one that knows
        // whether its keys are a query or a way through.
        if let Some(hints) = self.open_list_hints() {
            return hints;
        }
        // Only what this page adds. Tab and enter are the same here as anywhere,
        // and a line that repeats them costs a line that could say something the
        // page alone knows.
        let mut hints = vec![chrome::Hint::new("ctrl-e", "project")];
        if self.issue {
            hints.push(chrome::Hint::new("↑/↓", "result"));
        }
        hints.push(if self.issue {
            chrome::Hint::new("↩", "link")
        } else {
            chrome::Hint::new("ctrl-↩", "create")
        });
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

    fn open_list_hints(&self) -> Option<Vec<chrome::Hint>> {
        self.project.is_open().then(|| self.project.hints())
    }

    fn paste_into_list(&mut self, text: &str) {
        if self.focus == Focus::Project {
            self.project.paste(text);
        }
    }

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
            Some(Hit::Pill) => {
                self.focus = Focus::Project;
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
        if self.focus == Focus::Project {
            self.project.choose(index);
        }
        self.close_lists();
    }

    fn scroll_list(&mut self, forward: bool) {
        if self.focus == Focus::Project {
            self.project.scroll(forward);
        }
    }

    fn close_lists(&mut self) {
        self.project.close();
    }

    pub fn refusal(&self) -> Option<&'static str> {
        // A read under way, or a submission waiting for the first result, is not
        // a second answer.
        if self.submission.is_some() || self.queued_submit {
            return None;
        }
        // Only an installation that has not been set up has no harness to run a
        // task as; a project always names one.
        if self.project.chosen_value().is_some() && self.agent().is_none() {
            return Some("no coding agent is set up yet");
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
        // The choice has a chord of its own: reaching for the pill with the keys
        // should not mean tabbing past the prompt to get to it. A chord is a
        // detour rather than a move, so closing the list puts the keys back where
        // they were.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
            self.focus_return = Some(self.focus);
            self.focus = Focus::Project;
            self.open_list();
            return None;
        }
        // The answer is a chord, so it is never a key that writes the intent:
        // enter adds a line, and this starts the work. Link an issue, though, and
        // enter is already the answer, so the chord has nothing left to say.
        if keys::is_secondary(key) {
            return (!self.issue).then(|| self.answer()).flatten();
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

    fn list_key(&mut self, key: KeyEvent) -> bool {
        self.focus == Focus::Project && self.project.handle_key(key)
    }

    fn open_list(&mut self) {
        self.close_lists();
        if self.focus == Focus::Project {
            self.project.open();
        }
    }

    fn prompt_key(&mut self, key: KeyEvent) -> Option<Answer> {
        match key.code {
            // The issue page's prompt is one line of query, so enter is not a
            // second line there: it links the highlighted issue.
            KeyCode::Enter if self.issue => self.answer(),
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

    fn is_current(&self, token: SearchToken) -> bool {
        self.issue && token.popup == self.popup_id && token.generation == self.generation
    }

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

    fn is_search_pending(&self) -> bool {
        matches!(self.phase, IssuePhase::Searching) || self.query_due.is_some()
    }

    fn take_queued_submit(&mut self) -> Option<u64> {
        if !mem::take(&mut self.queued_submit) || self.results.is_empty() {
            return None;
        }
        self.begin_submission()
    }

    pub(in crate::tui) fn search_unavailable(&mut self, token: SearchToken, message: String) {
        if !self.is_current(token) {
            return;
        }
        self.search_in_flight = false;
        self.queued_submit = false;
        self.phase = IssuePhase::Failed(message);
    }

    pub(in crate::tui) fn submission_reference(&self, token: u64) -> Option<IssueReference> {
        self.submission
            .as_ref()
            .filter(|submission| submission.token == token)
            .map(|submission| submission.reference.clone())
    }

    pub(in crate::tui) fn take_submission(&mut self, token: u64) -> Option<Submission> {
        match &self.submission {
            Some(submission) if submission.token == token => self.submission.take(),
            _ => None,
        }
    }

    pub(in crate::tui) fn read_failed(&mut self, token: u64, error: String) {
        if self.submission.as_ref().is_some_and(|s| s.token == token) {
            self.submission = None;
            self.read_error = Some(error);
        }
    }

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
            // for the project and the agent.
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
        let (project, agent) = self.selections()?;
        Some(Answer::Create {
            task: self.task.value().trim().to_owned(),
            project: Box::new(project),
            agent,
        })
    }

    fn begin_submission(&mut self) -> Option<u64> {
        if self.submission.is_some() {
            return None;
        }
        let summary = self.results.get(self.selected_result)?.clone();
        let (project, agent) = self.selections()?;
        let token = NEXT_SUBMISSION.fetch_add(1, Ordering::Relaxed);
        self.read_error = None;
        self.submission = Some(Submission {
            token,
            reference: summary.reference,
            project,
            agent,
        });
        Some(token)
    }

    fn selections(&self) -> Option<(Project, AgentKind)> {
        let project = self.project.chosen_value()?.clone();
        let agent = self.agent()?;
        Some((project, agent))
    }

    fn agent(&self) -> Option<AgentKind> {
        self.project
            .chosen_value()
            .and_then(|project| project.agent)
    }

    fn draw_open_list(&self, frame: &mut Frame, area: Rect, anchor: Rect) -> Vec<(Rect, usize)> {
        if self.project.is_open() {
            return self.project.draw_list(frame, area, anchor);
        }
        Vec::new()
    }
}
