use crate::{
    agent::{AgentEngines, AgentError, AgentKind},
    config::InstallationIdentity,
    isolation::{
        EnvironmentDescription, EnvironmentProgress, IsolationEngine, IsolationError, ProfileMount,
        ProfileSummary, guest_path,
    },
    vcs::GitHub,
};
use std::{collections::HashMap, error::Error, fmt, path::PathBuf};
use uuid::Uuid;

mod agents;
mod capacity;
mod event_loop;
mod lifecycle;
mod signals;
mod state;
mod transfer;
mod turns;
pub use event_loop::{TaskEventLoop, TaskExecutionUpdate};
mod progress;
mod store;
pub use state::*;
pub use store::TaskStore;

use progress::EnvironmentProgressRegistry;

#[derive(Clone)]
pub struct TaskService<E> {
    store: TaskStore,
    isolation: E,
    snapshots_dir: PathBuf,
    agents: AgentEngines,
    /// The identity this installation delegates under, recorded on every task it founds.
    ///
    /// Nothing reads it back yet: the task's history is where it belongs, and its
    /// consumers are the ones that will need to tell several operators' work
    /// apart.
    identity: InstallationIdentity,
    environment_progress: EnvironmentProgressRegistry,
}

fn failure_events(task: &Task, error: String) -> [TaskHistoryEvent; 2] {
    let signal = TaskOperatorSignal {
        id: Uuid::new_v4().to_string(),
        task_id: task.id.clone(),
        kind: TaskOperatorSignalKind::Failure,
        priority: TaskOperatorSignalPriority::High,
        title: "Execution stopped".into(),
        body: format!(
            "{error}\n\nAltitude cannot continue this task. If ignored, it stays stopped.\n\nFix the problem first; open the task's shell with Enter if needed. Then answer this signal to retry with your instructions."
        ),
        raised_at_ms: chrono::Utc::now().timestamp_millis(),
        read: false,
    };
    [
        TaskHistoryEvent::ExecutionFailed {
            execution_status: TaskExecutionStatus::Failed,
            error,
        },
        TaskHistoryEvent::OperatorSignalRaised { signal },
    ]
}

fn snapshot_name(task_id: &str) -> String {
    format!("task-{task_id}-{}", Uuid::new_v4())
}

fn run_ended(
    agent_run: &TaskAgentRun,
    outcome: TaskAgentRunOutcome,
    exit_code: Option<i32>,
) -> TaskHistoryEvent {
    TaskHistoryEvent::AgentRunEnded {
        agent_run_id: agent_run.id.clone(),
        outcome,
        exit_code,
    }
}

#[derive(Debug)]
pub enum TaskError {
    Database(sqlx::Error),
    HistoryData(serde_json::Error),
    OperationRejected(&'static str),
    MissingSandbox,
    MissingAgent,
}
impl fmt::Display for TaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "task database error: {error}"),
            Self::HistoryData(error) => write!(formatter, "invalid task history data: {error}"),
            Self::OperationRejected(reason) => formatter.write_str(reason),
            Self::MissingSandbox => formatter.write_str("task has no running sandbox"),
            Self::MissingAgent => formatter.write_str("task has no agent harness"),
        }
    }
}
impl Error for TaskError {}

#[derive(Debug)]
pub enum TaskServiceError {
    Agent(AgentError),
    Task(TaskError),
    Isolation(IsolationError),
    InvalidReport(String),
}
impl fmt::Display for TaskServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(error) => error.fmt(formatter),
            Self::Task(error) => error.fmt(formatter),
            Self::Isolation(error) => error.fmt(formatter),
            Self::InvalidReport(error) => write!(formatter, "invalid agent turn report: {error}"),
        }
    }
}
impl Error for TaskServiceError {}

// Execution policy consumes recorded values; the store does not infer these decisions.

/// Whether this task is waiting for a slot, or holding one.
///
/// A paused task holds nothing: pausing suspends the capsule to a snapshot, so
/// its resources are already back. What it wants is to be admitted again, which
/// is a separate question from how much the host will carry.
pub(crate) fn wants_a_slot(task: &Task) -> bool {
    matches!(
        task.facts.execution_status,
        TaskExecutionStatus::Pending | TaskExecutionStatus::Running
    )
}

fn should_resume(task: &Task) -> bool {
    matches!(
        task.facts.execution_status,
        TaskExecutionStatus::Pending | TaskExecutionStatus::Running
    ) || (task.facts.execution_status == TaskExecutionStatus::Paused
        && task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::ApplicationShutdown))
}

fn should_suspend(task: &Task) -> bool {
    task.facts.sandbox_available
        && matches!(
            task.facts.execution_status,
            TaskExecutionStatus::Running | TaskExecutionStatus::Failed
        )
}

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone,
{
    pub fn new(
        store: TaskStore,
        isolation: E,
        snapshots_dir: PathBuf,
        agents: AgentEngines,
        identity: InstallationIdentity,
    ) -> Self {
        Self {
            store,
            isolation,
            snapshots_dir,
            agents,
            identity,
            environment_progress: EnvironmentProgressRegistry::default(),
        }
    }
    /// What each task's environment build is doing, for an interface to show.
    ///
    /// Keyed by task, and only present while this service is building for it.
    pub fn environment_builds(&self) -> HashMap<String, EnvironmentProgress> {
        self.environment_progress.published()
    }
    pub async fn list(&self) -> Result<Vec<Task>, TaskServiceError> {
        self.store.list().await.map_err(TaskServiceError::Task)
    }
    /// Bring back what was running, and return the tasks that want a slot.
    ///
    /// Recovery is not starting. This puts a task's work back on disk and leaves
    /// it waiting, so the caller decides what the host can carry; starting here
    /// would run everything at once on the way up.
    /// Every profile a task mounts, so an interface can say what logins exist.
    ///
    /// Built from the same recipes a task is provisioned with, so this cannot
    /// drift from what is actually mounted.
    pub fn profiles(&self) -> Vec<ProfileSummary> {
        let recipes = [
            self.agents.engine(AgentKind::Pi).environment_recipe(),
            self.agents.engine(AgentKind::Claude).environment_recipe(),
            GitHub::environment_recipe(),
        ];
        recipes
            .into_iter()
            .map(|recipe| ProfileSummary {
                profile: recipe.profile,
                mounts: recipe
                    .directories
                    .iter()
                    .map(|directory| ProfileMount {
                        path: directory.relative_path,
                        guest: guest_path(&directory.guest_path),
                    })
                    .collect(),
            })
            .collect()
    }
    /// What the workspace base says about itself, for an interface that
    /// describes environments rather than runs them.
    pub fn base_description(&self) -> Option<EnvironmentDescription> {
        self.isolation.base_description()
    }
}
