//! Binding a task to a tracker issue, and reporting the task's life back.
//!
//! The binding lives in the task's history, so it survives a restart and travels
//! with a transfer. Reporting is best-effort and checkpointed by history
//! position: the coordinator calls [`TaskService::sync_tracker`] on an interval,
//! and a failed pass leaves its work queued for the next one instead of failing
//! the task. Provider credentials stay inside the provider; nothing here reads
//! or records them.

use crate::{
    agent::AgentKind,
    isolation::{Environment, IsolationEngine},
    tracker::{IssueSnapshot, TrackerError, TrackerProvider, TrackerStatus},
};

use super::{
    Task, TaskExecutionPauseReason, TaskHistoryEvent, TaskOperatorSignal, TaskOperatorSignalKind,
    TaskService, TaskServiceError,
};

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone,
{
    pub async fn create_from_issue(
        &self,
        issue: IssueSnapshot,
        environment: Environment,
        agent: AgentKind,
    ) -> Result<Task, TaskServiceError> {
        let intent = issue.intent();
        self.adopt(
            self.draft(intent, environment, agent, None, None),
            None,
            &[TaskHistoryEvent::TrackerIssueBound { issue }],
        )
        .await
    }

    /// Report every bound task's life to its tracker, as far as it has not been
    /// reported already.
    ///
    /// Safe to call repeatedly: work is derived from history and checkpointed, so
    /// a pass that finds nothing new writes nothing and a pass that fails leaves
    /// its work for the next one. One task's provider failure does not stop the
    /// others; the first failure is returned for the interface to show.
    pub async fn sync_tracker(&self) -> Result<(), TrackerError> {
        let Some(provider) = self.tracker.clone() else {
            return Ok(());
        };
        let tasks = self
            .list()
            .await
            .map_err(|error| TrackerError::new(error.to_string()))?;
        let mut first_error = None;
        for task in tasks {
            if task.facts.tracker_issue.is_none() {
                continue;
            }
            if let Err(error) = self.sync_task_tracker(provider.as_ref(), &task).await {
                tracing::warn!(task_id = %task.id, %error, "could not sync a task to its tracker");
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn sync_task_tracker(
        &self,
        provider: &dyn TrackerProvider,
        task: &Task,
    ) -> Result<(), TrackerError> {
        let reference = task
            .facts
            .tracker_issue
            .as_ref()
            .expect("callers only sync bound tasks");
        let events = self
            .store
            .events(&task.id)
            .await
            .map_err(|error| TrackerError::new(error.to_string()))?;
        let start = checkpoint(&events);
        if start >= events.len() {
            return Ok(());
        }
        let target = latest_status(&events);
        let reported = latest_status(&events[..start]);
        let status_changed = target.is_some() && target != reported;
        let comments: Vec<String> = events[start..]
            .iter()
            .filter_map(|(_, event)| comment(task, event))
            .collect();
        if !status_changed && comments.is_empty() {
            return Ok(());
        }
        // A task removed while this pass was reading is no longer work, so it is
        // not commented on or moved; its record stays.
        let active = self
            .store
            .is_active(&task.id)
            .await
            .map_err(|error| TrackerError::new(error.to_string()))?;
        if !active {
            return Ok(());
        }
        if let Some(status) = target.filter(|_| status_changed) {
            provider.status(reference, status).await?;
        }
        for body in &comments {
            if !self
                .store
                .is_active(&task.id)
                .await
                .map_err(|error| TrackerError::new(error.to_string()))?
            {
                return Ok(());
            }
            provider.comment(reference, body).await?;
        }
        self.store
            .append_events(
                &task.id,
                &[TaskHistoryEvent::TrackerSynced {
                    event_count: events.len(),
                }],
            )
            .await
            .map_err(|error| TrackerError::new(error.to_string()))?;
        Ok(())
    }
}

/// The history position the last checkpoint acknowledged, or the start.
///
/// The checkpoint records the position at which it was written, checkpoints
/// counted, so resuming there re-reads the checkpoint itself and nothing it
/// already covered.
fn checkpoint(events: &[(i64, TaskHistoryEvent)]) -> usize {
    events
        .iter()
        .rev()
        .find_map(|(_, event)| match event {
            TaskHistoryEvent::TrackerSynced { event_count } => Some(*event_count),
            _ => None,
        })
        .unwrap_or(0)
}

/// The lifecycle status the whole history implies, with the latest transition
/// winning.
///
/// Done is terminal: acceptance is the operator's judgment, and a later
/// execution event cannot un-accept it. Failure, cancellation, and shutdown or
/// transfer pauses imply no status of their own, so none of them can fake a
/// review or an ending.
fn latest_status(events: &[(i64, TaskHistoryEvent)]) -> Option<TrackerStatus> {
    let mut status = None;
    for (_, event) in events {
        match event {
            TaskHistoryEvent::AgentRunStarted { .. } => {
                if status != Some(TrackerStatus::Done) {
                    status = Some(TrackerStatus::InProgress);
                }
            }
            TaskHistoryEvent::ExecutionPaused {
                execution_pause_reason,
                ..
            } if *execution_pause_reason == TaskExecutionPauseReason::AwaitingOperator => {
                if status != Some(TrackerStatus::Done) {
                    status = Some(TrackerStatus::InReview);
                }
            }
            TaskHistoryEvent::OperatorSignalConfirmed { .. }
            | TaskHistoryEvent::TaskCompleted { .. } => status = Some(TrackerStatus::Done),
            _ => {}
        }
    }
    status
}

/// The comment a history event deserves, if any.
///
/// Only updates the agent raised for the operator and the wrap-up request are
/// mirrored. A decision is a question the local operator answers in the
/// interface, and a failure's text is not tracker content.
fn comment(task: &Task, event: &TaskHistoryEvent) -> Option<String> {
    let TaskHistoryEvent::OperatorSignalRaised { signal } = event else {
        return None;
    };
    match signal.kind {
        TaskOperatorSignalKind::Update => Some(update_comment(signal)),
        TaskOperatorSignalKind::InReview => Some(review_comment(task, signal)),
        TaskOperatorSignalKind::Decision | TaskOperatorSignalKind::Failure => None,
    }
}

fn update_comment(signal: &TaskOperatorSignal) -> String {
    format!(
        "**Altitude update: {}**\n\n{}\n\n_Altitude_",
        signal.title.trim(),
        signal.body.trim()
    )
}

/// The wrap-up request, with the work it reports on.
///
/// A review is where the contributions matter, so their URLs go here rather than
/// on every update.
fn review_comment(task: &Task, signal: &TaskOperatorSignal) -> String {
    let mut body = format!(
        "**Altitude finished the work: {}**\n\n{}",
        signal.title.trim(),
        signal.body.trim()
    );
    if !task.code_contributions.is_empty() {
        body.push_str("\n\nWork submitted:\n");
        for (index, contribution) in task.code_contributions.iter().enumerate() {
            if index > 0 {
                body.push('\n');
            }
            body.push_str("- ");
            body.push_str(&contribution.url);
        }
    }
    body.push_str("\n\n_Altitude_");
    body
}
