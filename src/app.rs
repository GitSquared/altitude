use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;

use crate::{
    agent::ClaudeStream,
    isolation::{Command, CommandEvent, CommandSession, IsolationEngine, OutputStream},
    tasks::{
        Task, TaskAgentRequestedAction, TaskAgentRun, TaskAgentRunMode, TaskOperatorNotice,
        TaskService, TaskServiceError,
    },
};

pub enum TaskRuntimeUpdate {
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
pub struct TaskRuntime<E> {
    service: TaskService<E>,
    events: mpsc::UnboundedSender<TaskRuntimeUpdate>,
    commands: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Command>>>>,
    suspending: Arc<Mutex<HashSet<String>>>,
    handoffs: Arc<Mutex<HashMap<String, TaskAgentRunMode>>>,
    terminal_output: Arc<Mutex<HashMap<String, CachedTerminalOutput>>>,
}

impl<E> TaskRuntime<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub fn new(service: TaskService<E>) -> (Self, mpsc::UnboundedReceiver<TaskRuntimeUpdate>) {
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

    pub fn has_command(&self, task_id: &str) -> bool {
        self.commands
            .lock()
            .expect("task command lock poisoned")
            .contains_key(task_id)
    }

    pub fn send_input(&self, task_id: &str, bytes: Vec<u8>) {
        if let Some(commands) = self
            .commands
            .lock()
            .expect("task command lock poisoned")
            .get(task_id)
        {
            let _ = commands.send(Command::Input(bytes));
        }
    }

    pub fn resize(&self, task_id: &str, rows: u16, cols: u16) {
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
        self.request_handoff(task_id, TaskAgentRunMode::Operator);
    }

    pub fn take_stopped_control(&self, task: Task) {
        let runtime = self.clone();
        tokio::spawn(async move {
            let result = runtime.service.start_operator(task).await;
            let operator_task = result.as_ref().ok().map(|(task, _, _)| task.clone());
            runtime.observe(TaskAgentRunMode::Operator, result);
            if let Some(task) = operator_task {
                let _ = runtime.events.send(TaskRuntimeUpdate::OperatorReady(task));
            }
        });
    }

    pub fn return_to_autopilot(&self, task_id: &str) {
        self.request_handoff(task_id, TaskAgentRunMode::Autopilot);
    }

