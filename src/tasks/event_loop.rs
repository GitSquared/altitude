use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;

use crate::{
    agent::ClaudeStream,
    isolation::{Command, CommandEvent, CommandSession, IsolationEngine, OutputStream},
    tasks::{
        Task, TaskAgentRequestedAction, TaskAgentRun, TaskExecutionMode, TaskOperatorNotice,
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
        operator_notices: Vec<TaskOperatorNotice>,
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
    commands: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Command>>>>,
    suspending: Arc<Mutex<HashSet<String>>>,
    handoffs: Arc<Mutex<HashMap<String, TaskExecutionMode>>>,
    terminal_output: Arc<Mutex<HashMap<String, CachedTerminalOutput>>>,
}

impl<E> TaskEventLoop<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub fn new(service: TaskService<E>) -> (Self, mpsc::UnboundedReceiver<TaskExecutionUpdate>) {
        let (events, receiver) = mpsc::unbounded_channel();
        (
            Self {
                service,
                events,
                commands: Arc::new(Mutex::new(HashMap::new())),
                suspending: Arc::new(Mutex::new(HashSet::new())),
                handoffs: Arc::new(Mutex::new(HashMap::new())),
                terminal_output: Arc::new(Mutex::new(HashMap::new())),
            },
            receiver,
        )
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

    pub fn has_active_command(&self, task_id: &str) -> bool {
        self.commands
            .lock()
            .expect("task command lock poisoned")
            .contains_key(task_id)
    }

    pub fn send_terminal_input(&self, task_id: &str, bytes: Vec<u8>) {
        if let Some(commands) = self
            .commands
            .lock()
            .expect("task command lock poisoned")
            .get(task_id)
        {
            let _ = commands.send(Command::Input(bytes));
        }
    }

    pub fn resize_terminal(&self, task_id: &str, rows: u16, cols: u16) {
        if let Some(commands) = self
            .commands
            .lock()
            .expect("task command lock poisoned")
            .get(task_id)
        {
            let _ = commands.send(Command::Resize { rows, cols });
        }
    }

    pub fn take_control(&self, task_id: &str) {
        self.request_handoff(task_id, TaskExecutionMode::Operator);
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

    pub fn return_to_autopilot(&self, task_id: &str) {
        self.request_handoff(task_id, TaskExecutionMode::Autopilot);
    }

    fn request_handoff(&self, task_id: &str, target: TaskExecutionMode) {
        self.handoffs
            .lock()
            .expect("task handoff lock poisoned")
            .insert(task_id.to_owned(), target);
        if let Some(command) = self
            .commands
            .lock()
            .expect("task command lock poisoned")
            .get(task_id)
        {
            let _ = command.send(Command::Interrupt);
        }
    }

    pub fn watch_resumed_runs(
        &self,
        results: Vec<Result<(Task, TaskAgentRun, CommandSession), TaskServiceError>>,
    ) {
        for result in results {
            self.watch_agent_run(result);
        }
    }

    pub fn create_claude(&self, prompt: String) {
        tracing::info!("creating Claude task");
        let event_loop = self.clone();
        tokio::spawn(async move {
            let task = match event_loop.service.create_claude(prompt).await {
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
            event_loop.watch_agent_run(event_loop.service.start(task).await);
        });
    }

    pub fn remove(&self, task: Task) {
        let service = self.service.clone();
        let events = self.events.clone();
        self.terminal_output
            .lock()
            .expect("terminal output lock poisoned")
            .remove(&task.id);
        tokio::spawn(async move {
            let id = task.id.clone();
            let _ = events.send(TaskExecutionUpdate::Removed(id, service.remove(task).await));
        });
    }

    pub async fn suspend_all(&self) -> Result<(), TaskServiceError> {
        let tasks = self.list().await?;
        let suspending: Vec<_> = tasks
            .iter()
            .filter(|task| task.should_suspend())
            .map(|task| task.id.clone())
            .collect();
        self.suspending
            .lock()
            .expect("task suspension lock poisoned")
            .extend(suspending);
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
        self.commands
            .lock()
            .expect("task command lock poisoned")
            .insert(task.id.clone(), session.commands.clone());

        let mode = task.current_state.execution_mode;
        let service = self.service.clone();
        let events = self.events.clone();
        let commands = Arc::clone(&self.commands);
        let suspending = Arc::clone(&self.suspending);
        let handoffs = Arc::clone(&self.handoffs);
        let event_loop = self.clone();
        tokio::spawn(async move {
            let mut claude_stream =
                matches!(mode, TaskExecutionMode::AutopilotReport).then(ClaudeStream::new);
            loop {
                let event = session.events.recv().await.unwrap_or_else(|| {
                    CommandEvent::Failed(
                        "command event channel closed without an exit status".into(),
                    )
                });
                match event {
                    CommandEvent::Output { stream, bytes } => {
                        if let Some(parser) = &mut claude_stream {
                            parser.process(&bytes);
                        }
                        event_loop.cache_terminal_output(&task.id, &bytes);
                        let _ = events.send(TaskExecutionUpdate::Output {
                            task_id: task.id.clone(),
                            stream,
                            bytes,
                        });
                    }
                    event => {
                        if suspending
                            .lock()
                            .expect("task suspension lock poisoned")
                            .contains(&task.id)
                        {
                            tracing::debug!(task_id = %task.id, "command ended during intentional suspension");
                            commands
                                .lock()
                                .expect("task command lock poisoned")
                                .remove(&task.id);
                            return;
                        }
                        let handoff = handoffs
                            .lock()
                            .expect("task handoff lock poisoned")
                            .remove(&task.id);
                        commands
                            .lock()
                            .expect("task command lock poisoned")
                            .remove(&task.id);
                        if let Err(error) = event_loop
                            .handle_run_termination(
                                task.clone(),
                                &run,
                                mode,
                                handoff,
                                claude_stream,
                                event,
                            )
                            .await
                        {
                            let result = service
                                .finish(&task, &run, CommandEvent::Failed(error.to_string()))
                                .await;
                            let _ = events.send(TaskExecutionUpdate::Finished(task.id, result));
                        }
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
        handoff: Option<TaskExecutionMode>,
        report_stream: Option<ClaudeStream>,
        event: CommandEvent,
    ) -> Result<(), TaskServiceError> {
        if let Some(target) = handoff {
            self.service.continue_turn(run).await?;
            let result = match target {
                TaskExecutionMode::Autopilot => {
                    self.service.resume_autopilot_after_operator(task).await
                }
                TaskExecutionMode::Operator => self.service.start_operator(task).await,
                TaskExecutionMode::AutopilotReport => {
                    unreachable!("report turns cannot be handoff targets")
                }
            };
            let operator_task = result
                .as_ref()
                .ok()
                .filter(|(task, _, _)| {
                    task.current_state.execution_mode == TaskExecutionMode::Operator
                })
                .map(|(task, _, _)| task.clone());
            self.watch_agent_run(result);
            if let Some(task) = operator_task {
                let _ = self.events.send(TaskExecutionUpdate::OperatorReady(task));
            }
            return Ok(());
        }
        if !matches!(event, CommandEvent::Exited(0)) {
            let result = self.service.finish(&task, run, event).await;
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
                    .expect("report runs have a decoder")
                    .report()
                    .map_err(TaskServiceError::InvalidReport)?;
                let (task, operator_notices) =
                    self.service.apply_report(&task, run, &report).await?;
                let _ = self.events.send(TaskExecutionUpdate::Report {
                    task: task.clone(),
                    summary: report.summary,
                    operator_notices,
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
                    TaskAgentRequestedAction::Complete => self.complete(task, run).await,
                }
            }
            TaskExecutionMode::Operator => self.complete(task, run).await,
        }
        Ok(())
    }

    async fn complete(&self, task: Task, run: &TaskAgentRun) {
        let result = self.service.complete(&task, run).await;
        let _ = self
            .events
            .send(TaskExecutionUpdate::Finished(task.id, result));
    }
}
