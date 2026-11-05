use std::path::{Path, PathBuf};

use crate::remote::{TRANSCRIPT_FILE, WORK_PLAIN_FILE};
use crate::task::{TaskArrival, TaskError, TaskOperatorSignalKind, file_arrivals};

use crate::{
    agent::AgentKind,
    isolation::{
        CommandEvent, EnvironmentProgressSink, FileTransferSink, IsolationEngine, guest_absolute,
    },
    project::Project,
    task::{
        Task, TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus, TaskHistoryEvent,
        TaskServiceError,
    },
};

use super::TaskEventLoop;

impl<E> TaskEventLoop<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub async fn stage_transfer(
        &self,
        task_id: &str,
        transfer_id: &str,
        peer: &str,
    ) -> Result<Task, TaskServiceError> {
        let task = self
            .list()
            .await?
            .into_iter()
            .find(|task| task.id == task_id)
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        let is_staged =
            task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::Transfer);
        if is_staged && !task.facts.sandbox_available {
            return Ok(task);
        }
        if !is_staged
            && !self
                .starting
                .lock()
                .expect("starting lock poisoned")
                .insert(task.id.clone())
        {
            return Err(TaskServiceError::Task(TaskError::OperationRejected(
                "the task is still starting",
            )));
        }
        self.service
            .record(
                task_id,
                &[TaskHistoryEvent::TransferStaged {
                    transfer_id: transfer_id.to_owned(),
                    peer: peer.to_owned(),
                    execution_status: TaskExecutionStatus::Paused,
                    execution_pause_reason: TaskExecutionPauseReason::Transfer,
                    execution_mode: task.facts.execution_mode,
                    sandbox_available: task.facts.sandbox_available,
                    snapshot_available: task.facts.snapshot_available,
                    snapshot_path: task.facts.snapshot_path.clone(),
                }],
            )
            .await?;
        let mut capture = task;
        if is_staged {
            capture.facts.execution_status = TaskExecutionStatus::Running;
        }
        self.suspend(capture, TaskExecutionPauseReason::Transfer)
            .await?;
        let stopped = self
            .list()
            .await?
            .into_iter()
            .find(|task| task.id == task_id)
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        self.service
            .record(
                task_id,
                &[TaskHistoryEvent::TransferStaged {
                    transfer_id: transfer_id.to_owned(),
                    peer: peer.to_owned(),
                    execution_status: TaskExecutionStatus::Paused,
                    execution_pause_reason: TaskExecutionPauseReason::Transfer,
                    execution_mode: stopped.facts.execution_mode,
                    sandbox_available: false,
                    snapshot_available: stopped.facts.snapshot_available,
                    snapshot_path: stopped.facts.snapshot_path.clone(),
                }],
            )
            .await
    }

    pub async fn pack_task_files(
        &self,
        task: &Task,
        destination: &Path,
        progress: &dyn FileTransferSink,
    ) -> Result<(), TaskServiceError> {
        let owner =
            task.operator_id()
                .ok_or(TaskServiceError::Task(TaskError::OperationRejected(
                    "the task records no operator to pack for",
                )))?;
        let isolation = self
            .service
            .isolation
            .for_operator(owner)
            .map_err(TaskServiceError::Isolation)?;
        // A task that is not holding a sandbox is given one here before anything can
        // be read out of it, and on a cold machine that is a build, not a copy.
        tracing::info!(task = %task.id, "resuming a task's sandbox to pack its files");
        let resumed = self.service.provision(task.clone()).await?;
        let sandbox_id = resumed
            .facts
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        let packed = self
            .pack_into(
                &isolation,
                &resumed,
                sandbox_id.clone(),
                destination,
                progress,
            )
            .await
            .map(|_| ());
        // The sandbox was made to be read, so it goes once it has been, and the
        // task's record says so: a task left believing it has a sandbox that no
        // longer exists is one that fails the next time anything looks at it.
        //
        // When the pack failed it stays, because it holds the work the capsule never
        // got: what the agent has in that tree is the only copy of it, and a transfer
        // that ends without a capsule is not a reason to destroy it.
        if packed.is_err() {
            tracing::warn!(
                task = %task.id,
                sandbox_id = %sandbox_id,
                "a task's files could not be packed; its sandbox stays where the work is"
            );
            return packed;
        }
        if let Err(error) = isolation.remove(sandbox_id).await {
            tracing::warn!(%error, "could not remove the sandbox a task was packed from");
        }
        if let Err(error) = self
            .service
            .record(
                &task.id,
                &[TaskHistoryEvent::SandboxRemoved {
                    sandbox_available: false,
                }],
            )
            .await
        {
            tracing::warn!(task = %task.id, %error, "could not record that a packed sandbox is gone");
        }
        self.service.release_agent_channel(&task.id);
        packed
    }

    pub(super) async fn pack_into(
        &self,
        isolation: &E,
        task: &Task,
        sandbox_id: String,
        destination: &Path,
        progress: &dyn FileTransferSink,
    ) -> Result<(), TaskServiceError> {
        let project = task
            .project()
            .ok_or(TaskServiceError::Task(TaskError::MissingProject))?;
        if let Some(checkout) = project.checkout_path() {
            let checkout = guest_absolute(&checkout).map_err(TaskServiceError::Isolation)?;
            let mut excluded = Vec::new();
            for cache in &project.caches {
                excluded.push(guest_absolute(cache).map_err(TaskServiceError::Isolation)?);
            }
            isolation
                .collect_files(
                    sandbox_id.clone(),
                    checkout,
                    excluded,
                    destination.join(WORK_PLAIN_FILE),
                    progress,
                )
                .await
                .map_err(TaskServiceError::Isolation)?;
        }
        if let (Some(conversation), Some(kind)) =
            (&task.facts.agent_conversation_id, task.agent_kind)
        {
            let guest = self
                .service
                .agents
                .engine(kind)
                .conversation_transcript_path(conversation);
            // A conversation that has not written a transcript yet has nothing to
            // carry, and a move must not fail over the file it was never given: a
            // task that failed before its first turn still has work worth taking.
            // Asked of the filesystem rather than by running a program in there: the
            // guest is the agent's to have broken, and a task is not better off for
            // having its conversation left behind because `test` went missing.
            let exists = isolation
                .guest_regular_file_exists(sandbox_id.clone(), guest.clone())
                .await
                .map_err(TaskServiceError::Isolation)?;
            if exists {
                isolation
                    .collect_file(sandbox_id, guest, destination.join(TRANSCRIPT_FILE))
                    .await
                    .map_err(TaskServiceError::Isolation)?;
            }
        }
        Ok(())
    }

    pub async fn deliver_arrivals(
        &self,
        task: &Task,
        progress: &dyn FileTransferSink,
    ) -> Result<(), TaskServiceError> {
        let owner =
            task.operator_id()
                .ok_or(TaskServiceError::Task(TaskError::OperationRejected(
                    "the task records no operator to deliver for",
                )))?;
        let isolation = self
            .service
            .isolation
            .for_operator(owner)
            .map_err(TaskServiceError::Isolation)?;
        let sandbox_id = task
            .facts
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        self.service
            .deliver_arrivals(&isolation, task, &sandbox_id, progress)
            .await
    }

    pub async fn prepare_project(
        &self,
        project: &Project,
        agent: AgentKind,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<PathBuf, TaskServiceError> {
        self.service.prepare_project(project, agent, progress).await
    }

    pub async fn prepare_project_as_operator(
        &self,
        owner: &str,
        project: &Project,
        agent: AgentKind,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<PathBuf, TaskServiceError> {
        self.service
            .prepare_project_as_operator(owner, project, agent, progress)
            .await
    }

    pub async fn land(&self, arrival: TaskArrival) -> Result<Task, TaskServiceError> {
        let TaskArrival {
            intent,
            project,
            agent,
            conversation,
            history,
            files,
            transfer_id,
            peer,
        } = arrival;
        let task = self
            .service
            .adopt(
                self.service
                    .draft(intent, project, agent, conversation, None),
                Some(&history),
                &[TaskHistoryEvent::TransferStaged {
                    transfer_id: transfer_id.to_owned(),
                    peer: peer.to_owned(),
                    execution_status: TaskExecutionStatus::Paused,
                    execution_pause_reason: TaskExecutionPauseReason::Transfer,
                    execution_mode: TaskExecutionMode::Operator,
                    sandbox_available: false,
                    snapshot_available: false,
                    snapshot_path: None,
                }],
            )
            .await?;
        // Filed under the task's own id, where provisioning looks for them: the
        // sandbox does not exist yet, and this is the only place to put them until
        // it does.
        if let Some(files) = files {
            let arrivals = self.service.arrivals_dir(&task.id);
            match file_arrivals(&files, &arrivals) {
                Ok(()) => tracing::info!(
                    task = %task.id,
                    arrivals = %arrivals.display(),
                    "filed what this task arrived with"
                ),
                Err(error) => {
                    tracing::warn!(task = %task.id, %error, "could not keep the files this task arrived with")
                }
            }
        }
        Ok(task)
    }

    pub async fn abandon_transfer(
        &self,
        task_id: &str,
        transfer_id: &str,
    ) -> Result<(), TaskServiceError> {
        let task = self
            .list()
            .await?
            .into_iter()
            .find(|task| task.id == task_id)
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        self.service
            .record(
                task_id,
                &[TaskHistoryEvent::TransferAbandoned {
                    transfer_id: transfer_id.to_owned(),
                    execution_status: TaskExecutionStatus::Pending,
                    execution_pause_reason: None,
                    snapshot_available: task.facts.snapshot_available,
                    snapshot_path: task.facts.snapshot_path.clone(),
                }],
            )
            .await?;
        self.admit();
        Ok(())
    }

    pub async fn verify_landing(&self, task: Task) -> Result<Task, TaskServiceError> {
        let task = if task.facts.sandbox_available {
            task
        } else {
            self.service.provision(task).await?
        };
        let id = task
            .facts
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        let mut session = self
            .service
            .isolation
            .run_command(id, "true".to_owned())
            .await
            .map_err(TaskServiceError::Isolation)?;
        while let Some(event) = session.events.recv().await {
            match event {
                CommandEvent::Exited(0) => return Ok(task),
                CommandEvent::Output { .. } => {}
                _ => {
                    return Err(TaskServiceError::Task(TaskError::OperationRejected(
                        "the transferred sandbox did not boot",
                    )));
                }
            }
        }
        Err(TaskServiceError::Task(TaskError::MissingSandbox))
    }

    pub async fn finish_remote_transfer(
        &self,
        task_id: &str,
        transfer_id: &str,
    ) -> Result<(), TaskServiceError> {
        let blocked = self
            .service
            .unresolved_signal_threads(task_id)
            .await?
            .iter()
            .any(|thread| {
                matches!(
                    thread.kind,
                    TaskOperatorSignalKind::Blocked | TaskOperatorSignalKind::InReview
                )
            });
        self.service
            .record(
                task_id,
                &[TaskHistoryEvent::TransferLanded {
                    transfer_id: transfer_id.into(),
                    execution_status: if blocked {
                        TaskExecutionStatus::Paused
                    } else {
                        TaskExecutionStatus::Pending
                    },
                    execution_pause_reason: if blocked {
                        TaskExecutionPauseReason::AwaitingOperator
                    } else {
                        TaskExecutionPauseReason::Transfer
                    },
                }],
            )
            .await?;
        self.admit();
        Ok(())
    }

    pub async fn finish_transfer(
        &self,
        task_id: &str,
        transfer_id: &str,
    ) -> Result<(), TaskServiceError> {
        self.service
            .record(
                task_id,
                &[TaskHistoryEvent::TransferLanded {
                    transfer_id: transfer_id.to_owned(),
                    execution_status: TaskExecutionStatus::Paused,
                    execution_pause_reason: TaskExecutionPauseReason::AwaitingOperator,
                }],
            )
            .await?;
        Ok(())
    }

    // Only a take over makes a machine the desk a task is being worked at, and only
    // a desk hands the task back. A task moved to the machine its project runs on has
    // arrived home, and saying it came from somewhere would make it a second desk.
    pub async fn note_arrived_from(
        &self,
        task_id: &str,
        address: &str,
    ) -> Result<(), TaskServiceError> {
        self.service
            .record(
                task_id,
                &[TaskHistoryEvent::TaskArrivedFrom {
                    address: address.to_owned(),
                }],
            )
            .await?;
        Ok(())
    }
}