    fn request_handoff(&self, task_id: &str, target: TaskAgentRunMode) {
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

    pub fn resume(
        &self,
        results: Vec<Result<(Task, TaskAgentRun, CommandSession), TaskServiceError>>,
    ) {
        for result in results {
            self.observe(TaskAgentRunMode::Autopilot, result);
        }
    }

    pub fn create_claude(&self, prompt: String) {
        tracing::info!("creating Claude task");
        let runtime = self.clone();
        tokio::spawn(async move {
            let task = match runtime.service.create_claude(prompt).await {
                Ok(task) => task,
                Err(error) => {
                    let _ = runtime.events.send(TaskRuntimeUpdate::Created(Err(error)));
                    return;
                }
            };
            let _ = runtime
                .events
                .send(TaskRuntimeUpdate::Created(Ok(task.clone())));
            runtime.observe(
                TaskAgentRunMode::Autopilot,
                runtime.service.start(task).await,
            );
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
            let _ = events.send(TaskRuntimeUpdate::Removed(id, service.remove(task).await));
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

    fn observe(
        &self,
        mode: TaskAgentRunMode,
        result: Result<(Task, TaskAgentRun, CommandSession), TaskServiceError>,
    ) {
        let (task, run, mut session) = match result {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(error = %error, "task failed to start");
                let _ = self.events.send(TaskRuntimeUpdate::Started(Err(error)));
                return;
            }
        };
        self.terminal_output
            .lock()
            .expect("terminal output lock poisoned")
            .remove(&task.id);
        let _ = self
            .events
            .send(TaskRuntimeUpdate::Started(Ok(task.clone())));
        self.commands
            .lock()
            .expect("task command lock poisoned")
            .insert(task.id.clone(), session.commands.clone());

        let service = self.service.clone();
        let events = self.events.clone();
        let commands = Arc::clone(&self.commands);
        let suspending = Arc::clone(&self.suspending);
        let handoffs = Arc::clone(&self.handoffs);
        let runtime = self.clone();
        tokio::spawn(async move {
            let mut claude_stream =
                matches!(mode, TaskAgentRunMode::AutopilotReport).then(ClaudeStream::new);
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
                        runtime.cache_terminal_output(&task.id, &bytes);
                        let _ = events.send(TaskRuntimeUpdate::Output {
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
                        if let Some(target) = handoff {
                            if let Err(error) = service.continue_turn(&run).await {
                                let _ = events
                                    .send(TaskRuntimeUpdate::Finished(task.id.clone(), Err(error)));
                                return;
                            }
                            let result = match target {
                                TaskAgentRunMode::Autopilot => {
                                    service.resume_autopilot_after_operator(task).await
                                }
                                TaskAgentRunMode::AutopilotReport => {
                                    unreachable!("report turns cannot be handoff targets")
                                }
                                TaskAgentRunMode::Operator => service.start_operator(task).await,
                            };
                            let operator_task = matches!(target, TaskAgentRunMode::Operator)
                                .then(|| result.as_ref().ok().map(|(task, _, _)| task.clone()))
                                .flatten();
                            runtime.observe(target, result);
                            if let Some(task) = operator_task {
                                let _ = events.send(TaskRuntimeUpdate::OperatorReady(task));
                            }
                            return;
                        }
                        if matches!(mode, TaskAgentRunMode::Autopilot)
                            && matches!(event, CommandEvent::Exited(0))
                        {
                            if let Err(error) = service.continue_turn(&run).await {
                                let _ = events
                                    .send(TaskRuntimeUpdate::Finished(task.id.clone(), Err(error)));
                                return;
                            }
                            runtime.observe(
                                TaskAgentRunMode::AutopilotReport,
                                service.request_autopilot_report(task).await,
                            );
                            return;
                        }
                        if matches!(mode, TaskAgentRunMode::AutopilotReport)
                            && matches!(event, CommandEvent::Exited(0))
                        {
                            match claude_stream
                                .expect("report turns create a Claude parser")
                                .report()
                            {
                                Ok(report) => {
                                    let (task, operator_notices) =
                                        match service.apply_report(&task, &run, &report).await {
                                            Ok(result) => result,
                                            Err(error) => {
                                                let result = service
                                                    .finish(
                                                        &task,
                                                        &run,
                                                        CommandEvent::Failed(error.to_string()),
                                                    )
                                                    .await;
                                                let _ = events.send(TaskRuntimeUpdate::Finished(
                                                    task.id, result,
                                                ));
                                                return;
                                            }
                                        };
                                    let _ = events.send(TaskRuntimeUpdate::Report {
                                        task: task.clone(),
                                        summary: report.summary,
                                        operator_notices,
                                    });
                                    match report.requested_action {
                                        TaskAgentRequestedAction::Continue => {
                                            if let Err(error) = service.continue_turn(&run).await {
                                                let _ = events.send(TaskRuntimeUpdate::Finished(
                                                    task.id.clone(),
                                                    Err(error),
                                                ));
                                                return;
                                            }
                                            runtime.observe(
                                                TaskAgentRunMode::Autopilot,
                                                service.continue_autopilot(task).await,
                                            );
                                        }
                                        TaskAgentRequestedAction::AwaitOperator => {
                                            let result = service.await_operator(&task, &run).await;
                                            let _ = events
                                                .send(TaskRuntimeUpdate::Finished(task.id, result));
                                        }
                                        TaskAgentRequestedAction::Complete => {
                                            let result = service.complete(&task, &run).await;
                                            let _ = events
                                                .send(TaskRuntimeUpdate::Finished(task.id, result));
                                        }
                                    }
                                }
                                Err(error) => {
                                    let result = service
                                        .finish(&task, &run, CommandEvent::Failed(error))
                                        .await;
                                    let _ =
                                        events.send(TaskRuntimeUpdate::Finished(task.id, result));
                                }
                            }
                            return;
                        }
                        if matches!(mode, TaskAgentRunMode::Operator)
                            && matches!(event, CommandEvent::Exited(0))
                        {
                            let result = service.complete(&task, &run).await;
                            let _ = events.send(TaskRuntimeUpdate::Finished(task.id, result));
                            return;
                        }
                        let result = service.finish(&task, &run, event).await;
                        let _ = events.send(TaskRuntimeUpdate::Finished(task.id, result));
                        return;
                    }
                }
            }
        });
    }
}
