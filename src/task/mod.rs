use crate::{
    agent::{AgentChannels, AgentEngines, AgentError, AgentKind, tool::AgentToolRegistry},
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
use futures_util::future::BoxFuture;
use std::{
    collections::HashMap,
    collections::HashSet,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;

use crate::remote::{TRANSCRIPT_FILE, WORK_PLAIN_FILE};
use uuid::Uuid;

mod agent;
mod mirror;
pub mod picture;
mod run;
mod state;

use agent::tools::TaskAgentTools;
pub use run::event_loop::{ShellOpening, TaskEventLoop, TaskExecutionUpdate};

pub type HomeFuture = BoxFuture<'static, Result<(), String>>;

pub trait SendsTasksHome: Send + Sync {
    fn send_home(&self, task: Task) -> HomeFuture;
}
mod progress;
mod queue;
pub(crate) mod store;
pub use state::*;
pub use store::{TaskDraft, TaskStore};

pub(crate) use mirror::tracker;
pub(crate) use tracker::{OwnTrackers, TrackerHost};

use progress::EnvironmentProgressRegistry;

pub struct TaskArrival {
    pub intent: String,
    pub project: Project,
    pub agent: AgentKind,
    pub conversation: Option<String>,
    pub history: Vec<(i64, TaskHistoryEvent)>,
    pub files: Option<PathBuf>,
    pub transfer_id: String,
    pub peer: String,
}

#[derive(Clone)]
pub struct TaskService<E> {
    store: TaskStore,
    isolation: E,
    snapshots_dir: PathBuf,
    attachments_dir: PathBuf,
    agents: AgentEngines,
    identity: InstallationIdentity,
    environment_progress: EnvironmentProgressRegistry,
    agent_channels: AgentChannels,
    port_relays: PortRelays,
    credentials: Credentials<E>,
    trackers: Option<Arc<dyn TrackerHost>>,
    issue_reader: Option<Arc<dyn IssueReader>>,
    signal_changes: broadcast::Sender<String>,
    refused_status_syncs: Arc<Mutex<HashMap<String, usize>>>,
}

// What a task arrives with, filed under its own id until provisioning puts it in a
// sandbox. Moved rather than copied: this is the only copy of what came off the
// other machine, and a capsule can be hundreds of megabytes.
pub(crate) fn file_arrivals(source: &Path, arrivals: &Path) -> io::Result<()> {
    fs::create_dir_all(arrivals)?;
    for name in [WORK_PLAIN_FILE, TRANSCRIPT_FILE] {
        let from = source.join(name);
        if !from.is_file() {
            continue;
        }
        let to = arrivals.join(name);
        let _ = fs::remove_file(&to);
        fs::rename(&from, &to)?;
    }
    Ok(())
}

// A task that stopped is stopped, not asking anything: the thread it used to open
// made a state look like a question, so every resume had to remember to answer it.
// What an operator needs to know is the error, and the place they read it — the
// interface, or the session on the tracker — is the place that says it.
fn failure_events(error: String) -> [TaskHistoryEvent; 1] {
    [TaskHistoryEvent::ExecutionFailed {
        execution_status: TaskExecutionStatus::Failed,
        error,
    }]
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
            Self::AgentChannel(error) => write!(formatter, "agent channel error: {error}"),
            Self::RepositoryListing(error) => formatter.write_str(error),
            Self::Credentials(error) => error.fmt(formatter),
        }
    }
}
impl Error for TaskServiceError {}

// Execution policy consumes recorded values; the store does not infer these decisions.

pub(crate) use queue::{place_in_queue, tasks_waiting_for_room};

