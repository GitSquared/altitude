use std::{io, time::Instant};

use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use crate::{
    agent::{AgentStanding, AgentWindow},
    isolation::IsolationEngine,
    project::Project,
    task::{
        AskTransition, TaskAsk, TaskAskAuthor, TaskAskKind, TaskAskMessage, TaskEventLoop,
        TaskExecutionPauseReason, TaskExecutionStatus, TaskExecutionUpdate, TaskPlanStep,
        TaskService,
    },
    tui::popups,
    tui::screens,
    tui::widgets::format::money,
};

use crate::tui::widgets::list;
use popups::confirm::PendingConfirm;
use screens::shell::Shell;
use screens::signals::{Composer, SignalAction, SignalFilter, SignalFocus, SignalSort, actions};

use super::*;

impl App {
    pub(super) fn scroll_activity(&mut self, rows: i32) {
        let current = if self.signal_activity_scroll == u16::MAX {
            self.signal_activity_scroll_max
        } else {
            self.signal_activity_scroll
        };
        let next = (i32::from(current) + rows).clamp(0, i32::from(self.signal_activity_scroll_max));
        self.signal_activity_scroll = u16::try_from(next).unwrap_or(0);
    }

    pub(super) fn scroll_plan(&mut self, rows: i32) {
        let current = if self.signal_plan_scroll == u16::MAX {
            self.signal_plan_scroll_max
        } else {
            self.signal_plan_scroll
        };
        let next = (i32::from(current) + rows).clamp(0, i32::from(self.signal_plan_scroll_max));
        self.signal_plan_scroll = u16::try_from(next).unwrap_or(0);
    }

    pub(in crate::tui) fn page_plan(&mut self, up: bool) {
        let page = i32::from(self.signal_plan_rows.max(1));
        self.scroll_plan(if up { -page } else { page });
    }

    pub(in crate::tui) fn page_activity(&mut self, up: bool) {
        let page = i32::from(self.signal_activity_rows.max(1));
        self.scroll_activity(if up { -page } else { page });
    }

    pub(super) fn signal_task_running(&self) -> bool {
        if let Some(task) = self.signal_task() {
            return task.facts.execution_status == TaskExecutionStatus::Running;
        }
        self.signal_held()
            .is_some_and(|held| held.summary.status == TaskExecutionStatus::Running)
    }

    pub(super) fn scroll_signal(&mut self, rows: i32) {
        let current = if self.signal_scroll == u16::MAX {
            self.signal_scroll_max
        } else {
            self.signal_scroll
        };
        let next = (i32::from(current) + rows).clamp(0, i32::from(self.signal_scroll_max));
        self.signal_scroll = u16::try_from(next).unwrap_or(0);
        self.select_signal_message_at_scroll();
    }

    pub(super) fn select_signal_message_at_scroll(&mut self) {
        if self.signal_message_rows.is_empty() {
            return;
        }
        let current = if self.signal_scroll == u16::MAX {
            self.signal_scroll_max
        } else {
            self.signal_scroll
        };
        self.signal_message_cursor = self
            .signal_message_rows
            .iter()
            .position(|&(top, height)| current < top.saturating_add(height))
            .unwrap_or(self.signal_message_rows.len() - 1);
    }

    pub(in crate::tui) fn page_signal(&mut self, up: bool) {
        let page = i32::from(self.signal_viewport_rows.max(1));
        self.scroll_signal(if up { -page } else { page });
    }

    pub(super) fn reveal_signal_message(&mut self) {
        let Some((top, height)) = self
            .signal_message_rows
            .get(self.signal_message_cursor)
            .copied()
        else {
            return;
        };
        let current = if self.signal_scroll == u16::MAX {
            self.signal_scroll_max
        } else {
            self.signal_scroll
        };
        let viewport = self.signal_viewport_rows;
        let bottom = top.saturating_add(height);
        let next = if top < current {
            top
        } else if bottom > current.saturating_add(viewport) {
            bottom.saturating_sub(viewport).min(top)
        } else {
            return;
        };
        self.signal_scroll = next.min(self.signal_scroll_max);
    }

    pub(super) fn signal_held(&self) -> Option<&HeldTask> {
        let remote = self.remote_signals.as_ref()?;
        self.remote_tasks
            .iter()
            .find(|held| held.address == remote.address && held.summary.id == remote.task_id)
    }

