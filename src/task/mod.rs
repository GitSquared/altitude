use crate::{
    agent::{AgentChannels, AgentEngines, AgentError, AgentKind, tools::AgentToolRegistry},
    config::{Config, InstallationIdentity},
    credentials::{Credentials, CredentialsError},
    isolation::{
        EnvironmentDescription, EnvironmentProgress, IsolationEngine, IsolationError, PortNotice,
        PortRelays, VsockRoute,
    },
    project::Project,
    tracker::{IssueReader, TrackerProvider},
    vcs::{self, Repository, VcsProviderKind, tools::VcsAgentTools},
};
use std::{collections::HashMap, error::Error, fmt, io, path::PathBuf, sync::Arc};
use tokio::sync::broadcast;
use uuid::Uuid;

use tools::TaskAgentTools;

mod agents;
mod capacity;
mod event_loop;
mod lifecycle;
mod signals;
mod state;
mod turns;
pub use event_loop::{TaskEventLoop, TaskExecutionUpdate};
mod progress;
mod review;
mod store;
mod tools;
mod tracker;
pub use state::*;
pub use store::{TaskDraft, TaskStore};
pub(crate) use tracker::{OwnTrackers, TrackerHost};

use progress::EnvironmentProgressRegistry;

pub struct TaskArrival {
    pub intent: String,
    pub project: Project,
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
    /// The operator used when this installation founds a task for itself.
    identity: InstallationIdentity,
    environment_progress: EnvironmentProgressRegistry,
    /// Where each running task's agent channel listens.
    agent_channels: AgentChannels,
    /// The loopback ports a taken-over task's terminal can reach.
    port_relays: PortRelays,
    /// The logins tasks run under: a separate service, because a login is not a task.
    credentials: Credentials<E>,
    trackers: Option<Arc<dyn TrackerHost>>,
    issue_reader: Option<Arc<dyn IssueReader>>,
    /// Told whenever a signal thread is written by an agent tool. The interface
    /// reads the change rather than asking the store on a clock: a tool write is
    /// not a task update, so nothing else would carry it to the screen.
    signal_changes: broadcast::Sender<String>,
}

