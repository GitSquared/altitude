use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
};

use tokio::{sync::mpsc, time};

use crate::{
    agent::TaskReportDecoder,
    capacity::{HostCapacity, HostSampler, SAMPLE_INTERVAL},
    development::DevelopmentEnvironment,
    isolation::{Command, CommandEvent, CommandSession, IsolationEngine, OutputStream},
    tasks::{
        Task, TaskAgentRequestedAction, TaskAgentRun, TaskExecutionMode, TaskExecutionStatus,
        TaskService, TaskServiceError,
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
        summary: String,
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
    events: mpsc::UnboundedSender<TaskExecutionUpdate>,
    runs: Arc<Mutex<HashMap<String, ActiveRun>>>,
    control_requests: Arc<Mutex<HashMap<String, RequestedControl>>>,
    terminal_output: Arc<Mutex<HashMap<String, CachedTerminalOutput>>>,
    /// What the host will run at once, and what it has observed to decide it.
    capacity: Arc<Mutex<HostCapacity>>,
    /// Tasks admitted but not yet running. A task stays `Pending` until its run
    /// reports, so without this it would be admitted again on the next pass.
    starting: Arc<Mutex<HashSet<String>>>,
}

/// A turn this host is currently driving.
///
/// The command channel, the run it belongs to, and a reason to expect its
/// ending all share one lifetime. That is what keeps a control request or a
/// suspension reason from being read against a run they were never about.
struct ActiveRun {
    commands: mpsc::UnboundedSender<Command>,
    agent_run_id: String,
    suspending: bool,
}

/// A driver the operator asked for, and the run that was asked during.
///
/// Both are needed to answer the only question that matters later: whether this
/// request is still the one that applies. A request whose run has gone is not a
/// request any more.
struct RequestedControl {
    agent_run_id: String,
    requested_mode: TaskExecutionMode,
}

impl<E> TaskEventLoop<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub fn new(service: TaskService<E>) -> (Self, mpsc::UnboundedReceiver<TaskExecutionUpdate>) {
        let (events, receiver) = mpsc::unbounded_channel();
        let event_loop = Self {
            service,
            events,
            runs: Arc::new(Mutex::new(HashMap::new())),
            control_requests: Arc::new(Mutex::new(HashMap::new())),
            terminal_output: Arc::new(Mutex::new(HashMap::new())),
            capacity: Arc::new(Mutex::new(HostCapacity::new())),
            starting: Arc::new(Mutex::new(HashSet::new())),
        };
        event_loop.watch_host_capacity();
        (event_loop, receiver)
    }

    /// What the host will run at once, for an interface that has to explain a wait.
    pub fn capacity_limit(&self) -> usize {
        self.capacity
            .lock()
            .expect("capacity lock poisoned")
            .limit()
    }

    /// Observe the host on an interval, and let the observation admit work.
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
                        .filter(|task| super::wants_a_slot(task))
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

    /// Start waiting tasks while the host has room for them.
    ///
    /// Cheap to call and safe to call repeatedly: a task already being started is
    /// skipped, so this is idempotent and can be triggered by anything that might
    /// have freed a slot.
    pub fn admit(&self) {
        let event_loop = self.clone();
        tokio::spawn(async move { event_loop.admit_waiting().await });
    }

    async fn admit_waiting(&self) {
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
        let mut waiting: Vec<Task> = tasks
            .into_iter()
            .filter(|task| task.facts.execution_status == TaskExecutionStatus::Pending)
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
        for task in waiting.into_iter().take(room) {
            self.starting
                .lock()
                .expect("starting lock poisoned")
                .insert(task.id.clone());
            let event_loop = self.clone();
            tokio::spawn(async move {
                let result = event_loop.service.start(task.clone()).await;
                event_loop
                    .starting
                    .lock()
                    .expect("starting lock poisoned")
                    .remove(&task.id);
                event_loop.watch_agent_run(result);
                // Provisioning may have freed or taken room for something else.
                event_loop.admit();
            });
        }
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

    /// Ask for the task to be driven differently at the next safe boundary,
    /// answering whether the request was accepted.
    ///
    /// The request names the run it was asked during, and only that run may act
    /// on it. Asking while nothing is running is refused: a request with no run to
    /// receive it would otherwise wait for whichever run ends next, handing the
    /// task over long after the operator meant it.
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

    /// Take the run this host is driving for a task, if it still has one.
    fn take_active_run(&self, task_id: &str) -> Option<ActiveRun> {
        self.runs
            .lock()
            .expect("task run lock poisoned")
            .remove(task_id)
    }

    /// Take a control request if it belongs to the run that is ending now.
    ///
    /// A request addressed to another run is dropped. That run has already gone,
    /// and honouring the request now would hand the task over at a moment the
    /// operator never asked for.
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

    /// Expect this run to end, so its ending is not recorded as a failure and
    /// does not outlive the run it belongs to.
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

    pub fn create(&self, prompt: String, environment: DevelopmentEnvironment) {
        tracing::info!(harness = %environment.harness, "creating task");
        let event_loop = self.clone();
        tokio::spawn(async move {
            let task = match event_loop.service.create(prompt, environment).await {
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

    pub fn answer_operator_signal(&self, signal_id: String, answer: String) {
        let event_loop = self.clone();
        tokio::spawn(async move {
            let result = event_loop
                .service
                .answer_operator_signal(&signal_id, answer)
                .await;
            event_loop.watch_agent_run(result);
        });
    }

    pub fn remove(&self, task: Task) {
        let service = self.service.clone();
        let events = self.events.clone();
        self.terminal_output
            .lock()
            .expect("terminal output lock poisoned")
            .remove(&task.id);
        self.runs
            .lock()
            .expect("task run lock poisoned")
            .remove(&task.id);
        self.control_requests
            .lock()
            .expect("task control request lock poisoned")
            .remove(&task.id);
        tokio::spawn(async move {
            let id = task.id.clone();
            let _ = events.send(TaskExecutionUpdate::Removed(id, service.remove(task).await));
        });
    }

    pub async fn suspend_all(&self) -> Result<(), TaskServiceError> {
        let tasks = self.list().await?;
        // The flag lives on the run it describes, so a task suspended now and
        // resumed later does not carry a stale reason for its next ending.
        for task in tasks.iter().filter(|task| super::should_suspend(task)) {
            self.mark_suspending(&task.id);
        }
        for result in self.service.suspend_all(tasks).await {
            result?;
        }
        Ok(())
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
                        if suspending {
                            tracing::debug!(task_id = %task.id, "command ended during intentional suspension");
                            return;
                        }
                        if let Err(error) = event_loop
                            .handle_run_termination(
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

    /// Interpret a finished run inside tasks/, keeping the UI concerned only with updates and input.
    async fn handle_run_termination(
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
                self.watch_agent_run(self.service.request_autopilot_report(task).await);
            }
            TaskExecutionMode::AutopilotReport => {
                let report = report_stream
                    .ok_or_else(|| {
                        TaskServiceError::InvalidReport("the report turn has no decoder".into())
                    })?
                    .report()
                    .map_err(TaskServiceError::InvalidReport)?;
                let task = self.service.apply_report(&task, run, &report).await?;
                let _ = self.events.send(TaskExecutionUpdate::Report {
                    task: task.clone(),
                    summary: report.summary,
                });
                match report.requested_action {
                    TaskAgentRequestedAction::Continue => {
                        self.service.continue_turn(run).await?;
                        self.watch_agent_run(self.service.continue_autopilot(task).await);
                    }
                    TaskAgentRequestedAction::AwaitOperator => {
                        let result = self.service.await_operator(&task, run).await;
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