    // The account the task's credential runs on: how much of each window is gone, and
    // how long ago that was true. A reading, not a state, so the age travels with it.
    pub(in crate::tui) fn signal_task_standing(&self) -> Option<&AgentStanding> {
        let standing = match self.signal_task() {
            Some(task) => task.facts.standing.as_ref(),
            None => self.signal_held()?.summary.standing.as_ref(),
        }?;
        (!standing.windows.is_empty()).then_some(standing)
    }

    // The account's limit, when one is holding this task: which of its windows ran
    // out and when it lets go. The reason is read with the time, never the time
    // alone, so a task that came back does not keep saying it is waiting.
    pub(in crate::tui) fn signal_task_limit(&self) -> Option<(AgentWindow, i64)> {
        if let Some(task) = self.signal_task() {
            if task.facts.execution_pause_reason != Some(TaskExecutionPauseReason::AwaitingLimit) {
                return None;
            }
            return Some((
                task.facts
                    .limit_window
                    .clone()
                    .unwrap_or(AgentWindow::Other),
                task.facts.resumes_at_ms?,
            ));
        }
        let held = self.signal_held()?.summary.clone();
        if held.pause_reason != Some(TaskExecutionPauseReason::AwaitingLimit) {
            return None;
        }
        Some((
            held.limit_window.unwrap_or(AgentWindow::Other),
            held.resumes_at_ms?,
        ))
    }

    // What the task has spent, in the harness's own accounting: the money, marked as
    // an estimate when the provider did not price it. A subscription's money is not
    // shown, because a harness prices those at list rates whether or not anyone is
    // paying.
    pub(in crate::tui) fn signal_task_spend(&self) -> Option<String> {
        let usage = match self.signal_task() {
            Some(task) => &task.facts.usage,
            None => &self.signal_held()?.summary.usage,
        };
        if usage.is_unpriced() || !usage.is_charged {
            return None;
        }
        Some(if usage.cost_from_provider {
            money(usage.cost_usd_micros)
        } else {
            format!("~{}", money(usage.cost_usd_micros))
        })
    }

    pub(in crate::tui) fn signal_agent_label(&self) -> String {
        let local = self.signal_task();
        let agent = local
            .and_then(|task| task.agent_kind)
            .or_else(|| self.signal_held().and_then(|held| held.summary.agent));
        let model = local
            .and_then(|task| task.facts.model.as_deref())
            .or_else(|| {
                self.signal_held()
                    .and_then(|held| held.summary.model.as_deref())
            });
        match (agent, model) {
            (Some(agent), Some(model)) => format!(
                "{}-{}",
                agent.to_string().to_ascii_lowercase(),
                model
                    .rsplit('/')
                    .next()
                    .unwrap_or(model)
                    .to_ascii_lowercase()
            ),
            (Some(agent), None) => agent.to_string(),
            (None, Some(model)) => model
                .rsplit('/')
                .next()
                .unwrap_or(model)
                .to_ascii_lowercase(),
            (None, None) => "the agent".to_owned(),
        }
    }

    pub(in crate::tui) fn signal_plan(&self) -> Vec<TaskPlanStep> {
        self.signal_task()
            .map(|task| task.facts.plan.clone().unwrap_or_default())
            .or_else(|| self.signal_held().map(|held| held.summary.plan.clone()))
            .unwrap_or_default()
    }

    // What identifies the task in the row it has in the list: its tracker issue and the
    // pull requests it opened. The list draws them as columns; the screen that has one
    // task in front of it says them in the same order.
    pub(in crate::tui) fn signal_task_identification(&self) -> Vec<String> {
        let (issue, pulls) = match self.signal_task() {
            Some(task) => (
                task.facts
                    .tracker_issue
                    .as_ref()
                    .map(|issue| issue.identifier.clone()),
                task.code_contributions
                    .iter()
                    .map(|pull| format!("#{}", pull.reference))
                    .collect::<Vec<_>>(),
            ),
            None => match self.signal_held() {
                Some(held) => (
                    held.summary
                        .tracker_issue
                        .as_ref()
                        .map(|issue| issue.identifier.clone()),
                    held.summary
                        .contributions
                        .iter()
                        .map(|pull| format!("#{}", pull.reference))
                        .collect::<Vec<_>>(),
                ),
                None => (None, Vec::new()),
            },
        };
        issue.into_iter().chain(pulls).collect()
    }

