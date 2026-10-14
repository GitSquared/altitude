use crate::{
    agent::{AgentEngines, AgentError, AgentKind, mcp, tools::AgentToolRegistry},
    config::InstallationIdentity,
    isolation::{
        Environment, EnvironmentDescription, EnvironmentProgress, EnvironmentRecipe,
        IsolationEngine, IsolationError, ProfileMount, ProfileSummary, guest_path,
        private_directory,
    },
    tracker::TrackerProvider,
    vcs::GitHub,
};
use std::{
    collections::HashMap,
    error::Error,
    fmt, fs, io,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::{net::UnixListener, task::JoinHandle};
use uuid::Uuid;

use tools::TaskAgentTools;

mod agents;
mod capacity;
mod credential;
mod event_loop;
mod lifecycle;
mod signals;
mod state;
mod turns;
pub use credential::{CredentialShell, CredentialShellEvent, credential_shell_id};
pub use event_loop::{TaskEventLoop, TaskExecutionUpdate};
mod progress;
mod store;
mod tools;
mod tracker;
pub use state::*;
pub use store::{TaskDraft, TaskStore};

use progress::EnvironmentProgressRegistry;

pub struct TaskArrival {
    pub intent: String,
    pub environment: Environment,
    pub agent: AgentKind,
    pub conversation: Option<String>,
    pub history: Vec<(i64, TaskHistoryEvent)>,
    pub capsule: Option<String>,
    pub transfer_id: String,
    pub peer: String,
}

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
    /// Where each running task's agent channel listens.
    agent_channel_dir: PathBuf,
    agent_channels: Arc<Mutex<HashMap<String, AgentChannel>>>,
    /// The host-only tracker client, absent until one is configured.
    ///
    /// Held behind an `Arc` because it is shared with the tasks it reports on;
    /// an agent reaches it only through the read-only tool this service binds.
    tracker: Option<Arc<dyn TrackerProvider>>,
}

/// One task's agent channel: the listener serving its tools, and the socket it is
/// on.
struct AgentChannel {
    handle: JoinHandle<()>,
    path: PathBuf,
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
    AgentChannel(io::Error),
}
impl fmt::Display for TaskServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(error) => error.fmt(formatter),
            Self::Task(error) => error.fmt(formatter),
            Self::Isolation(error) => error.fmt(formatter),
            Self::InvalidReport(error) => write!(formatter, "invalid agent turn report: {error}"),
            Self::AgentChannel(error) => write!(formatter, "agent channel error: {error}"),
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
        agent_channel_dir: PathBuf,
    ) -> Self {
        Self {
            store,
            isolation,
            snapshots_dir,
            agents,
            identity,
            environment_progress: EnvironmentProgressRegistry::default(),
            agent_channel_dir,
            agent_channels: Arc::new(Mutex::new(HashMap::new())),
            tracker: None,
        }
    }

    /// Bind the host's tracker client to this service.
    ///
    /// Absent by default, so existing callers keep working without a tracker.
    pub fn with_tracker(mut self, tracker: Option<Arc<dyn TrackerProvider>>) -> Self {
        self.tracker = tracker;
        self
    }

    pub fn tracker(&self) -> Option<Arc<dyn TrackerProvider>> {
        self.tracker.clone()
    }

    /// The one place Altitude's subsystems are bound into an agent's tool set.
    ///
    /// Every agent-facing capability Altitude has is registered here and nowhere
    /// else, so this is what to read when asking what an agent can do. The MCP
    /// server serves whatever this returns and names no tool itself.
    fn agent_tools(&self, task_id: &str) -> AgentToolRegistry {
        let mut tools = AgentToolRegistry::new();
        tools.register(TaskAgentTools::new(
            self.store.clone(),
            task_id.to_owned(),
            self.tracker.clone(),
        ));
        tools
    }

    /// Open the channel this task's agent reaches Altitude through, and say
    /// where the sandbox finds it.
    ///
    /// Bound on every provisioning rather than kept: a restored capsule gets a
    /// new sandbox, and a vsock route is fixed when the sandbox is made.
    pub(super) fn bind_agent_channel(&self, task_id: &str) -> Result<PathBuf, TaskServiceError> {
        self.release_agent_channel(task_id);
        private_directory(&self.agent_channel_dir).map_err(TaskServiceError::Isolation)?;
        let path = self.agent_channel_dir.join(format!("{task_id}.sock"));
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).map_err(TaskServiceError::AgentChannel)?;
        let tools = self.agent_tools(task_id);
        let listening = task_id.to_owned();
        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let tools = tools.clone();
                        tokio::spawn(async move {
                            let (reader, writer) = stream.into_split();
                            if let Err(error) = mcp::serve_agent(tools, reader, writer).await {
                                tracing::debug!(%error, "an agent session ended");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, task_id = %listening, "the agent channel stopped listening");
                        break;
                    }
                }
            }
        });
        self.agent_channels
            .lock()
            .expect("agent channel lock poisoned")
            .insert(
                task_id.to_owned(),
                AgentChannel {
                    handle,
                    path: path.clone(),
                },
            );
        Ok(path)
    }

    /// Close a task's agent channel and remove the socket behind it.
    pub(super) fn release_agent_channel(&self, task_id: &str) {
        if let Some(channel) = self
            .agent_channels
            .lock()
            .expect("agent channel lock poisoned")
            .remove(task_id)
        {
            channel.handle.abort();
            let _ = fs::remove_file(&channel.path);
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
        self.recipes()
            .into_iter()
            .map(|recipe| ProfileSummary {
                profile: recipe.profile,
                display: recipe.display,
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

    /// The recipes a task is provisioned with, in the order it mounts them.
    fn recipes(&self) -> Vec<EnvironmentRecipe> {
        vec![
            self.agents.engine(AgentKind::Pi).environment_recipe(),
            self.agents.engine(AgentKind::Claude).environment_recipe(),
            GitHub::environment_recipe(),
        ]
    }

    /// What the workspace base says about itself, for an interface that
    /// describes environments rather than runs them.
    pub fn base_description(&self) -> Option<EnvironmentDescription> {
        self.isolation.base_description()
    }
}
