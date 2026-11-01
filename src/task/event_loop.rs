use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
};

use tokio::{sync::mpsc, time};

use crate::{
    agent::{AgentKind, TaskReportDecoder},
    config::InstallationIdentity,
    isolation::{
        Command, CommandEvent, CommandSession, FileTransferSink, IsolationEngine, OutputStream,
        guest_absolute,
    },
    project::Project,
    task::capacity::{HostCapacity, HostSampler, SAMPLE_INTERVAL},
    task::review::{
        Look, OBSERVATION_ATTEMPTS, REVIEW_POLL_INTERVAL, ReviewWaits, Watchers, unreadable,
        wake_reason,
    },
    task::{
        DeferredReview, Task, TaskAgentRequestedAction, TaskAgentRun, TaskExecutionMode,
        TaskExecutionPauseReason, TaskExecutionStatus, TaskHistoryEvent, TaskOperatorSignal,
        TaskOperatorSignalAuthor, TaskService, TaskServiceError,
    },
    tracker::{IssueSnapshot, TrackerStatus},
    vcs::{
        CodeContribution, ContributionObservation, ContributionOutlook, ContributionReadiness,
        now_ms,
    },
};

pub enum TaskExecutionUpdate {
    Created(Result<Task, TaskServiceError>),
    Started(Result<Task, TaskServiceError>),
    Finished(String, Result<Task, TaskServiceError>),
    Removed(String, Result<(), TaskServiceError>),
    Output {
        task_id: String,
        stream: OutputStream,
        bytes: Vec<u8>,
    },
    OperatorReady(Task),
    Report {
        task: Task,
    },
}

#[derive(Default)]
struct CachedTerminalOutput {
    chunks: VecDeque<Vec<u8>>,
    byte_count: usize,
}

impl CachedTerminalOutput {
    fn push(&mut self, bytes: &[u8]) {
        const LIMIT: usize = 2 * 1024 * 1024;
        let bytes = &bytes[bytes.len().saturating_sub(LIMIT)..];
        self.byte_count += bytes.len();
        self.chunks.push_back(bytes.to_vec());
        while self.byte_count > LIMIT {
            self.byte_count -= self
                .chunks
                .pop_front()
                .expect("nonempty output cache")
                .len();
        }
    }
}

#[derive(Clone)]
pub struct TaskEventLoop<E> {
    service: TaskService<E>,
    admission_enabled: Arc<AtomicBool>,
    events: mpsc::UnboundedSender<TaskExecutionUpdate>,
    runs: Arc<Mutex<HashMap<String, ActiveRun>>>,
    control_requests: Arc<Mutex<HashMap<String, RequestedControl>>>,
    terminal_output: Arc<Mutex<HashMap<String, CachedTerminalOutput>>>,
    capacity: Arc<Mutex<HostCapacity>>,
    starting: Arc<Mutex<HashSet<String>>>,
    watchers: Arc<Watchers<E>>,
    review_waits: Arc<ReviewWaits>,
}

struct ActiveRun {
    commands: mpsc::UnboundedSender<Command>,
    agent_run_id: String,
    suspending: bool,
}

struct RequestedControl {
    agent_run_id: String,
    requested_mode: TaskExecutionMode,
}