fn should_resume(task: &Task) -> bool {
    matches!(
        task.facts.execution_status,
        TaskExecutionStatus::Pending | TaskExecutionStatus::Running
    ) || (task.facts.execution_status == TaskExecutionStatus::Paused
        && matches!(
            task.facts.execution_pause_reason,
            Some(TaskExecutionPauseReason::ApplicationShutdown)
        ))
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
    E: IsolationEngine + Clone + 'static,
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
            attachments_dir: config.attachments_dir(),
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
            refused_status_syncs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn subscribe_signals(&self) -> broadcast::Receiver<String> {
        self.signal_changes.subscribe()
    }

    pub async fn task(&self, id: &str) -> Result<Option<Task>, TaskServiceError> {
        self.store.get(id).await.map_err(TaskServiceError::Task)
    }

    pub fn credentials(&self) -> &Credentials<E> {
        &self.credentials
    }

    pub async fn repositories(
        &self,
        kind: VcsProviderKind,
    ) -> Result<Vec<Repository>, TaskServiceError> {
        vcs::listing::repositories(&self.isolation, kind)
            .await
            .map_err(|error| TaskServiceError::RepositoryListing(error.to_string()))
    }

    pub fn with_trackers(mut self, trackers: Arc<dyn TrackerHost>) -> Self {
        self.trackers = Some(trackers);
        self
    }

    pub fn with_issue_reader(mut self, reader: Option<Arc<dyn IssueReader>>) -> Self {
        self.issue_reader = reader;
        self
    }

    pub fn tracker_connectable(&self, credential: &str) -> Option<Arc<dyn TrackerProvider>> {
        self.trackers
            .as_ref()
            .and_then(|host| host.connectable(&self.identity.id, credential))
    }

    pub fn tracker_for(&self, credential: &str) -> Option<Arc<dyn TrackerProvider>> {
        self.trackers
            .as_ref()
            .and_then(|host| host.for_credential(&self.identity.id, credential))
    }

    fn agent_tools(&self, task_id: &str) -> AgentToolRegistry {
        let mut tools = AgentToolRegistry::new();
        tools.register(TaskAgentTools::new(
            self.store.clone(),
            task_id.to_owned(),
            self.issue_reader.clone(),
            self.signal_changes.clone(),
            Some(Arc::new(self.isolation.clone())),
            self.attachments_dir.clone(),
        ));
        tools.register(VcsAgentTools::new(self.store.clone(), task_id.to_owned()));
        tools
    }

    // Pictures are named by the signals that carry them, so what nothing names is
    // nothing. Run when the application starts: it is the only moment every body is
    // known and none is being written.
    pub async fn sweep_pictures(&self) -> Result<usize, TaskServiceError> {
        let mut keep = HashSet::new();
        for task in self.list().await? {
            let threads = self
                .store
                .signal_threads(&task.id)
                .await
                .map_err(TaskServiceError::Task)?;
            for thread in threads {
                for message in thread.messages {
                    keep.extend(
                        picture::take(&message.body)
                            .1
                            .into_iter()
                            .map(|held| held.id),
                    );
                }
            }
        }
        picture::sweep(&self.attachments_dir, &keep).map_err(|_| {
            TaskServiceError::Task(TaskError::OperationRejected("could not sweep pictures"))
        })
    }

    pub(crate) fn bind_agent_channel(&self, task_id: &str) -> Result<PathBuf, TaskServiceError> {
        self.agent_channels
            .bind(task_id, self.agent_tools(task_id))
            .map_err(TaskServiceError::AgentChannel)
    }

    pub(crate) fn release_agent_channel(&self, task_id: &str) {
        self.agent_channels.release(task_id);
    }

    pub(crate) fn relay_channel(&self, sandbox_id: &str) -> VsockRoute {
        self.port_relays.channel(sandbox_id)
    }

    pub(crate) fn bind_ports(&self, sandbox_id: &str) -> Result<(), TaskServiceError> {
        self.port_relays
            .bind(sandbox_id)
            .map(|_| ())
            .map_err(|error| TaskServiceError::Isolation(IsolationError::new(error.to_string())))
    }

    pub fn port_notices(&self) -> broadcast::Receiver<PortNotice> {
        self.port_relays.subscribe()
    }

    pub(crate) fn release_ports(&self, task: &Task) {
        if let Some(sandbox_id) = &task.facts.sandbox_id {
            self.port_relays.release(sandbox_id);
        }
    }

    pub fn environment_builds(&self) -> HashMap<String, EnvironmentProgress> {
        self.environment_progress.published()
    }
    pub fn environment_step(&self, task_id: &str) -> Option<EnvironmentProgress> {
        self.environment_progress.step(task_id)
    }
    pub async fn list(&self) -> Result<Vec<Task>, TaskServiceError> {
        self.store.list().await.map_err(TaskServiceError::Task)
    }
    pub fn base_description(&self) -> Option<EnvironmentDescription> {
        self.isolation.base_description()
    }
}