    pub(in crate::tui) fn signal_task_title(&self) -> String {
        if let Some(task) = self.signal_task() {
            return task.title().to_owned();
        }
        self.signal_held()
            .map_or_else(String::new, |held| held.summary.title.clone())
    }

    pub(super) fn signal_actions(&self) -> Vec<SignalAction> {
        self.signal_thread().map(actions).unwrap_or_default()
    }

    pub(in crate::tui) fn move_signal_focus(&mut self, forward: bool) {
        self.signal_focus = self.signal_focus_ring.next(self.signal_focus, forward);
        if self.signal_focus == SignalFocus::Detail {
            let len = self
                .signal_thread()
                .map_or(0, |thread| thread.messages.len());
            self.signal_message_cursor = self.signal_message_cursor.min(len.saturating_sub(1));
            self.reveal_signal_message();
        }
    }

    pub(super) fn reset_signal_message(&mut self) {
        self.signal_message_cursor = self
            .signal_thread()
            .map_or(0, |thread| thread.messages.len().saturating_sub(1));
        self.signal_scroll = u16::MAX;
        // A stream belongs to the task it came from: another task's place in its
        // own activity means nothing here.
        self.signal_activity_scroll = u16::MAX;
        self.signal_activity_scroll_max = 0;
        self.signal_plan_scroll = u16::MAX;
        self.signal_plan_scroll_max = 0;
        self.signal_message_flash = None;
    }

    pub(in crate::tui) fn expire_message_flash(&mut self) {
        // A row that never became anything is a lie after a while: the send either
        // came back (and was taken) or it failed and said so.
        self.pending_tasks
            .retain(|pending| pending.started.elapsed() < PENDING_PATIENCE);
        if self
            .signal_flash_started
            .is_some_and(|started| started.elapsed() >= MESSAGE_FLASH)
        {
            self.signal_message_flash = None;
            self.signal_flash_started = None;
        }
    }

    pub(in crate::tui) fn message_flash_is_lit(&self) -> bool {
        self.signal_flash_started.is_some_and(|started| {
            let elapsed = started.elapsed();
            elapsed < MESSAGE_FLASH
                && (elapsed.as_millis() / MESSAGE_FLASH_BLINK.as_millis()).is_multiple_of(2)
        })
    }

    pub(super) fn move_signal_message(&mut self, forward: bool) {
        let len = self
            .signal_thread()
            .map_or(0, |thread| thread.messages.len());
        if len == 0 {
            return;
        }
        self.signal_message_cursor = if forward {
            (self.signal_message_cursor + 1).min(len - 1)
        } else {
            self.signal_message_cursor.saturating_sub(1)
        };
        self.reveal_signal_message();
    }

    pub(super) fn copy_signal_message(&mut self) {
        let Some(body) = self
            .signal_thread()
            .and_then(|thread| thread.messages.get(self.signal_message_cursor))
            .map(|message| message.body.clone())
        else {
            return;
        };
        match copy_to_clipboard(body) {
            Ok(()) => {
                self.signal_message_flash = Some(self.signal_message_cursor);
                self.signal_flash_started = Some(Instant::now());
            }
            Err(error) => self.problem(format!("Could not copy the message: {error}")),
        }
    }

    pub(super) fn normalise_signal_focus(&mut self) {
        let actions = self.signal_actions().len();
        if matches!(self.signal_focus, SignalFocus::Action(index) if index >= actions) {
            self.signal_focus = SignalFocus::List;
        }
        if self.signal_focus == SignalFocus::Detail {
            let len = self
                .signal_thread()
                .map_or(0, |thread| thread.messages.len());
            self.signal_message_cursor = self.signal_message_cursor.min(len.saturating_sub(1));
            self.reveal_signal_message();
        }
    }

