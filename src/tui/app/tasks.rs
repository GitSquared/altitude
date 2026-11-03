use std::{collections::HashMap, time::Instant};

use tokio::sync::mpsc;

use crate::{
    agent::Activity,
    isolation::IsolationEngine,
    remote::TaskOrigin,
    task::{Task, TaskEventLoop, TaskOperatorSignalAuthor, TaskOperatorSignalThread, TaskService},
    tracker::IssueReference,
    tui::popups,
    tui::screens,
};

use popups::new_task::{Answer, NewTask, SearchToken};
use screens::get_started::RemoteOffer;

use super::*;

impl App {
    pub(super) async fn resolve_new_task<E>(
        &mut self,
        question: Box<NewTask>,
        answer: Option<Answer>,
        event_loop: &TaskEventLoop<E>,
        task_service: &TaskService<E>,
        issue_reads: &mpsc::UnboundedSender<IssueReadReply>,
        delegations: &mpsc::Sender<TaskDelegationReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        match answer {
            None => self.foreground = Foreground::NewTask(question),
            // The question is dropped, and with it any read that had not yet
            // founded a task.
            Some(Answer::Cancel) => {}
            Some(Answer::Create {
                task,
                project,
                agent,
            }) => {
                // The tab follows the answer, so the task is where the operator
                // is looking. The project pill can name another one.
                self.focus_project(&project.id);
                match self.project_machine(&project) {
                    // A task for a remote is founded there, off this loop. Nothing
                    // is copied: the machine that runs it owns it.
                    Ok(Some(address)) => {
                        self.begin_delegation(
                            &address,
                            TaskOrigin::Intent(task),
                            &project.id,
                            agent,
                            delegations,
                        );
                    }
                    // Founded here, so whose work it is needs no saying. It is
                    // still a task that does not exist yet, so the list says so.
                    Ok(None) => {
                        self.pending_tasks.push(PendingTask {
                            label: first_line(&task),
                            address: None,
                            started: Instant::now(),
                        });
                        event_loop.create(task, *project, agent, None);
                    }
                    // The answer was fine; the machine is not. The question stays
                    // in front so the operator can fix it or answer differently.
                    Err(reason) => {
                        self.foreground = Foreground::NewTask(question);
                        self.say(reason);
                    }
                }
            }
            // The prompt stays in front, reading the issue: the operator sees the
            // wait, and can still dismiss it.
            Some(Answer::ReadIssue { token }) => {
                self.foreground = Foreground::NewTask(question);
                self.start_issue_read(token, task_service, issue_reads);
            }
        }
    }

    pub(super) fn start_issue_read<E>(
        &mut self,
        token: u64,
        task_service: &TaskService<E>,
        issue_reads: &mpsc::UnboundedSender<IssueReadReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(reference) = self.issue_submission_reference(token) else {
            return;
        };
        let tracker = self
            .tracker_credential()
            .and_then(|credential| task_service.tracker_for(&credential));
        let Some(tracker) = tracker else {
            if let Foreground::NewTask(question) = &mut self.foreground {
                question.read_failed(token, NO_TRACKER.to_owned());
            }
            return;
        };
        let replies = issue_reads.clone();
        tokio::spawn(async move {
            let result = tracker
                .read(&reference)
                .await
                .map_err(|error| error.to_string());
            let _ = replies.send(IssueReadReply { token, result });
        });
    }

    pub(super) fn issue_submission_reference(&self, token: u64) -> Option<IssueReference> {
        match &self.foreground {
            Foreground::NewTask(question) => question.submission_reference(token),
            _ => None,
        }
    }

    pub(in crate::tui) async fn issue_read_resolved<E>(
        &mut self,
        reply: IssueReadReply,
        event_loop: &TaskEventLoop<E>,
        delegations: &mpsc::Sender<TaskDelegationReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        match reply.result {
            Ok(snapshot) => {
                let submission = {
                    let Foreground::NewTask(question) = &mut self.foreground else {
                        return;
                    };
                    question.take_submission(reply.token)
                };
                let Some(submission) = submission else {
                    return;
                };
                self.focus_project(&submission.project.id);
                match self.project_machine(&submission.project) {
                    Ok(Some(address)) => {
                        self.begin_delegation(
                            &address,
                            TaskOrigin::Issue(Box::new(snapshot)),
                            &submission.project.id,
                            submission.agent,
                            delegations,
                        );
                        self.foreground = Foreground::Tasks;
                    }
                    Ok(None) => {
                        self.pending_tasks.push(PendingTask {
                            label: first_line(&snapshot.issue.title),
                            address: None,
                            started: Instant::now(),
                        });
                        event_loop.create_from_issue(
                            snapshot,
                            submission.project,
                            submission.agent,
                        );
                        self.foreground = Foreground::Tasks;
                    }
                    Err(reason) => {
                        self.say(reason);
                    }
                }
            }
            Err(error) => {
                if let Foreground::NewTask(question) = &mut self.foreground {
                    question.read_failed(reply.token, error);
                }
            }
        }
    }

