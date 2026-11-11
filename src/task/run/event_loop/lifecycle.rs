use std::sync::atomic::Ordering;

use crate::task::should_suspend;

use crate::{
    agent::AgentKind,
    config::InstallationIdentity,
    isolation::IsolationEngine,
    project::Project,
    task::{
        Task, TaskExecutionPauseReason, TaskExecutionStatus, TaskHistoryEvent, TaskServiceError,
    },
    tracker::{IssueSnapshot, TrackerStatus},
};

use super::{TaskEventLoop, TaskExecutionUpdate};

impl<E> TaskEventLoop<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub fn create(
        &self,
        prompt: String,
        project: Project,
        agent: AgentKind,
        requester: Option<InstallationIdentity>,
    ) {
        tracing::info!(agent = %agent, project = %project.name, "creating task");
        let event_loop = self.clone();
        tokio::spawn(async move {
            let task = match event_loop
                .service
                .create(prompt, project, agent, requester)
                .await
            {
                Ok(task) => task,
                Err(error) => {
                    let _ = event_loop
                        .events
                        .send(TaskExecutionUpdate::Created(Err(error)));
                    return;
                }
            };
            let _ = event_loop
                .events
                .send(TaskExecutionUpdate::Created(Ok(task.clone())));
            // A new task is not started here: it waits for a slot like everything
            // else, so a burst of them queues instead of all running at once.
            event_loop.admit();
        });
    }

    pub fn create_from_issue(&self, issue: IssueSnapshot, project: Project, agent: AgentKind) {
        tracing::info!(issue = %issue.issue.reference.identifier, "creating task from issue");
        let event_loop = self.clone();
        tokio::spawn(async move {
            let task = match event_loop
                .service
                .create_from_issue(issue, project, agent)
                .await
            {
                Ok(task) => task,
                Err(error) => {
                    let _ = event_loop
                        .events
                        .send(TaskExecutionUpdate::Created(Err(error)));
                    return;
                }
            };
            let _ = event_loop
                .events
                .send(TaskExecutionUpdate::Created(Ok(task)));
            event_loop.admit();
        });
    }

    pub fn reply_signal(&self, task_id: String, signal_id: String, body: String) {
        let event_loop = self.clone();
        tokio::spawn(async move {
            match event_loop
                .service
                .reply_operator_signal(&task_id, &signal_id, body)
                .await
            {
                Ok(()) => event_loop.admit(),
                Err(error) => event_loop.watch_agent_run(Err(error)),
            }
        });
    }

    pub fn settle_signal(&self, task_id: String, signal_id: String, body: String) {
        let event_loop = self.clone();
        tokio::spawn(async move {
            if let Err(error) = event_loop
                .service
                .reply_operator_signal(&task_id, &signal_id, body)
                .await
            {
                event_loop.watch_agent_run(Err(error));
                return;
            }
            if let Err(error) = event_loop
                .service
                .resolve_signal(&task_id, &signal_id, true)
                .await
            {
                event_loop.watch_agent_run(Err(error));
                return;
            }
            event_loop.admit();
        });
    }

    pub(super) fn detach_task(&self, task_id: &str) {
        self.terminal_output
            .lock()
            .expect("terminal output lock poisoned")
            .remove(task_id);
        self.runs
            .lock()
            .expect("task run lock poisoned")
            .remove(task_id);
        self.control_requests
            .lock()
            .expect("task control request lock poisoned")
            .remove(task_id);
    }

    pub fn remove(&self, task: Task) {
        let event_loop = self.clone();
        tokio::spawn(async move {
            let id = task.id.clone();
            let result = event_loop.remove_and_wait(task).await;
            if let Err(error) = result {
                let _ = event_loop
                    .events
                    .send(TaskExecutionUpdate::Removed(id, Err(error)));
            }
        });
    }

    pub async fn remove_and_wait(&self, task: Task) -> Result<(), TaskServiceError> {
        self.detach_task(&task.id);
        let id = task.id.clone();
        self.service.remove(task).await?;
        let _ = self.events.send(TaskExecutionUpdate::Removed(id, Ok(())));
        Ok(())
    }

    pub async fn suspend(
        &self,
        task: Task,
        reason: TaskExecutionPauseReason,
    ) -> Result<(), TaskServiceError> {
        self.mark_suspending(&task.id);
        for result in self.service.suspend_all(vec![task], reason).await {
            result?;
        }
        Ok(())
    }

    pub async fn suspend_all(&self) -> Result<(), TaskServiceError> {
        self.admission_enabled.store(false, Ordering::Release);
        let tasks = self.list().await?;
        // The flag lives on the run it describes, so a task suspended now and
        // resumed later does not carry a stale reason for its next ending.
        for task in tasks.iter().filter(|task| should_suspend(task)) {
            self.mark_suspending(&task.id);
        }
        // Every task is attempted even when one cannot be stopped: most of the
        // work suspended is better than none, and the first failure is reported
        // once the rest have had their turn.
        let mut first_error = None;
        for result in self
            .service
            .suspend_all(tasks, TaskExecutionPauseReason::ApplicationShutdown)
            .await
        {
            if let Err(error) = result {
                tracing::error!(%error, "a task could not be suspended");
                first_error.get_or_insert(error);
            }
        }
        // The scheduler owns no task, but it does own the sandboxes it observes
        // through; they are this host's to dispose of like any capsule.
        self.watchers.shutdown().await;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub async fn record_status(
        &self,
        task_id: &str,
        status: TrackerStatus,
    ) -> Result<(), TaskServiceError> {
        let events = self.service.events(task_id).await?;
        self.service
            .record(
                task_id,
                &[TaskHistoryEvent::TrackerSynced {
                    event_count: events.len(),
                    status: Some(status),
                }],
            )
            .await
            .map(|_| ())
    }

    // The tracker is where the operator said this work is over, which is the same
    // ending accepting it in altitude would be: the task is done, not paused with a
    // reason someone has to interpret later.
    pub async fn complete_task(&self, task_id: &str) -> Result<(), TaskServiceError> {
        let Some(task) = self
            .list()
            .await?
            .into_iter()
            .find(|task| task.id == task_id)
        else {
            return Ok(());
        };
        if task.facts.execution_status == TaskExecutionStatus::Completed {
            return Ok(());
        }
        self.service.release_resources(&task).await?;
        self.service
            .record(
                &task.id,
                &[TaskHistoryEvent::TaskCompleted {
                    execution_status: TaskExecutionStatus::Completed,
                }],
            )
            .await?;
        let _ = self
            .events
            .send(TaskExecutionUpdate::Removed(task.id, Ok(())));
        Ok(())
    }

    pub async fn stop(
        &self,
        task: Task,
        reason: TaskExecutionPauseReason,
    ) -> Result<(), TaskServiceError> {
        if task.facts.execution_pause_reason.as_ref() == Some(&reason) {
            return Ok(());
        }
        if should_suspend(&task) {
            self.suspend(task, reason).await?;
            return Ok(());
        }
        self.service
            .record(
                &task.id,
                &[TaskHistoryEvent::ExecutionPaused {
                    execution_status: TaskExecutionStatus::Paused,
                    sandbox_available: task.facts.sandbox_available,
                    snapshot_available: task.facts.snapshot_available,
                    execution_pause_reason: reason,
                    snapshot_path: task.facts.snapshot_path.clone().unwrap_or_default(),
                }],
            )
            .await?;
        Ok(())
    }

    pub async fn relinquish(
        &self,
        task: Task,
        transfer_id: &str,
        recipient: &str,
    ) -> Result<(), TaskServiceError> {
        self.service
            .store
            .relinquish(&task.id, transfer_id, recipient)
            .await
            .map_err(TaskServiceError::Task)?;
        if let Err(error) = self.service.release_resources(&task).await {
            tracing::warn!(task = %task.id, %error, "transferred task storage needs cleanup");
        }
        self.starting
            .lock()
            .expect("starting lock poisoned")
            .remove(&task.id);
        let _ = self
            .events
            .send(TaskExecutionUpdate::Removed(task.id, Ok(())));
        Ok(())
    }

    pub async fn create_issue_for_operator(
        &self,
        issue: IssueSnapshot,
        project: Project,
        agent: AgentKind,
        operator: InstallationIdentity,
        delegation: Option<String>,
    ) -> Result<Task, TaskServiceError> {
        let task = self
            .service
            .create_from_issue_bound(issue, project, agent, Some(operator), delegation)
            .await?;
        let _ = self
            .events
            .send(TaskExecutionUpdate::Created(Ok(task.clone())));
        self.admit();
        Ok(task)
    }

    // The newest task on an issue that is still in play, which is the one the issue
    // is being worked on by. Every condition is asked of the same task: the list is
    // ordered by whatever moved last, so a task on another issue answering "which
    // task is this issue's" is the bug this shape had — a mention, a comment and a
    // reply all found nothing to wake whenever a more recent task was running.
    pub async fn work_for_issue(
        &self,
        issue_id: &str,
        delegations: &[&str],
    ) -> Result<Option<Task>, TaskServiceError> {
        Ok(self.list().await?.into_iter().find(|task| {
            task.facts
                .tracker_issue
                .as_ref()
                .is_some_and(|bound| bound.id == issue_id || bound.identifier == issue_id)
                && !task
                    .facts
                    .execution_pause_reason
                    .as_ref()
                    .is_some_and(TaskExecutionPauseReason::is_out_of_play)
                && (delegations.is_empty()
                    || task
                        .facts
                        .tracker_delegation
                        .as_deref()
                        .is_some_and(|held| delegations.contains(&held)))
        }))
    }
}
