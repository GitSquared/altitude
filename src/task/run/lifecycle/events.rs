use super::*;

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub(super) fn stopped_events(&self, error: String) -> Vec<TaskHistoryEvent> {
        failure_events(error).to_vec()
    }
}

// A restart is the machine's news, not the agent's: it has to know its workspace
// moved under it, and the operator reads it in the record rather than in a question,
// because it asks nothing of anybody.
pub(super) fn restart_events(task: &Task, said: &str) -> Vec<TaskHistoryEvent> {
    notice_event(
        task,
        &format!(
            "altitude on {} restarted and is picking this task back up. {said}",
            task.host
        ),
    )
}

// What a restart left this task doing, in the words the operator reads: a task that
// was waiting on a person or on a pull request is not "resuming" anything.
// The last thing the task's history said it was doing: a run that started and never
// ended is a run the machine stopped in the middle of.
// Whether a run was in flight, which is the only thing a restart needs to know
// about a history it is picking up. The two run markers answer it and nothing else
// does: a pause is written after the run whose end it explains, and the machine's
// own suspend writes one at the end of every history it touches.
pub(super) fn was_running(events: &[TaskHistoryEvent]) -> bool {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            TaskHistoryEvent::AgentRunStarted { .. } => Some(true),
            TaskHistoryEvent::AgentRunEnded { .. } => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

pub(super) fn woke_up_saying(task: &Task, interrupted: bool) -> String {
    if interrupted {
        return "Its run was cut in half by the restart, so it picks the work back up.".to_owned();
    }
    if task.facts.deferred_review.is_some() {
        return "It was waiting on its pull requests, and the machine is watching them again."
            .to_owned();
    }
    if task.facts.waits_on_operator() {
        return "It was waiting on the operator, and still is.".to_owned();
    }
    "Its sandbox was saved when the machine went down, so the work resumes where it left off."
        .to_owned()
}

pub(super) fn resumed(task: &Task, snapshot_path: String) -> TaskHistoryEvent {
    if task.facts.deferred_review.is_some() {
        TaskHistoryEvent::ExecutionPaused {
            execution_status: TaskExecutionStatus::Paused,
            sandbox_available: false,
            snapshot_available: true,
            execution_pause_reason: TaskExecutionPauseReason::AwaitingReviewChecks,
            snapshot_path,
        }
    } else if task.facts.waits_on_operator() {
        TaskHistoryEvent::ExecutionPaused {
            execution_status: TaskExecutionStatus::Paused,
            sandbox_available: false,
            snapshot_available: true,
            execution_pause_reason: TaskExecutionPauseReason::AwaitingOperator,
            snapshot_path,
        }
    } else {
        TaskHistoryEvent::ExecutionRecovered {
            execution_status: TaskExecutionStatus::Pending,
            sandbox_available: false,
            snapshot_available: true,
            snapshot_path,
        }
    }
}
