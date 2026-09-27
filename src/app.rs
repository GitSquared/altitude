use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{sync::mpsc, time};

use crate::{
    agent::ClaudeStream,
    isolation::{Command, CommandEvent, CommandSession, IsolationEngine, OutputStream},
    tasks::{
        OutputChunk, Task, TaskRun, TaskRunMode, TaskService, TaskServiceError, TaskSignal,
        TurnDisposition,
    },
};

pub enum TaskEvent {
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
        signals: Vec<TaskSignal>,
    },
}

#[derive(Clone)]
pub struct TaskRuntime<E> {
    service: TaskService<E>,
    events: mpsc::UnboundedSender<TaskEvent>,
    commands: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Command>>>>,
    suspending: Arc<Mutex<HashSet<String>>>,
    handoffs: Arc<Mutex<HashMap<String, TaskRunMode>>>,
}

impl<E> TaskRuntime<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub fn new(service: TaskService<E>) -> (Self, mpsc::UnboundedReceiver<TaskEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        (
            Self {
                service,
                events,
                commands: Arc::new(Mutex::new(HashMap::new())),
                suspending: Arc::new(Mutex::new(HashSet::new())),
                handoffs: Arc::new(Mutex::new(HashMap::new())),
            },
            receiver,
        )
    }

    pub async fn list(&self) -> Result<Vec<Task>, TaskServiceError> {
        self.service.list().await
    }

    pub async fn output_history(&self, task_id: &str) -> Result<Vec<Vec<u8>>, TaskServiceError> {
        self.service.output_history(task_id).await
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
        self.request_handoff(task_id, TaskRunMode::Operator);
    }

    pub fn take_stopped_control(&self, task: Task) {
        let runtime = self.clone();
        tokio::spawn(async move {
            let result = runtime.service.start_operator(task).await;
            let operator_task = result.as_ref().ok().map(|(task, _, _)| task.clone());
            runtime.observe(TaskRunMode::Operator, result);
            if let Some(task) = operator_task {
                let _ = runtime.events.send(TaskEvent::OperatorReady(task));
            }
        });
    }

    pub fn return_to_autopilot(&self, task_id: &str) {
        self.request_handoff(task_id, TaskRunMode::Autopilot);
    }

    fn request_handoff(&self, task_id: &str, target: TaskRunMode) {
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

    pub fn resume(&self, results: Vec<Result<(Task, TaskRun, CommandSession), TaskServiceError>>) {
        for result in results {
            self.observe(TaskRunMode::Autopilot, result);
        }
    }

    pub fn create_claude(&self, prompt: String) {
        tracing::info!("creating Claude task");
        let runtime = self.clone();
        tokio::spawn(async move {
            let task = match runtime.service.create_claude(prompt).await {
                Ok(task) => task,
                Err(error) => {
                    let _ = runtime.events.send(TaskEvent::Created(Err(error)));
                    return;
                }
            };
            let _ = runtime.events.send(TaskEvent::Created(Ok(task.clone())));
            runtime.observe(TaskRunMode::Autopilot, runtime.service.start(task).await);
        });
    }

    pub fn remove(&self, task: Task) {
        let service = self.service.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let id = task.id.clone();
            let _ = events.send(TaskEvent::Removed(id, service.remove(task).await));
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
        mode: TaskRunMode,
        result: Result<(Task, TaskRun, CommandSession), TaskServiceError>,
    ) {
        let (task, run, mut session) = match result {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(error = %error, "task failed to start");
                let _ = self.events.send(TaskEvent::Started(Err(error)));
                return;
            }
        };
        let _ = self.events.send(TaskEvent::Started(Ok(task.clone())));
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
            let mut chunks = Vec::new();
            let mut sequence = 0;
            let mut claude_stream =
                matches!(mode, TaskRunMode::AutopilotReport).then(ClaudeStream::new);
            let mut flush = time::interval(Duration::from_millis(100));
            loop {
                tokio::select! {
                    _ = flush.tick(), if !chunks.is_empty() => {
                        if let Err(error) = service.append_output(&run, &chunks).await {
                            tracing::error!(task_id = %task.id, error = %error, "could not persist task output");
                            let result = service
                                .finish(
                                    &task,
                                    &run,
                                    CommandEvent::Failed(error.to_string()),
                                )
                                .await;
                            commands
                                .lock()
                                .expect("task command lock poisoned")
                                .remove(&task.id);
                            let _ = events.send(TaskEvent::Finished(task.id.clone(), result));
                            return;
                        }
                        chunks.clear();
                    }
                    event = session.events.recv() => {
                        let Some(event) = event else { return };
                        match event {
                            CommandEvent::Output { stream, bytes } => {
                                if let Some(parser) = &mut claude_stream {
                                    parser.process(&bytes);
                                }
                                chunks.push(OutputChunk::new(sequence, stream, bytes.clone()));
                                sequence += 1;
                                let _ = events.send(TaskEvent::Output { task_id: task.id.clone(), stream, bytes });
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
                                if !chunks.is_empty()
                                    && let Err(error) = service.append_output(&run, &chunks).await
                                {
                                    tracing::error!(task_id = %task.id, error = %error, "could not persist final task output");
                                    let result = service
                                        .finish(
                                            &task,
                                            &run,
                                            CommandEvent::Failed(error.to_string()),
                                        )
                                        .await;
                                    commands
                                        .lock()
                                        .expect("task command lock poisoned")
                                        .remove(&task.id);
                                    let _ = events.send(TaskEvent::Finished(task.id.clone(), result));
                                    return;
                                }
                                commands
                                    .lock()
                                    .expect("task command lock poisoned")
                                    .remove(&task.id);
                                if let Some(target) = handoff {
                                    if let Err(error) = service.continue_turn(&run).await {
                                        let _ = events.send(TaskEvent::Finished(task.id.clone(), Err(error)));
                                        return;
                                    }
                                    let result = match target {
                                        TaskRunMode::Autopilot => {
                                            service.resume_autopilot_after_operator(task).await
                                        }
                                        TaskRunMode::AutopilotReport => {
                                            unreachable!("report turns cannot be handoff targets")
                                        }
                                        TaskRunMode::Operator => service.start_operator(task).await,
                                    };
                                    let operator_task = matches!(target, TaskRunMode::Operator)
                                        .then(|| result.as_ref().ok().map(|(task, _, _)| task.clone()))
                                        .flatten();
                                    runtime.observe(target, result);
                                    if let Some(task) = operator_task {
                                        let _ = events.send(TaskEvent::OperatorReady(task));
                                    }
                                    return;
                                }
                                if matches!(mode, TaskRunMode::Autopilot)
                                    && matches!(event, CommandEvent::Exited(0))
                                {
                                    if let Err(error) = service.continue_turn(&run).await {
                                        let _ = events.send(TaskEvent::Finished(task.id.clone(), Err(error)));
                                        return;
                                    }
                                    runtime.observe(
                                        TaskRunMode::AutopilotReport,
                                        service.request_autopilot_report(task).await,
                                    );
                                    return;
                                }
                                if matches!(mode, TaskRunMode::AutopilotReport)
                                    && matches!(event, CommandEvent::Exited(0))
                                {
                                    match claude_stream.expect("report turns create a Claude parser").report() {
                                        Ok(report) => {
                                            let (task, signals) = match service.apply_report(&task, &run, &report).await {
                                                Ok(result) => result,
                                                Err(error) => {
                                                    let result = service.finish(&task, &run, CommandEvent::Failed(error.to_string())).await;
                                                    let _ = events.send(TaskEvent::Finished(task.id, result));
                                                    return;
                                                }
                                            };
                                            let _ = events.send(TaskEvent::Report {
                                                task: task.clone(),
                                                summary: report.summary,
                                                signals,
                                            });
                                            match report.disposition {
                                            TurnDisposition::Continue => {
                                                if let Err(error) = service.continue_turn(&run).await {
                                                    let _ = events.send(TaskEvent::Finished(task.id.clone(), Err(error)));
                                                    return;
                                                }
                                                runtime.observe(
                                                    TaskRunMode::Autopilot,
                                                    service.continue_autopilot(task).await,
                                                );
                                            }
                                            TurnDisposition::AwaitOperator => {
                                                let result = service.await_operator(&task, &run).await;
                                                let _ = events.send(TaskEvent::Finished(task.id, result));
                                            }
                                            TurnDisposition::Complete => {
                                                let result = service.complete(&task, &run).await;
                                                let _ = events.send(TaskEvent::Finished(task.id, result));
                                            }
                                        }
                                        }
                                        Err(error) => {
                                            let result = service
                                                .finish(
                                                    &task,
                                                    &run,
                                                    CommandEvent::Failed(error),
                                                )
                                                .await;
                                            let _ = events.send(TaskEvent::Finished(task.id, result));
                                        }
                                    }
                                    return;
                                }
                                if matches!(mode, TaskRunMode::Operator)
                                    && matches!(event, CommandEvent::Exited(0))
                                {
                                    let result = service.complete(&task, &run).await;
                                    let _ = events.send(TaskEvent::Finished(task.id, result));
                                    return;
                                }
                                let result = service.finish(&task, &run, event).await;
                                let _ = events.send(TaskEvent::Finished(task.id, result));
                                return;
                            }
                        }
                    }
                }
            }
        });
    }
}