    pub(in crate::tui) fn apply_signal_view(&mut self) {
        let selected = self.signal_thread().map(|thread| thread.id.clone());
        let filter = self
            .signal_filter
            .chosen_value()
            .copied()
            .unwrap_or(SignalFilter::Unresolved);
        let sort = self
            .signal_sort
            .chosen_value()
            .copied()
            .unwrap_or(SignalSort::Oldest);
        let mut threads: Vec<_> = self
            .signal_all_threads
            .iter()
            .filter(|thread| filter.matches(thread))
            .cloned()
            .collect();
        threads.sort_by(|left, right| {
            // Open asks first, then the history; within each, the order the menu
            // asks for.
            let group = |thread: &TaskAsk| u8::from(thread.is_resolved());
            group(left).cmp(&group(right)).then_with(|| {
                let when = |thread: &TaskAsk| thread.opened_at_ms;
                match sort {
                    SignalSort::Oldest => when(left).cmp(&when(right)),
                    SignalSort::Newest => when(right).cmp(&when(left)),
                }
            })
        });
        self.signal_threads = threads;
        match selected.as_ref().and_then(|id| {
            self.signal_threads
                .iter()
                .position(|thread| &thread.id == id)
        }) {
            Some(index) => self.signals_list.select(index),
            None => self.signals_list.select(0),
        }
        self.normalise_signal_focus();
    }

    pub(in crate::tui) fn move_signal_cursor(&mut self, forward: bool) {
        self.signals_list.move_by(forward);
        self.reset_signal_message();
    }

    pub(in crate::tui) fn page_signal_cursor(&mut self, forward: bool) {
        self.signals_list.page_by(forward);
        self.reset_signal_message();
    }

    pub(in crate::tui) fn resolve_signal_locally(&mut self, signal_id: &str, resolved: bool) {
        if let Some(thread) = self
            .signal_all_threads
            .iter_mut()
            .find(|thread| thread.id == signal_id)
        {
            let at_ms = chrono::Utc::now().timestamp_millis();
            thread.resolved = resolved;
            thread.resolved_by = resolved.then_some(TaskAskAuthor::Operator);
            thread.resolved_at_ms = resolved.then_some(at_ms);
            // The line belongs in the conversation without waiting for the next
            // read: this is what the operator just did.
            thread.transitions.push(AskTransition {
                at_ms,
                resolved,
                by: TaskAskAuthor::Operator,
            });
        }
        self.apply_signal_view();
    }

    pub(in crate::tui) fn select_next_open_thread(&mut self) {
        let len = self.signal_threads.len();
        if len == 0 {
            return;
        }
        let start = self.signals_list.selected();
        for offset in 1..=len {
            let index = (start + offset) % len;
            if self.signal_threads[index].is_unresolved() {
                self.signals_list.select(index);
                self.reset_signal_message();
                return;
            }
        }
    }

    pub(in crate::tui) async fn reload_signals<E>(&mut self, task_service: &TaskService<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        if self.remote_signals.is_some() {
            return;
        }
        let Some(task_id) = self.signal_task_id.clone() else {
            return;
        };
        let Ok(threads) = task_service.signal_threads(&task_id).await else {
            return;
        };
        self.signal_all_threads = threads;
        self.apply_signal_view();
    }

    pub(in crate::tui) fn remember_signal_reply(&mut self, signal_id: &str, answer: String) {
        if let Some(thread) = self
            .signal_all_threads
            .iter_mut()
            .find(|thread| thread.id == signal_id)
        {
            thread.messages.push(TaskAskMessage {
                id: format!("m{}", thread.messages.len() + 1),
                author: TaskAskAuthor::Operator,
                body: answer,
                at_ms: chrono::Utc::now().timestamp_millis(),
            });
            // A message is the whole conversation: it reopens a resolved thread.
            thread.resolved = false;
            thread.resolved_by = None;
            thread.resolved_at_ms = None;
        }
        self.apply_signal_view();
        self.signal_message_cursor = self
            .signal_thread()
            .map_or(0, |thread| thread.messages.len().saturating_sub(1));
        self.signal_scroll = u16::MAX;
    }

    pub(in crate::tui) fn signal_task_project(&self) -> Option<Project> {
        let task_id = self.signal_task_id.as_deref()?;
        let project_id = self
            .tasks
            .iter()
            .find(|task| task.id == task_id)?
            .project()?
            .id
            .clone();
        self.settings_file
            .as_ref()?
            .settings()
            .project(&project_id)
            .cloned()
    }

