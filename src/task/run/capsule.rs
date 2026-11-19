use crate::{
    isolation::IsolationEngine,
    task::{
        Task, TaskExecutionPauseReason, TaskExecutionStatus, TaskHistoryEvent, TaskService,
        failure_events, snapshot_name,
    },
};

// What became of a task's sandbox when a turn stopped using it. Only `Saved` lets a
// task claim a snapshot: a resume from a path that was never written is a task nobody
// can pick up, and a tree that could not be archived is worth more on its host than in
// a claim that there is one.
pub(super) enum Capsule {
    // Archived to this path, with the sandbox released.
    Saved(String),
    // Still on this host with nothing archived: the work in it is the only copy.
    Kept { sandbox: String, error: String },
    // The engine says it is not there, so there is nothing left to keep.
    Gone { error: String },
}

impl Capsule {
    // The pause this capsule allows, if it allows one. A sandbox that is still there
    // and unarchived is not a pause — something may still be writing in it — and only
    // a failure says that honestly.
    pub(super) fn pause_event(
        &self,
        task: &Task,
        reason: TaskExecutionPauseReason,
    ) -> Option<TaskHistoryEvent> {
        let (sandbox_available, snapshot_available, snapshot_path) = match self {
            Self::Saved(path) => (false, true, path.clone()),
            // A sandbox that is gone does not take the workspace with it: the snapshot
            // an earlier pause kept is still where a resume starts, and saying it is
            // not would throw away the best copy this task has.
            Self::Gone { .. } => (
                false,
                task.facts.snapshot_available,
                task.facts.snapshot_path.clone().unwrap_or_default(),
            ),
            Self::Kept { .. } => return None,
        };
        Some(TaskHistoryEvent::ExecutionPaused {
            execution_status: TaskExecutionStatus::Paused,
            sandbox_available,
            snapshot_available,
            execution_pause_reason: reason,
            snapshot_path,
        })
    }

    // A stop or a pause that did not happen, with a workspace that would not be put
    // away. The opening sentence belongs to the caller: a turn that would not stop and
    // a task the tracker stopped are the same news about the work.
    pub(super) fn unpaused_events(&self, task: &Task, opening: &str) -> Vec<TaskHistoryEvent> {
        let mut events = self.workspace_events();
        events.extend(failure_events(format!(
            "{opening} {}",
            self.workspace_report(task)
        )));
        events
    }

    // Where the workspace is now, for the endings that are not a pause. Only a
    // snapshot that was just written is claimed as one; a sandbox that is still there
    // goes on saying so, and one the engine no longer has says that instead.
    pub(super) fn workspace_events(&self) -> Vec<TaskHistoryEvent> {
        match self {
            Self::Saved(path) => vec![TaskHistoryEvent::TaskSnapshotSaved {
                sandbox_available: false,
                snapshot_available: true,
                snapshot_path: path.clone(),
            }],
            Self::Kept { .. } => Vec::new(),
            Self::Gone { .. } => vec![TaskHistoryEvent::SandboxRemoved {
                sandbox_available: false,
            }],
        }
    }

    // What is left of the work, in the words the operator reads on the task row. A
    // failure is theirs to act on, so it says what survived and what a reply does.
    pub(super) fn workspace_report(&self, task: &Task) -> String {
        match self {
            Self::Saved(_) => {
                "Its workspace was saved, so replying to this task resumes it from there."
                    .to_owned()
            }
            Self::Kept { sandbox, error } => format!(
                "{error}, so it was kept as {sandbox} rather than thrown away; replying to this \
                 task resumes the work where it is."
            ),
            Self::Gone { error } if task.facts.snapshot_available => {
                format!("{error}; replying to this task resumes it from the snapshot it last kept.")
            }
            Self::Gone { error } => format!(
                "{error}; replying to this task starts it again from the project's repository."
            ),
        }
    }
}

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub(super) async fn suspend_capsule(&self, task: &Task) -> Capsule {
        // Everything this machine holds open in the guest is closed first. An agent
        // channel or a port relay is a session the guest is still serving, and a
        // guest with something to serve does not quiesce for a graceful stop — which
        // is what left a snapshot waiting on a shutdown that never came.
        self.release_agent_channel(&task.id);
        self.release_ports(task);
        let Some(sandbox_id) = task.facts.sandbox_id.clone() else {
            return Capsule::Gone {
                error: "It has no sandbox to save".to_owned(),
            };
        };
        // Before the sandbox is stopped: the session the turn refreshed lives in it, and it is
        // the only copy that was spent.
        self.keep_agent_session(task).await;
        // The sandbox has to be still before it is packed, or the archive reads a tree
        // that is still being written and the pause keeps nothing. A sandbox that will
        // not stop is not a reason to throw its tree away: it stays on the host that
        // made it, and the record says so.
        match self.isolation.stop_work(sandbox_id.clone()).await {
            Ok(()) => {}
            // An engine that cannot find the sandbox to stop has none to archive either:
            // this is an absence, not a failure to preserve one.
            Err(error) if error.is_missing_sandbox() => {
                return Capsule::Gone {
                    error: format!("Its sandbox is gone ({error})"),
                };
            }
            Err(error) => {
                tracing::error!(
                    task_id = %task.id,
                    %error,
                    "a task's sandbox did not stop; nothing is archived and nothing is released"
                );
                return Capsule::Kept {
                    sandbox: sandbox_id,
                    error: format!("Its sandbox did not stop ({error})"),
                };
            }
        }
        match self
            .isolation
            .suspend(
                sandbox_id.clone(),
                snapshot_name(&task.id),
                self.snapshots_dir.clone(),
            )
            .await
        {
            Ok(path) => Capsule::Saved(path.to_string_lossy().into_owned()),
            Err(error) if error.is_missing_sandbox() => Capsule::Gone {
                error: format!("Its sandbox is gone ({error})"),
            },
            // A host that did not answer is not a tree that is gone. Reporting it as
            // one would take the only copy of the work away from a task whose sandbox
            // may still be there, so the sandbox is kept and the failure says why.
            Err(error) => {
                tracing::error!(
                    task_id = %task.id,
                    snapshots = %self.snapshots_dir.display(),
                    %error,
                    "a task's sandbox could not be archived; it is kept rather than released"
                );
                Capsule::Kept {
                    sandbox: sandbox_id,
                    error: format!("Its sandbox could not be archived ({error})"),
                }
            }
        }
    }
}