fn failure_events(task: &Task, error: String, signal_id: String) -> [TaskHistoryEvent; 2] {
    let signal = TaskOperatorSignal {
        id: signal_id,
        task_id: task.id.clone(),
        kind: TaskOperatorSignalKind::Blocked,
        title: "Execution stopped".into(),
        body: format!(
            "{error}\n\nAltitude cannot continue this task. If ignored, it stays stopped.\n\nFix the problem first; take over the task with ctrl-enter if needed. Then reply here to retry with your instructions."
        ),
        raised_at_ms: chrono::Utc::now().timestamp_millis(),
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
    MissingProject,
}
impl fmt::Display for TaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "task database error: {error}"),
            Self::HistoryData(error) => write!(formatter, "invalid task history data: {error}"),
            Self::OperationRejected(reason) => formatter.write_str(reason),
            Self::MissingSandbox => formatter.write_str("task has no running sandbox"),
            Self::MissingAgent => formatter.write_str("task has no agent harness"),
            Self::MissingProject => formatter.write_str("task has no recorded project"),
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
    RepositoryListing(String),
    Credentials(CredentialsError),
}
impl fmt::Display for TaskServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(error) => error.fmt(formatter),
            Self::Task(error) => error.fmt(formatter),
            Self::Isolation(error) => error.fmt(formatter),
            Self::InvalidReport(error) => write!(formatter, "invalid agent turn report: {error}"),
            Self::AgentChannel(error) => write!(formatter, "agent channel error: {error}"),
            Self::RepositoryListing(error) => formatter.write_str(error),
            Self::Credentials(error) => error.fmt(formatter),
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
        config: Config,
        agents: AgentEngines,
        identity: InstallationIdentity,
    ) -> Self {
        let port_relays = PortRelays::new(config.agent_channel_dir());
        Self {
            store,
            snapshots_dir: config.task_snapshots_dir(),
            agent_channels: AgentChannels::new(config.agent_channel_dir()),
            port_relays: port_relays.clone(),
            credentials: Credentials::new(isolation.clone(), config, agents.clone(), port_relays),
            isolation,
            agents,
            identity,
            environment_progress: EnvironmentProgressRegistry::default(),
            trackers: None,
            issue_reader: None,
            // Small on purpose: a receiver that falls behind more than this has
            // missed changes it can read from the store anyway.
            signal_changes: broadcast::channel(32).0,
        }
    }

    /// A receiver told when a signal thread changes, so a screen showing one can
    /// read it instead of polling.
    pub fn subscribe_signals(&self) -> broadcast::Receiver<String> {
        self.signal_changes.subscribe()
    }

    /// One task as it reads now, for an interface keeping a card current.
    pub async fn task(&self, id: &str) -> Result<Option<Task>, TaskServiceError> {
        self.store.get(id).await.map_err(TaskServiceError::Task)
    }

    /// The logins tasks run under.
    pub fn credentials(&self) -> &Credentials<E> {
        &self.credentials
    }

    /// The repositories this machine's version-control login can reach.
    ///
    /// Asked from a project's settings, so it runs in the login's own build and
    /// never in a task's. Off the caller's loop: it may have to install the CLI
    /// the first time.
    pub async fn repositories(
        &self,
        kind: VcsProviderKind,
    ) -> Result<Vec<Repository>, TaskServiceError> {
        vcs::listing::repositories(&self.isolation, kind)
            .await
            .map_err(|error| TaskServiceError::RepositoryListing(error.to_string()))
    }

    /// Use the tracker each task's own operator pushed, for a machine serving
    /// other peers rather than reporting its own work.
    pub fn with_trackers(mut self, trackers: Arc<dyn TrackerHost>) -> Self {
        self.trackers = Some(trackers);
        self
    }

    pub fn with_issue_reader(mut self, reader: Option<Arc<dyn IssueReader>>) -> Self {
        self.issue_reader = reader;
        self
    }

    pub fn tracker(&self) -> Option<Arc<dyn TrackerProvider>> {
        self.tracker_for("trackers/linear")
    }

    /// The tracker identity a project route names, or the service default.
    pub fn tracker_for(&self, credential: &str) -> Option<Arc<dyn TrackerProvider>> {
        self.trackers
            .as_ref()
            .and_then(|host| host.for_credential(&self.identity.id, credential))
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
            self.issue_reader.clone(),
            self.signal_changes.clone(),
        ));
        tools.register(VcsAgentTools::new(self.store.clone(), task_id.to_owned()));
        tools
    }

    /// Open the channel this task's agent reaches Altitude through, and say
    /// where the sandbox finds it.
    ///
    /// Bound on every provisioning rather than kept: a restored capsule gets a
    /// new sandbox, and a vsock route is fixed when the sandbox is made.
    pub(super) fn bind_agent_channel(&self, task_id: &str) -> Result<PathBuf, TaskServiceError> {
        self.agent_channels
            .bind(task_id, self.agent_tools(task_id))
            .map_err(TaskServiceError::AgentChannel)
    }

    /// Close a task's agent channel and remove the socket behind it.
    pub(super) fn release_agent_channel(&self, task_id: &str) {
        self.agent_channels.release(task_id);
    }

    /// The relay channel a task's sandbox is made with, so that taking the task
    /// over can reach whatever its terminal starts listening on.
    pub(super) fn relay_channel(&self, sandbox_id: &str) -> VsockRoute {
        self.port_relays.channel(sandbox_id)
    }

    /// Bind the relay a task's terminal is about to be opened over.
    pub(super) fn bind_ports(&self, sandbox_id: &str) -> Result<(), TaskServiceError> {
        self.port_relays
            .bind(sandbox_id)
            .map(|_| ())
            .map_err(|error| TaskServiceError::Isolation(IsolationError::new(error.to_string())))
    }

    /// Hear about each port that becomes reachable, for an interface to show.
    pub fn port_notices(&self) -> broadcast::Receiver<PortNotice> {
        self.port_relays.subscribe()
    }

    /// Close the relay a task's terminal held, answering nothing when it held none.
    pub(super) fn release_ports(&self, task: &Task) {
        if let Some(sandbox_id) = &task.facts.sandbox_id {
            self.port_relays.release(sandbox_id);
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
    /// What the workspace base says about itself, for an interface that
    /// describes environments rather than runs them.
    pub fn base_description(&self) -> Option<EnvironmentDescription> {
        self.isolation.base_description()
    }
}