impl<E> TaskEventLoop<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub fn new(service: TaskService<E>) -> (Self, mpsc::UnboundedReceiver<TaskExecutionUpdate>) {
        Self::with_admission(service, true)
    }

    pub fn waiting_for_runtime(
        service: TaskService<E>,
    ) -> (Self, mpsc::UnboundedReceiver<TaskExecutionUpdate>) {
        Self::with_admission(service, false)
    }

    fn with_admission(
        service: TaskService<E>,
        enabled: bool,
    ) -> (Self, mpsc::UnboundedReceiver<TaskExecutionUpdate>) {
        let (events, receiver) = mpsc::unbounded_channel();
        let watchers = Arc::new(Watchers::new(service.clone()));
        let event_loop = Self {
            service,
            admission_enabled: Arc::new(AtomicBool::new(enabled)),
            events,
            runs: Arc::new(Mutex::new(HashMap::new())),
            control_requests: Arc::new(Mutex::new(HashMap::new())),
            terminal_output: Arc::new(Mutex::new(HashMap::new())),
            capacity: Arc::new(Mutex::new(HostCapacity::new())),
            starting: Arc::new(Mutex::new(HashSet::new())),
            watchers,
            review_waits: Arc::new(ReviewWaits::default()),
        };
        event_loop.watch_host_capacity();
        event_loop.watch_held_reviews();
        (event_loop, receiver)
    }

    pub fn capacity_limit(&self) -> usize {
        self.capacity
            .lock()
            .expect("capacity lock poisoned")
            .limit()
    }

    fn watch_host_capacity(&self) {
        let event_loop = self.clone();
        tokio::spawn(async move {
            let mut sampler = HostSampler::new();
            let mut tick = time::interval(SAMPLE_INTERVAL);
            loop {
                tick.tick().await;
                let demand = match event_loop.service.list().await {
                    Ok(tasks) => tasks
                        .iter()
                        .filter(|task| {
                            super::wants_a_slot(task) && task.facts.deferred_review.is_none()
                        })
                        .count(),
                    Err(error) => {
                        tracing::warn!(%error, "could not read tasks to measure demand");
                        continue;
                    }
                };
                // A host that cannot be read leaves the limit where it is.
                let Some(sample) = sampler.sample() else {
                    continue;
                };
                let limit = event_loop
                    .capacity
                    .lock()
                    .expect("capacity lock poisoned")
                    .observe(sample, demand);
                tracing::debug!(limit, demand, "observed host capacity");
                event_loop.admit();
            }
        });
    }

    pub fn enable_admission(&self) {
        self.admission_enabled.store(true, Ordering::Release);
        self.admit();
    }

    pub fn admit(&self) {
        if !self.admission_enabled.load(Ordering::Acquire) {
            return;
        }
        let event_loop = self.clone();
        tokio::spawn(async move { event_loop.admit_waiting().await });
    }

    async fn admit_waiting(&self) {
        if !self.admission_enabled.load(Ordering::Acquire) {
            return;
        }
        let tasks = match self.service.list().await {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "could not read tasks to admit work");
                return;
            }
        };
        let limit = self
            .capacity
            .lock()
            .expect("capacity lock poisoned")
            .limit();
        // A capsule that exists is a slot in use. A task being started has no
        // capsule yet but is about to want one, so it counts too.
        let occupied = tasks
            .iter()
            .filter(|task| task.facts.sandbox_available)
            .count();
        let taken = occupied + self.starting.lock().expect("starting lock poisoned").len();
        let room = limit.saturating_sub(taken);
        // A task with a held review is the scheduler's: admission must not start
        // an agent turn for it while the review is still waiting.
        let mut waiting: Vec<Task> = tasks
            .into_iter()
            .filter(|task| task.facts.execution_status == TaskExecutionStatus::Pending)
            .filter(|task| task.facts.deferred_review.is_none())
            .filter(|task| {
                !self
                    .starting
                    .lock()
                    .expect("starting lock poisoned")
                    .contains(&task.id)
            })
            .collect();
        // The list arrives newest first; the queue is served oldest first.
        waiting.reverse();
        let mut fresh = 0;
        for task in waiting {
            if !task.facts.sandbox_available {
                if fresh >= room {
                    continue;
                }
                fresh += 1;
            }
            if !self.admission_enabled.load(Ordering::Acquire) {
                break;
            }
            if !self
                .starting
                .lock()
                .expect("starting lock poisoned")
                .insert(task.id.clone())
            {
                continue;
            }
            let event_loop = self.clone();
            tokio::spawn(async move {
                let result = event_loop.service.start(task.clone()).await;
                event_loop.watch_agent_run(result);
                event_loop
                    .starting
                    .lock()
                    .expect("starting lock poisoned")
                    .remove(&task.id);
                // Provisioning may have freed or taken room for something else.
                event_loop.admit();
            });
        }
    }

    pub(crate) fn service(&self) -> &TaskService<E> {
        &self.service
    }

    pub async fn task(&self, task_id: &str) -> Result<Option<Task>, TaskServiceError> {
        self.service.task(task_id).await
    }

    pub async fn update_intent(
        &self,
        task_id: &str,
        instruction: String,
    ) -> Result<(), TaskServiceError> {
        let task = self
            .list()
            .await?
            .into_iter()
            .find(|task| task.id == task_id)
            .ok_or(TaskServiceError::Task(super::TaskError::OperationRejected(
                "this task is no longer active",
            )))?;
        if task.facts.accepted {
            return Err(TaskServiceError::Task(super::TaskError::OperationRejected(
                "this task has already been accepted",
            )));
        }
        self.suspend(task, TaskExecutionPauseReason::ApplicationShutdown)
            .await?;
        let task = self
            .list()
            .await?
            .into_iter()
            .find(|task| task.id == task_id)
            .ok_or(TaskServiceError::Task(super::TaskError::OperationRejected(
                "this task is no longer active",
            )))?;
        self.service
            .record(
                task_id,
                &[TaskHistoryEvent::IntentExpressed {
                    intent: format!(
                        "{}\n\nThe operator added these instructions:\n{}",
                        task.facts.intent, instruction
                    ),
                    project: task
                        .project()
                        .cloned()
                        .ok_or(TaskServiceError::Task(super::TaskError::MissingProject))?,
                    identity_id: task
                        .facts
                        .operator_id
                        .clone()
                        .ok_or(TaskServiceError::Task(super::TaskError::OperationRejected(
                            "this task has no operator",
                        )))?,
                    identity_label: task.facts.operator_label.clone().unwrap_or_default(),
                    agent_conversation_id: task.facts.agent_conversation_id.clone(),
                    execution_status: TaskExecutionStatus::Pending,
                    execution_mode: TaskExecutionMode::Autopilot,
                    operator_attention_level: task.facts.operator_attention_level,
                    sandbox_available: task.facts.sandbox_available,
                    snapshot_available: task.facts.snapshot_available,
                }],
            )
            .await?;
        self.admit();
        Ok(())
    }

    pub async fn signal_threads(
        &self,
        task_id: &str,
    ) -> Result<Vec<super::TaskOperatorSignalThread>, TaskServiceError> {
        self.service.signal_threads(task_id).await
    }

    pub async fn reply(
        &self,
        task_id: &str,
        signal_id: &str,
        answer: String,
    ) -> Result<(), TaskServiceError> {
        if let Some(run) = self
            .service
            .reply_operator_signal(task_id, signal_id, answer)
            .await?
        {
            self.watch_agent_run(Ok(run));
        }
        Ok(())
    }

    pub async fn express_input(&self, task_id: &str, body: String) -> Result<(), TaskServiceError> {
        if let Some(run) = self.service.express_input(task_id, body).await? {
            self.watch_agent_run(Ok(run));
        }
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

    pub async fn work_for_issue(
        &self,
        issue_id: &str,
        delegations: &[&str],
    ) -> Result<Option<Task>, TaskServiceError> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .find(|task| {
                task.facts.execution_pause_reason.is_none()
                    || !matches!(
                        task.facts.execution_pause_reason,
                        Some(
                            TaskExecutionPauseReason::TrackerEnded
                                | TaskExecutionPauseReason::StoppedInTracker
                                | TaskExecutionPauseReason::DelegationWithdrawn
                        )
                    )
            })
            .filter(|task| {
                task.facts
                    .tracker_issue
                    .as_ref()
                    .is_some_and(|bound| bound.id == issue_id || bound.identifier == issue_id)
            })
            .filter(|task| {
                delegations.is_empty()
                    || task
                        .facts
                        .tracker_delegation
                        .as_deref()
                        .is_some_and(|held| delegations.contains(&held))
            }))
    }

    pub async fn list(&self) -> Result<Vec<Task>, TaskServiceError> {
        self.service.list().await
    }

    pub fn cached_terminal_output(&self, task_id: &str) -> Vec<Vec<u8>> {
        self.terminal_output
            .lock()
            .expect("terminal output lock poisoned")
            .get(task_id)
            .map(|output| output.chunks.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn cache_terminal_output(&self, task_id: &str, bytes: &[u8]) {
        self.terminal_output
            .lock()
            .expect("terminal output lock poisoned")
            .entry(task_id.to_owned())
            .or_default()
            .push(bytes);
    }

    pub fn has_active_run(&self, task_id: &str) -> bool {
        self.runs
            .lock()
            .expect("task run lock poisoned")
            .contains_key(task_id)
    }

    pub fn send_terminal_input(&self, task_id: &str, bytes: Vec<u8>) {
        if let Some(run) = self
            .runs
            .lock()
            .expect("task run lock poisoned")
            .get(task_id)
        {
            let _ = run.commands.send(Command::Input(bytes));
        }
    }

    pub fn resize_terminal(&self, task_id: &str, rows: u16, cols: u16) {
        if let Some(run) = self
            .runs
            .lock()
            .expect("task run lock poisoned")
            .get(task_id)
        {
            let _ = run.commands.send(Command::Resize { rows, cols });
        }
    }

    pub fn take_control(&self, task_id: &str) -> bool {
        self.request_control(task_id, TaskExecutionMode::Operator)
    }

    pub fn start_operator_control(&self, task: Task) {
        let event_loop = self.clone();
        tokio::spawn(async move {
            let result = event_loop.service.start_operator(task).await;
            let operator_task = result.as_ref().ok().map(|(task, _, _)| task.clone());
            event_loop.watch_agent_run(result);
            if let Some(task) = operator_task {
                let _ = event_loop
                    .events
                    .send(TaskExecutionUpdate::OperatorReady(task));
            }
        });
    }

    pub fn return_to_autopilot(&self, task_id: &str) -> bool {
        self.request_control(task_id, TaskExecutionMode::Autopilot)
    }

    fn request_control(&self, task_id: &str, requested_mode: TaskExecutionMode) -> bool {
        let agent_run_id = {
            let runs = self.runs.lock().expect("task run lock poisoned");
            let Some(run) = runs.get(task_id) else {
                tracing::warn!(task_id = %task_id, ?requested_mode, "refusing a control request: the task has no run to receive it");
                return false;
            };
            run.agent_run_id.clone()
        };
        // Register before interrupting, so a run that ends on the interrupt cannot
        // terminate before its request is on record.
        self.control_requests
            .lock()
            .expect("task control request lock poisoned")
            .insert(
                task_id.to_owned(),
                RequestedControl {
                    agent_run_id: agent_run_id.clone(),
                    requested_mode,
                },
            );
        let service = self.service.clone();
        let id = task_id.to_owned();
        tokio::spawn(async move {
            if let Err(error) = service
                .request_control(&id, requested_mode, &agent_run_id)
                .await
            {
                tracing::error!(task_id = %id, %error, "could not record a control request");
            }
        });
        if let Some(run) = self
            .runs
            .lock()
            .expect("task run lock poisoned")
            .get(task_id)
        {
            let _ = run.commands.send(Command::Interrupt);
        }
        true
    }

    fn take_active_run(&self, task_id: &str) -> Option<ActiveRun> {
        self.runs
            .lock()
            .expect("task run lock poisoned")
            .remove(task_id)
    }

    fn take_control_request(&self, task_id: &str, agent_run_id: &str) -> Option<TaskExecutionMode> {
        let mut requests = self
            .control_requests
            .lock()
            .expect("task control request lock poisoned");
        match requests.remove(task_id) {
            Some(request) if request.agent_run_id == agent_run_id => Some(request.requested_mode),
            Some(_) => {
                tracing::debug!(task_id = %task_id, "dropping a control request addressed to an earlier run");
                None
            }
            None => None,
        }
    }

    fn mark_suspending(&self, task_id: &str) {
        if let Some(run) = self
            .runs
            .lock()
            .expect("task run lock poisoned")
            .get_mut(task_id)
        {
            run.suspending = true;
        }
    }

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
                Ok(Some(run)) => event_loop.watch_agent_run(Ok(run)),
                Ok(None) => {}
                Err(error) => event_loop.watch_agent_run(Err(error)),
            }
        });
    }

    pub fn settle_signal(&self, task_id: String, signal_id: String, body: String) {
        let event_loop = self.clone();
        tokio::spawn(async move {
            let run = match event_loop
                .service
                .reply_operator_signal(&task_id, &signal_id, body)
                .await
            {
                Ok(run) => run,
                Err(error) => {
                    event_loop.watch_agent_run(Err(error));
                    return;
                }
            };
            if let Err(error) = event_loop
                .service
                .resolve_signal(&task_id, &signal_id, true)
                .await
            {
                event_loop.watch_agent_run(Err(error));
                return;
            }
            if let Some(run) = run {
                event_loop.watch_agent_run(Ok(run));
            }
        });
    }

    fn detach_task(&self, task_id: &str) {
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

    pub async fn stop(
        &self,
        task: Task,
        reason: TaskExecutionPauseReason,
    ) -> Result<(), TaskServiceError> {
        if task.facts.execution_pause_reason.as_ref() == Some(&reason) {
            return Ok(());
        }
        if super::should_suspend(&task) {
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
            .ok_or(TaskServiceError::Task(super::TaskError::MissingSandbox))?;
        let is_staged =
            task.facts.execution_pause_reason == Some(super::TaskExecutionPauseReason::Transfer);
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
            return Err(TaskServiceError::Task(super::TaskError::OperationRejected(
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
                    execution_pause_reason: super::TaskExecutionPauseReason::Transfer,
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
            .ok_or(TaskServiceError::Task(super::TaskError::MissingSandbox))?;
        self.service
            .record(
                task_id,
                &[TaskHistoryEvent::TransferStaged {
                    transfer_id: transfer_id.to_owned(),
                    peer: peer.to_owned(),
                    execution_status: TaskExecutionStatus::Paused,
                    execution_pause_reason: super::TaskExecutionPauseReason::Transfer,
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
        let owner = task.operator_id().ok_or(TaskServiceError::Task(
            super::TaskError::OperationRejected("the task records no operator to pack for"),
        ))?;
        let isolation = self
            .service
            .isolation
            .for_operator(owner)
            .map_err(TaskServiceError::Isolation)?;
        let resumed = self.service.provision(task.clone()).await?;
        let sandbox_id = resumed
            .facts
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(super::TaskError::MissingSandbox))?;
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
        let outcome = packed;
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
        outcome
    }

    async fn pack_into(
        &self,
        isolation: &E,
        task: &Task,
        sandbox_id: String,
        destination: &Path,
        progress: &dyn FileTransferSink,
    ) -> Result<(), TaskServiceError> {
        let project = task
            .project()
            .ok_or(TaskServiceError::Task(super::TaskError::MissingProject))?;
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
                    destination.join("work"),
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
            isolation
                .collect_file(sandbox_id, guest, destination.join("transcript.jsonl"))
                .await
                .map_err(TaskServiceError::Isolation)?;
        }
        Ok(())
    }

    pub async fn deliver_arrivals(
        &self,
        task: &Task,
        progress: &dyn FileTransferSink,
    ) -> Result<(), TaskServiceError> {
        let owner = task.operator_id().ok_or(TaskServiceError::Task(
            super::TaskError::OperationRejected("the task records no operator to deliver for"),
        ))?;
        let isolation = self
            .service
            .isolation
            .for_operator(owner)
            .map_err(TaskServiceError::Isolation)?;
        let sandbox_id = task
            .facts
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(super::TaskError::MissingSandbox))?;
        self.service
            .deliver_arrivals(&isolation, task, &sandbox_id, progress)
            .await
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

    pub async fn prepare_project(
        &self,
        project: &Project,
        agent: AgentKind,
    ) -> Result<PathBuf, TaskServiceError> {
        self.service.prepare_project(project, agent).await
    }

    pub async fn prepare_project_as_operator(
        &self,
        owner: &str,
        project: &Project,
        agent: AgentKind,
    ) -> Result<PathBuf, TaskServiceError> {
        self.service
            .prepare_project_as_operator(owner, project, agent)
            .await
    }

    pub async fn land(&self, arrival: super::TaskArrival) -> Result<Task, TaskServiceError> {
        let super::TaskArrival {
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
                    execution_pause_reason: super::TaskExecutionPauseReason::Transfer,
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
            if let Err(error) = super::copy_tree(&files, &arrivals) {
                tracing::warn!(task = %task.id, %error, "could not keep the files this task arrived with");
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
            .ok_or(TaskServiceError::Task(super::TaskError::MissingSandbox))?;
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

    pub async fn verify_landing(&self, task: Task) -> Result<(), TaskServiceError> {
        let task = if task.facts.sandbox_available {
            task
        } else {
            self.service.provision(task).await?
        };
        let id = task
            .facts
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(super::TaskError::MissingSandbox))?;
        let mut session = self
            .service
            .isolation
            .run_command(id, "true".to_owned())
            .await
            .map_err(TaskServiceError::Isolation)?;
        while let Some(event) = session.events.recv().await {
            match event {
                CommandEvent::Exited(0) => return Ok(()),
                CommandEvent::Output { .. } => {}
                _ => {
                    return Err(TaskServiceError::Task(super::TaskError::OperationRejected(
                        "the transferred sandbox did not boot",
                    )));
                }
            }
        }
        Err(TaskServiceError::Task(super::TaskError::MissingSandbox))
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
                    super::TaskOperatorSignalKind::Blocked
                        | super::TaskOperatorSignalKind::InReview
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
                        super::TaskExecutionPauseReason::AwaitingOperator
                    } else {
                        super::TaskExecutionPauseReason::Transfer
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
                    execution_pause_reason: super::TaskExecutionPauseReason::AwaitingOperator,
                }],
            )
            .await?;
        Ok(())
    }

    pub async fn events(
        &self,
        task_id: &str,
    ) -> Result<Vec<(i64, TaskHistoryEvent)>, TaskServiceError> {
        self.service.events(task_id).await
    }

    pub async fn suspend_all(&self) -> Result<(), TaskServiceError> {
        self.admission_enabled.store(false, Ordering::Release);
        let tasks = self.list().await?;
        // The flag lives on the run it describes, so a task suspended now and
        // resumed later does not carry a stale reason for its next ending.
        for task in tasks.iter().filter(|task| super::should_suspend(task)) {
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

    fn watch_agent_run(
        &self,
        result: Result<(Task, TaskAgentRun, CommandSession), TaskServiceError>,
    ) {
        let (task, run, mut session) = match result {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(error = %error, "task failed to start");
                let _ = self.events.send(TaskExecutionUpdate::Started(Err(error)));
                return;
            }
        };
        self.terminal_output
            .lock()
            .expect("terminal output lock poisoned")
            .remove(&task.id);
        let _ = self
            .events
            .send(TaskExecutionUpdate::Started(Ok(task.clone())));
        self.runs.lock().expect("task run lock poisoned").insert(
            task.id.clone(),
            ActiveRun {
                commands: session.commands.clone(),
                agent_run_id: run.id.clone(),
                suspending: false,
            },
        );

        let mode = task.facts.execution_mode;
        let service = self.service.clone();
        let events = self.events.clone();
        let event_loop = self.clone();
        tokio::spawn(async move {
            let mut report_stream = match mode {
                TaskExecutionMode::Autopilot | TaskExecutionMode::AutopilotReport => {
                    service.report_decoder(&task)
                }
                _ => None,
            };
            loop {
                let event = session.events.recv().await.unwrap_or_else(|| {
                    CommandEvent::Failed(
                        "command event channel closed without an exit status".into(),
                    )
                });
                match event {
                    CommandEvent::Output { stream, bytes } => {
                        if let Some(decoder) = &mut report_stream {
                            decoder.process(&bytes);
                        }
                        event_loop.cache_terminal_output(&task.id, &bytes);
                        let _ = events.send(TaskExecutionUpdate::Output {
                            task_id: task.id.clone(),
                            stream,
                            bytes,
                        });
                    }
                    event => {
                        // This run's entry, its control request and its suspension
                        // reason are taken together: all three describe the run
                        // that is ending now and mean nothing to the next one.
                        let suspending = event_loop
                            .take_active_run(&task.id)
                            .is_some_and(|active| active.suspending);
                        let control = event_loop.take_control_request(&task.id, &run.id);
                        service.release_ports(&task);
                        if suspending {
                            tracing::debug!(task_id = %task.id, "command ended during intentional suspension");
                            return;
                        }
                        if let Err(error) = event_loop
                            .settle_finished_run(
                                task.clone(),
                                &run,
                                mode,
                                control,
                                report_stream,
                                event,
                            )
                            .await
                        {
                            let result = service
                                .finish(&task, &run, CommandEvent::Failed(error.to_string()), None)
                                .await;
                            let _ = events.send(TaskExecutionUpdate::Finished(task.id, result));
                        }
                        // A run ending is the main way a slot frees.
                        event_loop.admit();
                        return;
                    }
                }
            }
        });
    }

    fn watch_held_reviews(&self) {
        let event_loop = self.clone();
        tokio::spawn(async move {
            let mut tick = time::interval(REVIEW_POLL_INTERVAL);
            tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if event_loop.watchers.is_stopped() {
                    return;
                }
                // The runtime may not be ready yet; a held review waits for it
                // rather than being observed through a sandbox that cannot be
                // made.
                if !event_loop.admission_enabled.load(Ordering::Acquire) {
                    continue;
                }
                event_loop.observe_held_reviews().await;
            }
        });
    }

    async fn observe_held_reviews(&self) {
        let tasks = match self.service.store.list_held_reviews().await {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "could not read held reviews");
                return;
            }
        };
        // A task that failed is not watched, so it keeps no sandbox and its wait
        // is forgotten.
        let watching: Vec<&Task> = tasks
            .iter()
            .filter(|task| task.facts.execution_status != TaskExecutionStatus::Failed)
            .collect();
        let owners: HashSet<String> = watching
            .iter()
            .filter_map(|task| watcher_key(task))
            .collect();
        self.watchers.retain(&owners).await;

        let watched: HashSet<String> = watching.iter().map(|task| task.id.clone()).collect();
        let due = self.review_waits.due(&watched).await;
        let waiting: Vec<&Task> = watching
            .into_iter()
            .filter(|task| due.contains(&task.id))
            .collect();
        // One request per owner covers every review that owner is waiting on, so
        // there is no per-review budget to keep: the pass is bounded by how many
        // owners have something due, and each of those is asked once.
        let mut owner_names: Vec<String> = waiting
            .iter()
            .filter_map(|task| watcher_key(task))
            .collect();
        owner_names.sort();
        owner_names.dedup();
        for owner in owner_names {
            if self.watchers.is_stopped() {
                return;
            }
            let theirs: Vec<&Task> = waiting
                .iter()
                .copied()
                .filter(|task| watcher_key(task).as_deref() == Some(owner.as_str()))
                .collect();
            self.observe_owner(&owner, &theirs).await;
        }
    }

    async fn observe_owner(&self, owner: &str, tasks: &[&Task]) {
        // A capsule that outlived its hold is given back before observing, so
        // watching never competes with running work.
        let mut watching = Vec::with_capacity(tasks.len());
        for task in tasks {
            if !task.facts.sandbox_available {
                watching.push((*task).clone());
                continue;
            }
            match self.service.hold_review_checks(task, None).await {
                Ok(task) => watching.push(task),
                Err(error) => {
                    tracing::warn!(task_id = %task.id, %error, "could not release a task capsule while watching its pull requests");
                }
            }
        }
        // A pull request linked to two reviews is one pull request, asked about
        // once.
        let mut contributions: Vec<CodeContribution> = Vec::new();
        for task in &watching {
            for contribution in &task.code_contributions {
                if !contributions.iter().any(|seen| seen.id == contribution.id) {
                    contributions.push(contribution.clone());
                }
            }
        }
        let observations = match self.watchers.look(owner, &contributions).await {
            Look::Read(observations) => observations,
            // Nothing to decide from: either the provider was not asked, or it was
            // asked and could not be read. A provider that cannot be read must not
            // be mistaken for a finished check, and the owner's backoff decides
            // when either is tried again.
            Look::Unreadable | Look::NotAsked => return,
            // Only the operator can sign a provider in again, so the reviews this
            // owner is holding become theirs.
            Look::GivenUp => {
                let reason = self
                    .watchers
                    .last_failure(owner)
                    .await
                    .unwrap_or_else(|| "it did not say".to_owned());
                for task in &watching {
                    if let Some(deferred) = task.facts.deferred_review.clone() {
                        self.abandon_review(task, &deferred, &reason).await;
                    }
                }
                return;
            }
        };
        for task in &watching {
            self.decide_held_review(task, &observations).await;
        }
    }

    async fn decide_held_review(&self, task: &Task, observations: &[ContributionObservation]) {
        let Some(deferred) = task.facts.deferred_review.clone() else {
            return;
        };
        let mine: Vec<ContributionObservation> = observations
            .iter()
            .filter(|observation| {
                task.code_contributions
                    .iter()
                    .any(|contribution| contribution.id == observation.contribution_id)
            })
            .cloned()
            .collect();
        let Some((task, moved)) = self.record_observations(task, &mine).await else {
            return;
        };
        // The operator can take over or remove a task while a pass is in flight;
        // acting on a hold that is gone would raise a signal nobody owns.
        match self.service.store.is_kept(&task.id).await {
            Ok(true) => {}
            Ok(false) | Err(_) => return,
        }
        if task
            .facts
            .deferred_review
            .as_ref()
            .map(|hold| &hold.signal.id)
            != Some(&deferred.signal.id)
        {
            return;
        }
        self.review_waits.looked(&task.id, moved).await;
        self.resolve_held_review(&task, &deferred, &mine).await;
    }

    async fn record_observations(
        &self,
        task: &Task,
        observations: &[ContributionObservation],
    ) -> Option<(Task, bool)> {
        match self
            .service
            .store
            .record_code_contribution_observations(&task.id, observations)
            .await
        {
            Ok(recorded) => Some(recorded),
            Err(error) => {
                tracing::warn!(task_id = %task.id, %error, "could not record PR observations");
                None
            }
        }
    }

    async fn resolve_held_review(
        &self,
        task: &Task,
        deferred: &DeferredReview,
        observations: &[ContributionObservation],
    ) {
        let outlook = ContributionOutlook::of(observations, now_ms());
        match outlook.readiness {
            ContributionReadiness::NeedsAction => {
                if self
                    .wake_review(task, deferred, wake_reason(&outlook.needing_action))
                    .await
                {
                    // The task is queued; admission decides whether the host has
                    // room, so a review that needs work does not jump the queue.
                    self.admit();
                }
            }
            ContributionReadiness::Ready => {
                self.deliver_review(task, deferred.signal.clone()).await
            }
            // Checks are still running. The wait is already recorded, and the
            // review's own cadence decides when it is worth looking again.
            ContributionReadiness::Pending => {}
        }
    }

    async fn abandon_review(&self, task: &Task, deferred: &DeferredReview, reason: &str) {
        tracing::warn!(
            task_id = %task.id,
            attempts = OBSERVATION_ATTEMPTS,
            %reason,
            "giving up on linked contributions"
        );
        self.deliver_review(task, unreadable(&deferred.signal, reason))
            .await;
    }

    async fn deliver_review(&self, task: &Task, signal: TaskOperatorSignal) {
        let events = [
            TaskHistoryEvent::OperatorSignalRaised { signal },
            TaskHistoryEvent::ExecutionPaused {
                execution_status: TaskExecutionStatus::Paused,
                sandbox_available: false,
                snapshot_available: task.facts.snapshot_available,
                execution_pause_reason: super::TaskExecutionPauseReason::AwaitingOperator,
                snapshot_path: task.facts.snapshot_path.clone().unwrap_or_default(),
            },
        ];
        let result = self.service.record(&task.id, &events).await;
        match &result {
            Ok(_) => tracing::info!(task_id = %task.id, "delivered a held review request"),
            Err(error) => {
                tracing::warn!(task_id = %task.id, %error, "could not deliver a held review request")
            }
        }
        let _ = self
            .events
            .send(TaskExecutionUpdate::Finished(task.id.clone(), result));
    }

    async fn wake_review(&self, task: &Task, deferred: &DeferredReview, reason: String) -> bool {
        let events = [
            TaskHistoryEvent::ReviewDeferralCleared {
                signal_id: deferred.signal.id.clone(),
                reason,
            },
            // The review never reached the operator, and the agent is being woken
            // to do the work it was asking about. Whatever thread the hold was
            // continuing ends here; the next request opens a new one.
            TaskHistoryEvent::OperatorSignalResolved {
                signal_id: deferred.signal.id.clone(),
                resolved: true,
                by: TaskOperatorSignalAuthor::Agent,
            },
            TaskHistoryEvent::ExecutionQueued {
                execution_status: TaskExecutionStatus::Pending,
            },
        ];
        match self.service.record(&task.id, &events).await {
            Ok(_) => {
                tracing::info!(task_id = %task.id, "woke a task because a linked pull request needs attention");
                true
            }
            Err(error) => {
                tracing::warn!(task_id = %task.id, %error, "could not clear a held review request");
                false
            }
        }
    }

    async fn settle_finished_run(
        &self,
        task: Task,
        run: &TaskAgentRun,
        mode: TaskExecutionMode,
        control: Option<TaskExecutionMode>,
        mut report_stream: Option<Box<dyn TaskReportDecoder>>,
        event: CommandEvent,
    ) -> Result<(), TaskServiceError> {
        if let Some(target) = control {
            self.service.continue_turn(run).await?;
            let result = match target {
                TaskExecutionMode::Autopilot => {
                    self.service.resume_autopilot_after_operator(task).await
                }
                TaskExecutionMode::Operator => self.service.start_operator(task).await,
                TaskExecutionMode::AutopilotReport => {
                    unreachable!("a report turn cannot be a control target")
                }
            };
            let operator_task = result
                .as_ref()
                .ok()
                .filter(|(task, _, _)| task.facts.execution_mode == TaskExecutionMode::Operator)
                .map(|(task, _, _)| task.clone());
            self.watch_agent_run(result);
            if let Some(task) = operator_task {
                let _ = self.events.send(TaskExecutionUpdate::OperatorReady(task));
            }
            return Ok(());
        }
        if !matches!(event, CommandEvent::Exited(0)) {
            let diagnostic = report_stream
                .as_mut()
                .and_then(|decoder| decoder.failure_message());
            let result = self.service.finish(&task, run, event, diagnostic).await;
            let _ = self
                .events
                .send(TaskExecutionUpdate::Finished(task.id, result));
            return Ok(());
        }
        match mode {
            TaskExecutionMode::Autopilot => {
                self.service.continue_turn(run).await?;
                self.watch_agent_run(
                    self.service
                        .request_autopilot_report(task, run.answering_signal_id.clone())
                        .await,
                );
            }
            TaskExecutionMode::AutopilotReport => {
                let report = report_stream
                    .ok_or_else(|| {
                        TaskServiceError::InvalidReport("the report turn has no decoder".into())
                    })?
                    .report()
                    .map_err(TaskServiceError::InvalidReport)?;
                let (task, action) = self.service.apply_report(&task, run, &report).await?;
                let _ = self
                    .events
                    .send(TaskExecutionUpdate::Report { task: task.clone() });
                match action {
                    // Another turn is a step only while the work is moving. A run of
                    // turns that each ask for one and say nothing between them starts
                    // the next from the same state, so the operator is asked instead.
                    TaskAgentRequestedAction::Continue
                        if self.service.went_quiet(&task).await? =>
                    {
                        let result = self
                            .service
                            .await_operator(&task, run, TaskExecutionPauseReason::AgentWentQuiet)
                            .await;
                        let _ = self
                            .events
                            .send(TaskExecutionUpdate::Finished(task.id, result));
                    }
                    TaskAgentRequestedAction::Continue => {
                        self.service.continue_turn(run).await?;
                        self.watch_agent_run(self.service.continue_autopilot(task).await);
                    }
                    TaskAgentRequestedAction::AwaitOperator
                        if task.facts.deferred_review.is_some() =>
                    {
                        // The request is held in history and the host watches the
                        // linked pull requests. Give the capsule back now: watching
                        // needs no agent, and the wait should cost no capacity.
                        let result = self.service.hold_review_checks(&task, Some(run)).await;
                        let _ = self
                            .events
                            .send(TaskExecutionUpdate::Finished(task.id, result));
                    }
                    TaskAgentRequestedAction::AwaitOperator => {
                        let result = self
                            .service
                            .await_operator(&task, run, TaskExecutionPauseReason::AwaitingOperator)
                            .await;
                        let _ = self
                            .events
                            .send(TaskExecutionUpdate::Finished(task.id, result));
                    }
                }
            }
            // An operator's terminal ending is dropping the task off their desk,
            // which is the same thing as handing it back deliberately: the work
            // carries on under autopilot rather than the task being closed out.
            TaskExecutionMode::Operator => {
                self.service.continue_turn(run).await?;
                self.watch_agent_run(self.service.resume_autopilot_after_operator(task).await);
            }
        }
        Ok(())
    }
}

fn watcher_key(task: &Task) -> Option<String> {
    let operator = task.operator_id()?;
    let project = task.project()?;
    let credential = project
        .vcs_credential
        .clone()
        .unwrap_or_else(|| project.vcs_provider().profile().to_owned());
    Some(format!("{operator}\0{credential}"))
}