    pub(super) async fn act_on_signal<E>(
        &mut self,
        action: SignalAction,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(thread) = self.signal_thread().cloned() else {
            return;
        };
        let remote = self.remote_signals.is_some();
        match action {
            // A remote task is not in this list, so it is not looked for there;
            // its reply travels to where it is.
            SignalAction::Reply => {
                if remote || self.tasks.iter().any(|task| task.id == thread.task_id) {
                    self.signal_composer = Some(Composer::new(thread.task_id, thread.id));
                    // The box takes room from the conversation, so the newest
                    // message stays the one just above it.
                    self.signal_scroll = u16::MAX;
                } else {
                    self.say("This task is no longer in the list");
                }
            }
            SignalAction::Resolve => {
                // A review ends on acceptance, which is the operator's act and a
                // different button; a question is closed and opened by them.
                if thread.kind != TaskAskKind::Steer {
                    return;
                }
                let resolved = !thread.resolved;
                if resolved == thread.resolved {
                    return;
                }
                if remote {
                    self.act_on_remote_signal(
                        thread.id,
                        RemoteSignalAction::Resolve(resolved),
                        None,
                        remote_signals,
                    );
                } else {
                    match task_service
                        .resolve_signal(&thread.task_id, &thread.id, resolved)
                        .await
                    {
                        Ok(task) => {
                            self.resolve_signal_locally(&thread.id, resolved);
                            self.replace_task(task);
                            if resolved {
                                self.select_next_open_thread();
                            }
                        }
                        Err(error) => self.problem(error.to_string()),
                    }
                }
            }
            SignalAction::Apply => {
                self.apply_project_proposal(&thread, remote_signals, event_loop)
                    .await;
            }
            SignalAction::Accept => {
                if thread.kind != TaskAskKind::Review || thread.resolved {
                    return;
                }
                let question = "Accept this work and close the task?\n\
                    The agent will be shut down and its sandbox cleared up."
                    .to_owned();
                self.foreground = Foreground::Confirm(PendingConfirm::confirming_signal(
                    thread.task_id,
                    thread.id,
                    question,
                ));
            }
        }
    }

    pub(super) async fn open_local_signals<E>(&mut self, task_service: &TaskService<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(task) = self.selected_task() else {
            return;
        };
        let task_id = task.id.clone();
        // The threads are this machine's now, whatever remote was open before.
        self.remote_signals = None;
        self.remote_signals_loading = false;
        let threads = match task_service.signal_threads(&task_id).await {
            Ok(threads) => threads,
            Err(error) => {
                self.problem(error.to_string());
                return;
            }
        };
        self.signal_task_id = Some(task_id);
        self.signal_all_threads = threads;
        self.signal_threads.clear();
        self.signals_list = list::List::default();
        self.apply_signal_view();
        self.signal_arrivals.clear();
        self.signal_focus = SignalFocus::List;
        self.signal_scroll = u16::MAX;
        self.signal_composer = None;
        self.signal_plan_open = false;
        self.foreground = Foreground::Signals;
        self.reset_signal_message();
    }

    pub(super) fn leave_signals(&mut self) {
        self.foreground = Foreground::Tasks;
        self.remote_signals = None;
        self.remote_signals_loading = false;
        self.signal_task_id = None;
        self.signal_all_threads.clear();
        self.signal_threads.clear();
        self.signal_arrivals.clear();
        self.signal_targets.clear();
        self.signal_composer = None;
        self.signal_plan_open = false;
        self.signal_focus = SignalFocus::List;
        self.signal_message_cursor = 0;
        self.signal_scroll = 0;
        self.signal_message_rows.clear();
        self.signal_scroll_max = 0;
        self.signal_viewport_rows = 0;
    }

