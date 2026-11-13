use super::events::{restart_events, resumed, was_running, woke_up_saying};
use super::*;

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub(crate) async fn release_resources(&self, task: &Task) -> Result<Task, TaskServiceError> {
        self.release_agent_channel(&task.id);
        self.release_ports(task);
        let mut task = task.clone();
        if task.facts.sandbox_available {
            let id = task
                .facts
                .sandbox_id
                .clone()
                .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
            if let Err(error) = self.isolation.remove(id).await {
                if !task.facts.accepted {
                    self.fail(&task, error.to_string()).await?;
                }
                return Err(TaskServiceError::Isolation(error));
            }
            task = self
                .record(
                    &task.id,
                    &[TaskHistoryEvent::SandboxRemoved {
                        sandbox_available: false,
                    }],
                )
                .await?;
        }
        if task.facts.snapshot_available {
            let path = task
                .facts
                .snapshot_path
                .as_ref()
                .expect("available snapshot has a recorded path");
            if let Err(error) = self.isolation.remove_snapshot(PathBuf::from(path)).await {
                if !task.facts.accepted {
                    self.fail(&task, error.to_string()).await?;
                }
                return Err(TaskServiceError::Isolation(error));
            }
            task = self
                .record(
                    &task.id,
                    &[TaskHistoryEvent::SnapshotRemoved {
                        snapshot_available: false,
                    }],
                )
                .await?;
        }
        Ok(task)
    }
    // A limit that clears puts a task back without anyone being committed to it: the
    // same shape as a restart resuming what it was running, on a shorter clock. The
    // task is Pending again, which is what the queue already knows how to start. A
    // hold with no clock on it is not a hold, so it comes back rather than staying
    // stopped with nothing to wake it.
    pub async fn resume_limits_due(&self, tasks: &[Task]) -> Vec<Result<Task, TaskServiceError>> {
        let due = tasks.iter().filter(|task| {
            task.facts.execution_status == TaskExecutionStatus::Paused
                && task.facts.execution_pause_reason
                    == Some(TaskExecutionPauseReason::AwaitingLimit)
                && task
                    .facts
                    .resumes_at_ms
                    .is_none_or(|clears_at_ms| clears_at_ms <= now_ms())
        });
        join_all(due.map(|task| {
            let service = self.clone();
            async move {
                let event = resumed(task, task.facts.snapshot_path.clone().unwrap_or_default());
                service.record(&task.id, &[event]).await
            }
        }))
        .await
    }

    pub async fn recover_all(&self) -> Vec<Result<Task, TaskServiceError>> {
        let tasks = match self.list().await {
            Ok(tasks) => tasks,
            Err(error) => return vec![Err(error)],
        };
        join_all(tasks.into_iter().filter(should_resume).map(|task| {
            let service = self.clone();
            async move {
                let mut task = task;
                if let Some(sandbox_id) = &task.facts.sandbox_id
                    && task.facts.sandbox_available
                {
                    match service
                        .isolation
                        .suspend(
                            sandbox_id.clone(),
                            snapshot_name(&task.id),
                            service.snapshots_dir.clone(),
                        )
                        .await
                    {
                        Ok(path) => {
                            service.release_agent_channel(&task.id);
                            let event = resumed(&task, path.to_string_lossy().into_owned());
                            task = service.record_snapshot(&task, &[event]).await?;
                        }
                        Err(error) if error.is_missing_sandbox() => {
                            // The machine went away under this task and its capsule
                            // with it: there is nothing to snapshot and nothing to
                            // resume from. Leaving it Running is how a task ends up
                            // claiming to work with nothing running behind it, so it
                            // is queued instead, and told what happened to it.
                            tracing::info!(
                                task_id = %task.id,
                                "a task's sandbox did not survive the restart"
                            );
                            let mut events = vec![TaskHistoryEvent::ExecutionQueued {
                                execution_status: TaskExecutionStatus::Pending,
                                wake: None,
                            }];
                            events.extend(restart_events(
                                &task,
                                "This task's sandbox did not survive the restart, so it starts \
                                 again from the project's repository.",
                            ));
                            task = service.record(&task.id, &events).await?;
                        }
                        Err(error) => {
                            service
                                .fail(
                                    &task,
                                    format!(
                                        "could not recover the task sandbox from {}: {error}",
                                        service.snapshots_dir.display()
                                    ),
                                )
                                .await?;
                            return Err(TaskServiceError::Isolation(error));
                        }
                    }
                } else if task.facts.snapshot_available
                    && task.facts.execution_pause_reason
                        == Some(TaskExecutionPauseReason::ApplicationShutdown)
                {
                    // The shutdown had already suspended the capsule, so there is
                    // nothing to stop and no live sandbox to stop it: the task was
                    // left holding a snapshot and nothing picked it up again. What the
                    // machine does now depends on what it cut in half: a task that was
                    // waiting is still waiting, and a task that was working has a turn
                    // to finish.
                    // A history that cannot be read is not a history that says the task
                    // was idle: saying nothing happened and saying nothing are the same
                    // answer here, and only one of them is honest.
                    let interrupted = match service.events(&task.id).await {
                        Ok(history) => {
                            let events: Vec<_> = history
                                .into_iter()
                                .map(|(_, event)| event)
                                .collect();
                            was_running(&events)
                        }
                        Err(error) => {
                            tracing::warn!(
                                task = %task.id,
                                %error,
                                "could not read a task's history to see what the restart cut in half"
                            );
                            false
                        }
                    };
                    let mut events = vec![if interrupted {
                        TaskHistoryEvent::ExecutionRecovered {
                            execution_status: TaskExecutionStatus::Pending,
                            sandbox_available: false,
                            snapshot_available: true,
                            snapshot_path: task.facts.snapshot_path.clone().unwrap_or_default(),
                        }
                    } else {
                        resumed(&task, task.facts.snapshot_path.clone().unwrap_or_default())
                    }];
                    events.extend(restart_events(&task, &woke_up_saying(&task, interrupted)));
                    task = service.record(&task.id, &events).await?;
                } else if task.facts.deferred_review.is_some() && task.facts.snapshot_available {
                    // A shutdown can catch a review between the hold being
                    // recorded and its capsule being suspended, which leaves it
                    // paused for the shutdown — a pause recovery resumes.
                    let mut events = vec![TaskHistoryEvent::ExecutionPaused {
                        execution_status: TaskExecutionStatus::Paused,
                        sandbox_available: false,
                        snapshot_available: true,
                        execution_pause_reason: TaskExecutionPauseReason::AwaitingReviewChecks,
                        snapshot_path: task.facts.snapshot_path.clone().unwrap_or_default(),
                    }];
                    events.extend(restart_events(&task, &woke_up_saying(&task, false)));
                    task = service.record(&task.id, &events).await?;
                }
                Ok(task)
            }
        }))
        .await
    }
    pub async fn events(
        &self,
        task_id: &str,
    ) -> Result<Vec<(i64, TaskHistoryEvent)>, TaskServiceError> {
        self.store
            .events(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn suspend_all(
        &self,
        tasks: Vec<Task>,
        reason: TaskExecutionPauseReason,
    ) -> Vec<Result<(), TaskServiceError>> {
        join_all(tasks.into_iter().filter(should_suspend).map(|task| {
            let service = self.clone();
            let reason = reason.clone();
            async move {
                let sandbox_id = task
                    .facts
                    .sandbox_id
                    .clone()
                    .expect("suspendable tasks have a sandbox");
                match service
                    .isolation
                    .suspend(
                        sandbox_id,
                        snapshot_name(&task.id),
                        service.snapshots_dir.clone(),
                    )
                    .await
                {
                    Ok(path) => {
                        let snapshot_path = path.to_string_lossy().into_owned();
                        let event = if task.facts.execution_status == TaskExecutionStatus::Failed {
                            TaskHistoryEvent::TaskSnapshotSaved {
                                snapshot_path,
                                sandbox_available: false,
                                snapshot_available: true,
                            }
                        } else {
                            TaskHistoryEvent::ExecutionPaused {
                                execution_status: TaskExecutionStatus::Paused,
                                sandbox_available: false,
                                snapshot_available: true,
                                execution_pause_reason: reason,
                                snapshot_path,
                            }
                        };
                        service.record_snapshot(&task, &[event]).await?;
                        service.release_agent_channel(&task.id);
                        Ok(())
                    }
                    Err(error) if error.is_missing_sandbox() => {
                        // There is no capsule to stop, so there is nothing to keep:
                        // the pause is recorded without one, and the next run builds
                        // it again.
                        service
                            .record(
                                &task.id,
                                &[TaskHistoryEvent::ExecutionPaused {
                                    execution_status: TaskExecutionStatus::Paused,
                                    sandbox_available: false,
                                    snapshot_available: task.facts.snapshot_available,
                                    execution_pause_reason: reason.clone(),
                                    snapshot_path: task
                                        .facts
                                        .snapshot_path
                                        .clone()
                                        .unwrap_or_default(),
                                }],
                            )
                            .await?;
                        Ok(())
                    }
                    // A machine that is going down has nothing to do about it: the
                    // task keeps the state it has, and the boot that follows works out
                    // what survived. Recording a failure here would end a task because
                    // its host restarted, and queuing it would hand work to a process
                    // that is about to be gone.
                    Err(error) if reason == TaskExecutionPauseReason::ApplicationShutdown => {
                        tracing::warn!(
                            task_id = %task.id,
                            %error,
                            "a task could not be saved before the machine stopped"
                        );
                        Ok(())
                    }
                    // Where it was writing is half of why it could not: a permission
                    // denied on one machine is a directory owned by another, and the
                    // operator is the only one who can look.
                    Err(error) => {
                        service
                            .fail(
                                &task,
                                format!(
                                    "could not save the task sandbox to {}: {error}",
                                    service.snapshots_dir.display()
                                ),
                            )
                            .await?;
                        Err(TaskServiceError::Isolation(error))
                    }
                }
            }
        }))
        .await
    }

    pub(crate) async fn record_snapshot(
        &self,
        task: &Task,
        events: &[TaskHistoryEvent],
    ) -> Result<Task, TaskServiceError> {
        let updated = self.record(&task.id, events).await?;
        if let Some(previous) = &task.facts.snapshot_path
            && task.facts.snapshot_available
            && updated.facts.snapshot_path.as_ref() != Some(previous)
            && let Err(error) = self
                .isolation
                .remove_snapshot(PathBuf::from(previous))
                .await
        {
            tracing::warn!(task_id = %task.id, %error, "could not remove superseded task snapshot");
        }
        Ok(updated)
    }

    pub(crate) async fn fail(&self, task: &Task, error: String) -> Result<Task, TaskServiceError> {
        tracing::error!(task_id = %task.id, %error, "task execution failed");
        let events = self.stopped_events(error);
        self.record(&task.id, &events).await
    }
}
