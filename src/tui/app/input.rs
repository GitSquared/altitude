use std::{io, mem};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tokio::sync::mpsc;

use crate::{
    credentials::{CredentialShellEvent, Slot},
    isolation::IsolationEngine,
    task::{TaskEventLoop, TaskService},
    tui::keys::{is_dismiss, is_remove, is_secondary},
    tui::popups,
    tui::screens,
    tui::widgets::text::Field,
};

use crate::tui::widgets::alert::Alert;
use popups::confirm::PendingConfirm;
use popups::message::{Decision as MessageDecision, MessagePrompt};
use popups::new_task::NewTask;
use popups::repositories::Key as RepositoriesKey;
use popups::scopes::Key as ScopesKey;
use popups::send_task::SendTask;
use screens::get_started::Focus as SetupFocus;
use screens::settings::{Focus, Target};
use screens::signals::{SignalFocus, SignalHit, primary_action, secondary_action};

use super::*;

impl App {
    pub(in crate::tui) async fn panel_keys<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
        tracker_connects: &mpsc::UnboundedSender<TrackerConnectReply>,
        repository_lists: &mpsc::UnboundedSender<RepositoryListReply>,
        tracker_scopes: &mpsc::UnboundedSender<TrackerScopesReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if matches!(self.credential_shell, CredentialShellState::Opening { .. }) {
            if is_dismiss(key) {
                self.close_credential_shell(task_service).await;
            }
            return;
        }
        if self.panel.form.is_some() {
            // The repository field opens its list from its own keys. A page this
            // long does not get a chord per field: reaching one is what tabbing
            // and the arrows are for.
            let wants_repositories = self.panel.form.as_ref().is_some_and(|form| {
                !form.target.is_remote()
                    && form.repositories.is_none()
                    && matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))
                    && form.focus == Focus::Repository
            });
            if wants_repositories {
                self.begin_repository_listing(task_service, repository_lists);
                return;
            }
            let scope_credential = self.panel.form.as_ref().and_then(|form| {
                form.tracker_credential().or_else(|| {
                    self.settings_file.as_ref().and_then(|file| {
                        file.settings()
                            .default_credential(Slot::Tracker)
                            .map(str::to_owned)
                    })
                })
            });
            let wants_scopes = matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))
                && self.panel.form.as_ref().is_some_and(|form| {
                    form.focus == Focus::Scope && form.scopes.is_none() && form.has_tracker()
                })
                && scope_credential
                    .as_deref()
                    .is_some_and(|id| self.tracker_state_for(id) == TrackerState::Connected);
            if wants_scopes {
                self.begin_scope_listing(task_service, tracker_scopes, scope_credential);
                return;
            }
            self.panel_form_key(key).await;
        } else {
            self.panel_key(key, task_service, events, tracker_connects)
                .await;
        }
    }

    pub(in crate::tui) async fn panel_form_key(&mut self, key: KeyEvent) {
        let Some(mut form) = self.panel.form.take() else {
            return;
        };
        let mut keep = true;
        let mut save = false;
        // An open list holds every key, escape included: it closes the list
        // rather than the page, which is what a dropdown does everywhere else.
        if form.list_open() {
            form.list_key(key);
            self.panel.form = Some(form);
            return;
        }
        if let Some(mut scopes) = form.scopes.take() {
            match scopes.key(key) {
                ScopesKey::Still => {
                    form.scopes = Some(scopes);
                    self.panel.form = Some(form);
                    return;
                }
                ScopesKey::Closed => {
                    self.panel.form = Some(form);
                    return;
                }
                ScopesKey::Chosen(scope) => {
                    let key = if scope.key.is_empty() {
                        scope.id.as_str()
                    } else {
                        scope.key.as_str()
                    };
                    form.scope = Field::with(key);
                    self.panel.form = Some(form);
                    return;
                }
                // A save while the list is open means the form is finished with
                // its choice, so the list closes and the save proceeds.
                ScopesKey::Save => save = true,
            }
        }
        if let Some(browser) = form.repositories.as_mut() {
            match browser.key(key) {
                RepositoriesKey::Still => {
                    self.panel.form = Some(form);
                    return;
                }
                RepositoriesKey::Closed => {
                    form.repositories = None;
                    self.panel.form = Some(form);
                    return;
                }
                RepositoriesKey::Chosen(repository) => {
                    form.repositories = None;
                    form.choose_repository(&repository.url);
                    self.panel.form = Some(form);
                    return;
                }
                RepositoriesKey::Save => {
                    form.repositories = None;
                    save = true;
                }
            }
        } else {
            match key.code {
                // Escape leaves the page from a text box too, so cancelling a form
                // is one gesture rather than a first press that only moves focus.
                KeyCode::Esc => keep = false,
                _ if is_dismiss(key) && !form.focus.takes_text() => keep = false,
                // The chord that starts work on the creation page saves here, so a
                // page with fields is finished the same way from wherever the caret
                // is.
                _ if is_secondary(key) => save = true,
                KeyCode::BackTab => form.move_focus(false),
                KeyCode::Tab => form.move_focus(true),
                // A field that is a pill has no caret to move, so the arrows and
                // the vim keys are travel: they go where tab goes. A one-line box
                // keeps left and right for its text but lets the vertical arrows
                // out; a box with several lines keeps all of them.
                KeyCode::Down if !form.focus.is_multiline() => form.move_focus(true),
                KeyCode::Up if !form.focus.is_multiline() => form.move_focus(false),
                KeyCode::Down | KeyCode::Right | KeyCode::Char('j') | KeyCode::Char('l')
                    if !form.focus.takes_text() =>
                {
                    form.move_focus(true)
                }
                KeyCode::Up | KeyCode::Left | KeyCode::Char('k') | KeyCode::Char('h')
                    if !form.focus.takes_text() =>
                {
                    form.move_focus(false)
                }
                KeyCode::Enter | KeyCode::Char(' ') if form.focus == Focus::Accept => {
                    if form.has_tracker() {
                        form.accepts_delegated_work = !form.accepts_delegated_work;
                    }
                }
                // A pill opens its list, the way a pill does everywhere else.
                // The two whose list is a page of its own are opened before this
                // by the keys that fetch it.
                KeyCode::Enter | KeyCode::Char(' ') if form.focus.is_a_pill() => {
                    form.open_pill();
                }
                KeyCode::Enter if form.enters_newline() => form.handle_text(key),
                KeyCode::Enter => match form.focus {
                    Focus::Cancel => keep = false,
                    Focus::Save => save = true,
                    _ => form.move_focus(true),
                },
                _ => form.handle_text(key),
            }
        }
        if save {
            let added = match form.target {
                Target::NewRemote => Some(form.primary.value().trim().to_owned()),
                _ => None,
            };
            // A refused save keeps the page open, so the reason and the work
            // are both still there.
            if !self.save_form(&form) {
                self.panel.form = Some(form);
            } else if let Some(address) = added {
                self.remote_status.remove(&address);
            }
        } else if keep {
            self.panel.form = Some(form);
        }
    }

    pub(in crate::tui) async fn signals_mouse<E>(
        &mut self,
        mouse: MouseEvent,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let target = self.signal_targets.at(mouse.column, mouse.row).copied();
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = mouse.kind == MouseEventKind::ScrollUp;
                if self.signal_filter.is_open() {
                    self.signal_filter.scroll(up);
                    return;
                }
                if self.signal_sort.is_open() {
                    self.signal_sort.scroll(up);
                    return;
                }
                match target {
                    Some(SignalHit::List | SignalHit::Thread(_)) => {
                        self.move_signal_cursor(!up);
                    }
                    Some(SignalHit::Plan) => {
                        self.signal_focus = SignalFocus::Plan;
                        self.scroll_plan(if up { -1 } else { 1 });
                    }
                    Some(SignalHit::Activity) => {
                        self.signal_focus = SignalFocus::Activity;
                        self.scroll_activity(if up { -1 } else { 1 });
                    }
                    Some(
                        SignalHit::Action(_)
                        | SignalHit::ComposerSend
                        | SignalHit::ComposerCancel
                        | SignalHit::Filter
                        | SignalHit::Sort,
                    ) => {}
                    _ => {
                        self.signal_focus = SignalFocus::Detail;
                        self.scroll_signal(if up { -3 } else { 3 });
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if (self.signal_filter.is_open() || self.signal_sort.is_open())
                    && !matches!(
                        target,
                        Some(SignalHit::FilterOffer(_) | SignalHit::SortOffer(_))
                    )
                {
                    self.signal_filter.close();
                    self.signal_sort.close();
                    return;
                }
                let Some(target) = target else {
                    return;
                };
                match target {
                    SignalHit::List => self.signal_focus = SignalFocus::List,
                    SignalHit::Thread(index) => {
                        self.signals_cursor.select(index);
                        self.signal_focus = SignalFocus::List;
                        self.reset_signal_message();
                    }
                    SignalHit::Detail => self.signal_focus = SignalFocus::Detail,
                    SignalHit::Plan => self.signal_focus = SignalFocus::Plan,
                    SignalHit::Activity => self.signal_focus = SignalFocus::Activity,
                    SignalHit::Message(index) => {
                        self.signal_focus = SignalFocus::Detail;
                        self.signal_message_cursor = index;
                    }
                    SignalHit::Action(index) => {
                        self.signal_focus = SignalFocus::Action(index);
                        if let Some(action) = self.signal_actions().get(index).copied() {
                            self.act_on_signal(action, task_service, remote_signals, event_loop)
                                .await;
                        }
                    }
                    SignalHit::ComposerSend => {
                        self.composer_key(
                            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                            event_loop,
                            remote_signals,
                        );
                    }
                    SignalHit::ComposerCancel => self.signal_composer = None,
                    SignalHit::Filter => {
                        self.signal_focus = SignalFocus::Filter;
                        self.signal_filter.open();
                    }
                    SignalHit::Sort => {
                        self.signal_focus = SignalFocus::Sort;
                        self.signal_sort.open();
                    }
                    SignalHit::FilterOffer(index) => {
                        self.signal_filter.choose(index);
                        self.signal_filter.close();
                        self.apply_signal_view();
                    }
                    SignalHit::SortOffer(index) => {
                        self.signal_sort.choose(index);
                        self.signal_sort.close();
                        self.apply_signal_view();
                    }
                }
            }
            _ => {}
        }
    }

    pub(in crate::tui) async fn message_key<E>(
        &mut self,
        key: KeyEvent,
        event_loop: &TaskEventLoop<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let (task_id, decision) = {
            let Foreground::Message(prompt) = &mut self.foreground else {
                return;
            };
            (prompt.task_id().to_owned(), prompt.handle_key(key))
        };
        match decision {
            Some(MessageDecision::Cancel) => self.foreground = Foreground::Signals,
            Some(MessageDecision::Send(body)) => {
                if let Some(remote) = self.remote_signals.clone() {
                    self.act_on_remote_input(remote, body, remote_signals);
                    self.foreground = Foreground::Signals;
                } else {
                    match event_loop.express_input(&task_id, body).await {
                        Ok(()) => {
                            self.foreground = Foreground::Signals;
                            self.alert = Some(Alert::note("Message sent to the agent."));
                        }
                        Err(error) => self.problem(error.to_string()),
                    }
                }
            }
            None => {}
        }
    }

    pub(in crate::tui) async fn signals_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if self.signal_composer.is_some() {
            self.composer_key(key, event_loop, remote_signals);
            return;
        }
        // A dropdown owns every key while it is open, escape included.
        if self.signal_filter.is_open() {
            self.signal_filter.handle_key(key);
            self.apply_signal_view();
            return;
        }
        if self.signal_sort.is_open() {
            self.signal_sort.handle_key(key);
            self.apply_signal_view();
            return;
        }
        // Tab switches the screen's tabs where it has them; the horizontal
        // arrows and h/l move between the parts of the screen. j/k and the
        // vertical arrows move inside the part that has the keys.
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            if self.signal_plan_tabs {
                self.signal_plan_open = !self.signal_plan_open;
            }
            return;
        }
        if matches!(
            key.code,
            KeyCode::Right | KeyCode::Left | KeyCode::Char('l') | KeyCode::Char('h')
        ) {
            let forward = matches!(key.code, KeyCode::Right | KeyCode::Char('l'));
            self.move_signal_focus(forward);
            return;
        }
        match key.code {
            KeyCode::PageDown => match self.signal_focus {
                SignalFocus::List => {
                    self.page_signal_cursor(true);
                }
                SignalFocus::Plan => self.page_plan(false),
                SignalFocus::Activity => self.page_activity(false),
                _ => self.page_signal(false),
            },
            KeyCode::PageUp => match self.signal_focus {
                SignalFocus::List => {
                    self.page_signal_cursor(false);
                }
                SignalFocus::Plan => self.page_plan(true),
                SignalFocus::Activity => self.page_activity(true),
                _ => self.page_signal(true),
            },
            KeyCode::Home => match self.signal_focus {
                SignalFocus::Plan => self.signal_plan_scroll = 0,
                SignalFocus::Activity => self.scroll_activity(-i32::from(u16::MAX)),
                _ => {
                    self.signal_scroll = 0;
                    self.select_signal_message_at_scroll();
                }
            },
            KeyCode::End => match self.signal_focus {
                SignalFocus::Plan => self.signal_plan_scroll = u16::MAX,
                SignalFocus::Activity => self.signal_activity_scroll = u16::MAX,
                _ => {
                    self.signal_scroll = u16::MAX;
                    self.select_signal_message_at_scroll();
                }
            },
            _ if is_dismiss(key) || key.code == KeyCode::Char('q') => self.leave_signals(),
            KeyCode::Down | KeyCode::Char('j') => match self.signal_focus {
                SignalFocus::List => {
                    let len = self.signal_threads.len();
                    if self.signals_cursor.selected() + 1 < len {
                        self.move_signal_cursor(true);
                    }
                }
                SignalFocus::Detail => self.move_signal_message(true),
                SignalFocus::Plan => self.scroll_plan(1),
                SignalFocus::Activity => self.scroll_activity(1),
                SignalFocus::Action(_) => self.move_signal_focus(true),
                SignalFocus::Filter | SignalFocus::Sort => {
                    self.signal_focus = SignalFocus::List;
                }
            },
            KeyCode::Up | KeyCode::Char('k') => match self.signal_focus {
                SignalFocus::List => {
                    if self.signals_cursor.selected() == 0 {
                        self.signal_focus = SignalFocus::Filter;
                    } else {
                        self.move_signal_cursor(false);
                    }
                }
                SignalFocus::Detail => self.move_signal_message(false),
                SignalFocus::Plan => self.scroll_plan(-1),
                SignalFocus::Activity => self.scroll_activity(-1),
                SignalFocus::Action(_) => self.move_signal_focus(false),
                SignalFocus::Filter | SignalFocus::Sort => {}
            },
            KeyCode::Char('y') if self.signal_focus == SignalFocus::Detail => {
                self.copy_signal_message();
            }
            KeyCode::Char('f') => {
                self.signal_focus = SignalFocus::Filter;
                self.signal_filter.open();
            }
            KeyCode::Char('s') => {
                self.signal_focus = SignalFocus::Sort;
                self.signal_sort.open();
            }
            KeyCode::Char('p') => self.signal_plan_open = !self.signal_plan_open,
            KeyCode::Char('n') if self.remote_signals.is_none() => {
                if let Some(task_id) = self.signal_task_id.clone() {
                    let agent = self.signal_agent_label();
                    self.foreground =
                        Foreground::Message(Box::new(MessagePrompt::new(task_id, agent)));
                }
            }
            KeyCode::Enter if is_secondary(key) => {
                if let Some(action) = self.signal_thread().and_then(secondary_action) {
                    self.act_on_signal(action, task_service, remote_signals, event_loop)
                        .await
                }
            }
            KeyCode::Enter => match self.signal_focus {
                SignalFocus::Filter => self.signal_filter.open(),
                SignalFocus::Sort => self.signal_sort.open(),
                SignalFocus::Action(index) => {
                    if let Some(action) = self.signal_actions().get(index).copied() {
                        self.act_on_signal(action, task_service, remote_signals, event_loop)
                            .await
                    }
                }
                SignalFocus::List
                | SignalFocus::Detail
                | SignalFocus::Plan
                | SignalFocus::Activity => {
                    if let Some(action) = self.signal_thread().and_then(primary_action) {
                        self.act_on_signal(action, task_service, remote_signals, event_loop)
                            .await
                    }
                }
            },
            _ => {}
        }
    }

    pub(in crate::tui) async fn list_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) -> io::Result<bool>
    where
        E: IsolationEngine + Clone + 'static,
    {
        // A task belongs to the machine running it, so a key does the same thing
        // wherever the card is drawn and only the trip differs: a task on another
        // machine is reached over the link rather than called directly. Taking one
        // over has to move it here first, and its question says so.
        if is_secondary(key) {
            let asked = self.selected_held().map(|held| {
                let agent = held
                    .summary
                    .agent
                    .map(|kind| kind.to_string().to_ascii_lowercase())
                    .unwrap_or_else(|| "the agent".to_owned());
                (
                    held.summary.id.clone(),
                    held.summary.title.clone(),
                    agent,
                    Some(self.remote_name(&held.address)),
                )
            });
            let asked = asked.or_else(|| {
                self.selected_task().map(|task| {
                    (
                        task.id.clone(),
                        task.title().to_owned(),
                        task.agent_kind
                            .map(|kind| kind.to_string().to_ascii_lowercase())
                            .unwrap_or_else(|| "the agent".to_owned()),
                        None,
                    )
                })
            });
            if let Some((id, title, agent, from)) = asked {
                self.ask_to_take_over(&id, &title, &agent, from);
            }
            return Ok(false);
        }

        if self.selected_is_remote() {
            // The same screen this machine's tasks use, for a task whose actions
            // travel to the machine that owns it.
            if key.code == KeyCode::Enter || key.code == KeyCode::Char('i') {
                self.open_signals(task_service, remote_signals).await;
                return Ok(false);
            }
            // The issue a task is bound to is a page, and it opens the same way
            // whether the task is here or on another machine: through this
            // machine's browser, by the operator's own act.
            if key.code == KeyCode::Char('b') {
                let issue = self
                    .selected_held()
                    .and_then(|held| held.summary.tracker_issue.clone());
                if let Some(issue) = issue {
                    match open_in_browser(&issue.url) {
                        Ok(()) => tracing::info!(%issue.url, "opened a remote task's issue"),
                        Err(error) => {
                            self.problem(format!("Could not open {}: {error}", issue.url))
                        }
                    }
                }
                return Ok(false);
            }
        }

        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
            // The tabs are one keystroke away in either direction, and they are
            // the context every other key works in.
            KeyCode::Tab => {
                self.switch_project(true);
                return Ok(false);
            }
            KeyCode::BackTab => {
                self.switch_project(false);
                return Ok(false);
            }
            KeyCode::Char('[') => {
                self.switch_project(false);
                return Ok(false);
            }
            KeyCode::Char(']') => {
                self.switch_project(true);
                return Ok(false);
            }
            KeyCode::Char('c') => self.open_new_task(false),
            // Linking an issue is the other way to start work: the same question,
            // asked of the tracker instead of the operator's prose. A chord,
            // because `l` is navigation wherever there is a column to its right.
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.reload_settings() {
                    let question = NewTask::new(
                        self.projects(),
                        &self.active_project_id(),
                        self.task_hosts(),
                    );
                    self.foreground = Foreground::NewTask(Box::new(question.linking()));
                }
            }
            KeyCode::Char('s') => {
                open_panel(self, task_service).await;
            }
            KeyCode::Char('i') => self.open_signals(task_service, remote_signals).await,
            // j and k walk the cards, the way a list does; the arrows keep the
            // grid's shape, where up and down are a row rather than a card.
            KeyCode::Char('j') => {
                let columns = self.grid_columns();
                self.select_adjacent(1, columns);
            }
            KeyCode::Char('k') => {
                let columns = self.grid_columns();
                self.select_adjacent(-1, columns);
            }
            KeyCode::Down => {
                let columns = self.grid_columns();
                self.select_adjacent(columns as isize, columns);
            }
            KeyCode::Up => {
                let columns = self.grid_columns();
                self.select_adjacent(-(columns as isize), columns);
            }
            KeyCode::Right | KeyCode::Char('l') => {
                let columns = self.grid_columns();
                self.select_adjacent(1, columns);
            }
            KeyCode::Left | KeyCode::Char('h') => {
                let columns = self.grid_columns();
                self.select_adjacent(-1, columns);
            }
            _ if is_remove(key) => {
                // A remote's task is removed the same way. Which machine holds it
                // is where the act happens, not what the act is.
                if let Some(held) = self.selected_held() {
                    let question = format!(
                        "Delete task \"{}\" from {}?",
                        held.summary.title,
                        self.remote_name(&held.address)
                    );
                    self.foreground = Foreground::Confirm(PendingConfirm::removing_remote(
                        held.address.clone(),
                        held.summary.id.clone(),
                        question,
                    ));
                } else if let Some(task) = self.selected_task() {
                    let question = format!("Stop any agents and delete task \"{}\"?", task.title());
                    self.foreground =
                        Foreground::Confirm(PendingConfirm::removing(task.id, question));
                }
            }
            // Enter reads what the task is asking. Taking over has a key of its
            // own, and asks first, because it ends the agent's autonomy.
            KeyCode::Enter => self.open_signals(task_service, remote_signals).await,
            // Moving a task across is one key: which way it goes is decided by
            // where the task already is, so there is nothing to remember.
            KeyCode::Char('p') => {
                let Some(task) = self.selected_task() else {
                    return Ok(false);
                };
                if task.facts.is_borrowed() {
                    // The way back is leaving the take over, not a key: this task is
                    // held here, and holding it is the only reason it is here.
                    let from = task
                        .facts
                        .arrived_from
                        .as_deref()
                        .map(|address| self.remote_name(address))
                        .unwrap_or_default();
                    self.say(format!(
                        "This task is held here from {from} until the take over ends; it goes back then"
                    ));
                    return Ok(false);
                }
                if self.transfer.is_some() {
                    self.say("A task is already on its way; let that finish first");
                    return Ok(false);
                }
                // A task goes where its project runs, and nowhere else. So this
                // either offers that one machine or says why there is no move.
                let Some(project) = task.project().cloned() else {
                    self.say("This task has no recorded project");
                    return Ok(false);
                };
                let hosts = match self.project_machine(&project) {
                    Ok(Some(address)) => self
                        .task_hosts()
                        .into_iter()
                        .filter(|host| host.address.as_deref() == Some(address.as_str()))
                        .collect::<Vec<_>>(),
                    Ok(None) => {
                        self.say(
                            "New tasks for this project start on this machine. Set their machine \
                             in Settings → Projects to delegate them.",
                        );
                        return Ok(false);
                    }
                    Err(reason) => {
                        self.say(reason);
                        return Ok(false);
                    }
                };
                let title = task.title().to_owned();
                let project_name = project.name.clone();
                let agent = task.agent_kind;
                self.foreground = Foreground::SendTask(Box::new(SendTask::new(
                    task.id,
                    title,
                    project_name,
                    agent,
                    hosts,
                )));
            }
            // The issue a task is bound to is a page, so it opens the same way a
            // sandbox's link does: through this machine's browser, by the
            // operator's own act.
            KeyCode::Char('b') => {
                if let Some(issue) = self
                    .selected_task()
                    .and_then(|task| task.facts.tracker_issue)
                {
                    match open_in_browser(&issue.url) {
                        Ok(()) => tracing::info!(%issue.url, "opened a task's issue"),
                        Err(error) => {
                            self.problem(format!("Could not open {}: {error}", issue.url))
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(false)
    }

    pub(in crate::tui) fn started_mouse(&mut self, mouse: MouseEvent) {
        let Foreground::GetStarted(mut panel) = mem::take(&mut self.foreground) else {
            return;
        };
        let Some(browser) = panel.repositories.as_mut() else {
            self.foreground = Foreground::GetStarted(panel);
            return;
        };
        if let Some(repository) = browser.mouse(mouse) {
            panel.repositories = None;
            self.bind_project_repository(&repository.url);
            panel.choose_repository(&repository);
            panel.focus = SetupFocus::Ready;
        }
        self.foreground = Foreground::GetStarted(panel);
    }

    pub(in crate::tui) async fn new_task_key<E>(
        &mut self,
        key: KeyEvent,
        event_loop: &TaskEventLoop<E>,
        task_service: &TaskService<E>,
        issue_reads: &mpsc::UnboundedSender<IssueReadReply>,
        delegations: &mpsc::Sender<TaskDelegationReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Foreground::NewTask(mut question) = mem::take(&mut self.foreground) else {
            return;
        };
        let answer = question.handle_key(key);
        self.resolve_new_task(
            question,
            answer,
            event_loop,
            task_service,
            issue_reads,
            delegations,
        )
        .await;
    }

    pub(in crate::tui) async fn new_task_mouse<E>(
        &mut self,
        mouse: MouseEvent,
        event_loop: &TaskEventLoop<E>,
        task_service: &TaskService<E>,
        issue_reads: &mpsc::UnboundedSender<IssueReadReply>,
        delegations: &mpsc::Sender<TaskDelegationReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Foreground::NewTask(mut question) = mem::take(&mut self.foreground) else {
            return;
        };
        let answer = question.mouse(mouse, &self.new_task_targets);
        self.resolve_new_task(
            question,
            answer,
            event_loop,
            task_service,
            issue_reads,
            delegations,
        )
        .await;
    }

    pub(in crate::tui) fn composer_key<E>(
        &mut self,
        key: KeyEvent,
        event_loop: &TaskEventLoop<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if key.code == KeyCode::Esc {
            self.signal_composer = None;
            return;
        }
        let newline = key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
        if key.code == KeyCode::Enter && !newline {
            let Some(composer) = self.signal_composer.take() else {
                return;
            };
            if composer.is_blank() {
                self.signal_composer = Some(composer);
                return;
            }
            let answer = composer.body.value().trim().to_owned();
            let task_id = composer.task_id;
            let signal_id = composer.signal_id;
            if self.remote_signals.is_some() {
                self.act_on_remote_signal(
                    signal_id.clone(),
                    RemoteSignalAction::Reply,
                    Some(answer.clone()),
                    remote_signals,
                );
            } else {
                event_loop.reply_signal(task_id, signal_id.clone(), answer.clone());
            }
            self.remember_signal_reply(&signal_id, answer);
            return;
        }
        if let Some(composer) = self.signal_composer.as_mut() {
            composer.body.handle_key(key);
        }
    }

    pub(in crate::tui) fn setup_key(&mut self, key: KeyEvent) {
        if is_dismiss(key) {
            // A provider's guide steps back to the list it came from; everything
            // else closes to the panel.
            let stays = match &mut self.foreground {
                Foreground::Setup(setup) => setup.back(),
                _ => false,
            };
            if !stays {
                let to_card = matches!(
                    &self.foreground,
                    Foreground::Setup(setup) if setup.returns_to_card()
                );
                if to_card {
                    self.foreground = Foreground::GetStarted(self.started_panel());
                } else {
                    self.foreground = Foreground::Panel;
                }
                self.alert = None;
            }
        } else if let Some(text) = match &self.foreground {
            Foreground::Setup(setup) if key.code == KeyCode::Char('c') => {
                setup.copy_text().map(str::to_owned)
            }
            _ => None,
        } {
            match copy_to_clipboard(text) {
                Ok(()) => self.say("Public key copied"),
                Err(error) => self.problem(format!("Could not copy the key: {error}")),
            }
        } else if let Foreground::Setup(setup) = &mut self.foreground {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') if setup.is_picker() => {
                    setup.move_selection(true)
                }
                KeyCode::Up | KeyCode::Char('k') if setup.is_picker() => {
                    setup.move_selection(false)
                }
                KeyCode::Enter if setup.is_picker() => setup.open_selected(),
                _ => setup.scroll(key),
            }
        }
    }

    pub(in crate::tui) fn help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | '?') => self.dismiss_keys(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.help_scroll = self.help_scroll.saturating_add(1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.help_scroll = self.help_scroll.saturating_sub(1)
            }
            KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(12),
            KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(12),
            KeyCode::Home => self.help_scroll = 0,
            KeyCode::End => self.help_scroll = u16::MAX,
            _ => {}
        }
    }

    pub(in crate::tui) fn dismiss_keys(&mut self) {
        if let Foreground::Keys(behind) = mem::take(&mut self.foreground) {
            self.foreground = *behind;
        }
    }
}