    pub(in crate::tui) fn due_issue_search(&mut self) -> Option<(SearchToken, String)> {
        let Foreground::NewTask(question) = &mut self.foreground else {
            return None;
        };
        question.due_search(Instant::now())
    }

    pub(in crate::tui) fn issue_search_unavailable(
        &mut self,
        token: SearchToken,
        message: &'static str,
    ) {
        if let Foreground::NewTask(question) = &mut self.foreground {
            question.search_unavailable(token, message.to_owned());
        }
    }

    pub(in crate::tui) fn issue_search_resolved<E>(
        &mut self,
        reply: IssueSearchReply,
        task_service: &TaskService<E>,
        issue_reads: &mpsc::UnboundedSender<IssueReadReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let token = {
            let Foreground::NewTask(question) = &mut self.foreground else {
                return;
            };
            question.apply_search(reply.token, reply.result)
        };
        if let Some(token) = token {
            self.start_issue_read(token, task_service, issue_reads);
        }
    }

    pub(super) fn adopt_remote(&mut self, offer: &RemoteOffer) -> bool {
        let Some(file) = self.settings_file.as_mut() else {
            return false;
        };
        let mut proposed = file.settings().clone();
        proposed.add_remote(&offer.address);
        let saved = file.save(proposed);
        match saved {
            Ok(()) => {
                self.remote_status.remove(&offer.address);
                true
            }
            Err(error) => {
                self.problem(error.to_string());
                false
            }
        }
    }

    pub(in crate::tui) fn signal_thread(&self) -> Option<&TaskOperatorSignalThread> {
        self.signal_threads.get(self.signals_cursor.selected())
    }

    pub(in crate::tui) fn signal_task(&self) -> Option<&Task> {
        let id = self.signal_task_id.as_deref()?;
        self.tasks.iter().find(|task| task.id == id)
    }

    pub(in crate::tui) fn signal_agent_working(&self) -> bool {
        let Some(thread) = self.signal_thread() else {
            return false;
        };
        if thread.is_resolved()
            || thread.messages.last().map(|message| message.author)
                != Some(TaskOperatorSignalAuthor::Operator)
        {
            return false;
        }
        self.signal_task_running()
    }

    // How much of an agent's own stream is worth keeping: enough to read back
    // through, little enough that a task left running for hours stays cheap.
    pub(super) const ACTIVITY_LINES: usize = 200;

    pub(in crate::tui) fn signal_activity(&self) -> Option<&Vec<Activity>> {
        let task_id = self.signal_task_id.as_deref()?;
        self.activities.get(task_id)
    }

    pub(in crate::tui) async fn refresh_signal_threads<E>(&mut self, event_loop: &TaskEventLoop<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        if self.remote_signals.is_some() {
            return;
        }
        let Some(task_id) = self.signal_task_id.clone() else {
            return;
        };
        let selected = self.signal_thread().map(|thread| thread.id.clone());
        // What each thread looked like before this read, so a new message or a
        // new ending can be the one that moves on screen.
        let previous: HashMap<String, (usize, bool)> = self
            .signal_all_threads
            .iter()
            .map(|thread| (thread.id.clone(), (thread.messages.len(), thread.resolved)))
            .collect();
        let Ok(threads) = event_loop.signal_threads(&task_id).await else {
            return;
        };
        let grown = selected.as_ref().is_some_and(|id| {
            threads
                .iter()
                .find(|thread| &thread.id == id)
                .is_some_and(|thread| {
                    thread.messages.len() > previous.get(id).map_or(0, |before| before.0)
                })
        });
        let now = Instant::now();
        for thread in &threads {
            let changed = previous.get(&thread.id).is_none_or(|before| {
                before.0 != thread.messages.len() || before.1 != thread.resolved
            });
            if changed {
                self.signal_arrivals.insert(thread.id.clone(), now);
            }
        }
        self.signal_arrivals
            .retain(|id, _| threads.iter().any(|thread| &thread.id == id));
        self.signal_all_threads = threads;
        self.apply_signal_view();
        if grown {
            self.reset_signal_message();
        }
    }

    pub(in crate::tui) async fn signal_changed<E>(
        &mut self,
        task_id: &str,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if self.tasks.iter().any(|task| task.id == task_id)
            && let Ok(Some(task)) = event_loop.task(task_id).await
        {
            self.replace_task(task);
        }
        if matches!(self.foreground, Foreground::Signals)
            && self.signal_task_id.as_deref() == Some(task_id)
        {
            self.refresh_signal_threads(event_loop).await;
        }
    }

    pub(in crate::tui) async fn signal_changes_lagged<E>(&mut self, event_loop: &TaskEventLoop<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        if matches!(self.foreground, Foreground::Signals) && self.remote_signals.is_none() {
            self.refresh_signal_threads(event_loop).await;
        }
    }

    pub(in crate::tui) async fn open_signals<E>(
        &mut self,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if let Some(held) = self.selected_held().cloned() {
            self.open_remote_signals(held, remote_signals);
            return;
        }
        self.open_local_signals(task_service).await;
    }
}