    pub(in crate::tui) async fn apply<E>(
        &mut self,
        update: TaskExecutionUpdate,
        terminal: &mut DefaultTerminal,
        event_loop: &TaskEventLoop<E>,
    ) -> io::Result<()>
    where
        E: IsolationEngine + Clone + 'static,
    {
        match update {
            TaskExecutionUpdate::Created(Ok(task)) => {
                // A task that was just asked for is the one in view, whether it
                // was founded or an existing binding was reused for it.
                self.pending_tasks.clear();
                let id = task.id.clone();
                self.replace_task(task);
                self.pending_reveal = Some(TaskRef::Local(id));
            }
            TaskExecutionUpdate::Created(Err(error)) => {
                self.pending_tasks.clear();
                self.problem(error.to_string());
            }
            TaskExecutionUpdate::Started(Ok(task)) => self.replace_task(task),
            TaskExecutionUpdate::Started(Err(error)) => self.problem(error.to_string()),
            TaskExecutionUpdate::Finished(id, Ok(task)) if !self.is_removing(&id) => {
                self.activities.remove(&id);
                if self
                    .moving
                    .as_ref()
                    .is_some_and(|moving| moving.task_id == id)
                {
                    self.moving = None;
                }
                self.remove_shell(&id, event_loop);
                self.replace_task(task);
            }
            // A run's failure is the task's own news rather than the
            // interface's: the card turns red and says so, and the reason
            // belongs to the task rather than to the band under the list.
            TaskExecutionUpdate::Finished(id, Err(error)) if !self.is_removing(&id) => {
                tracing::warn!(task_id = %id, %error, "a task's run ended with an error");
            }
            TaskExecutionUpdate::Removed(id, Ok(())) => {
                self.activities.remove(&id);
                self.remove_task(&id);
            }
            TaskExecutionUpdate::Removed(id, Err(error)) => {
                self.cancel_removing(&id);
                show_task_error(self, &id, error);
            }
            TaskExecutionUpdate::Activity { task_id, activity } => {
                let lines = self.activities.entry(task_id).or_default();
                if lines.last() != Some(&activity) {
                    lines.push(activity);
                }
                let excess = lines.len().saturating_sub(Self::ACTIVITY_LINES);
                lines.drain(..excess);
            }
            TaskExecutionUpdate::ShellEnded { task_id } => self.pop_sandbox_shell(&task_id),
            TaskExecutionUpdate::ShellOutput { task_id, bytes } => {
                let id = ShellId::TaskShell(task_id);
                for reply in self.forward_shell_output(&id, bytes) {
                    self.send_shell_input(&id, reply, event_loop);
                }
            }
            TaskExecutionUpdate::Output {
                task_id,
                stream,
                bytes,
            } => {
                let _ = stream;
                let id = ShellId::Task(task_id);
                for reply in self.forward_shell_output(&id, bytes) {
                    self.send_shell_input(&id, reply, event_loop);
                }
            }
            TaskExecutionUpdate::HandedBack(task_id, outcome) => {
                if self
                    .moving
                    .as_ref()
                    .is_some_and(|moving| moving.task_id == task_id)
                {
                    self.moving = None;
                    self.transfer = None;
                }
                if let Err(error) = outcome {
                    self.problem(format!("The task could not go back: {error}"));
                }
            }
            TaskExecutionUpdate::HandingBack {
                task_id,
                address,
                transfer,
            } => {
                // The task layer started this one; the interface only watches it, and
                // says on the card which task is leaving so the list does not look as
                // though the task vanished.
                self.moving = Some(Moving {
                    task_id,
                    watching: transfer.clone(),
                });
                self.watch_transfer(
                    format!("Handing back to {}", self.remote_name(&address)),
                    address,
                    transfer,
                );
            }
            TaskExecutionUpdate::OperatorReady(task) => {
                let id = task.id.clone();
                let title = task_shell_title(&task);
                self.replace_task(task);
                let shell_id = ShellId::Task(id.clone());
                self.foreground = Foreground::Shell(shell_id.clone());
                self.alert = None;
                let (rows, cols) = screens::shell::size(self.shell_area(terminal.size()?.into()));
                let history = event_loop.cached_terminal_output(&id);
                event_loop.resize_terminal(&id, rows, cols);
                self.shells
                    .insert(shell_id.clone(), Shell::new(rows, cols, history, title));
                self.foreground = Foreground::Shell(shell_id);
            }
            _ => {}
        }
        // Any news about a task can be news about its threads: the agent may
        // have answered while the screen was open. The read is cheap and the
        // selection is kept by id, so this cannot move the ground under the
        // operator.
        if matches!(self.foreground, Foreground::Signals) {
            self.refresh_signal_threads(event_loop).await;
        }
        Ok(())
    }
}
