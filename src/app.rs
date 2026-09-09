use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{sync::mpsc, time};

use crate::{
    isolation::{Command, CommandSession, IsolationEngine, OutputStream},
    tasks::{OutputChunk, Task, TaskRun, TaskService, TaskServiceError},
};

pub enum TaskEvent {
    Started(Result<Task, TaskServiceError>),
    Finished(String, Result<Task, TaskServiceError>),
    Removed(String, Result<(), TaskServiceError>),
    Output {
        task_id: String,
        stream: OutputStream,
        bytes: Vec<u8>,
    },
}

#[derive(Clone)]
pub struct TaskRuntime<E> {
    service: TaskService<E>,
    events: mpsc::UnboundedSender<TaskEvent>,
    commands: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Command>>>>,
    suspending: Arc<Mutex<HashSet<String>>>,
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

    pub fn resume(&self, results: Vec<Result<(Task, TaskRun, CommandSession), TaskServiceError>>) {
        for result in results {
            self.observe(result);
        }
    }

    pub fn create(&self, command: String) {
        let runtime = self.clone();
        tokio::spawn(async move {
            runtime.observe(runtime.service.create_and_start(command).await);
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
            .filter(|task| task.is_resumable())
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

    fn observe(&self, result: Result<(Task, TaskRun, CommandSession), TaskServiceError>) {
        let (task, run, mut session) = match result {
            Ok(result) => result,
            Err(error) => {
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
        tokio::spawn(async move {
            let mut chunks = Vec::new();
            let mut sequence = 0;
            let mut flush = time::interval(Duration::from_millis(100));
            loop {
                tokio::select! {
                    _ = flush.tick(), if !chunks.is_empty() => {
                        if let Err(error) = service.append_output(&run, &chunks).await {
                            let result = service
                                .finish(
                                    &task,
                                    &run,
                                    crate::isolation::CommandEvent::Failed(error.to_string()),
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
                            crate::isolation::CommandEvent::Output { stream, bytes } => {
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
                                if !chunks.is_empty()
                                    && let Err(error) = service.append_output(&run, &chunks).await
                                {
                                    let result = service
                                        .finish(
                                            &task,
                                            &run,
                                            crate::isolation::CommandEvent::Failed(error.to_string()),
                                        )
                                        .await;
                                    commands
                                        .lock()
                                        .expect("task command lock poisoned")
                                        .remove(&task.id);
                                    let _ = events.send(TaskEvent::Finished(task.id.clone(), result));
                                    return;
                                }
                                let result = service.finish(&task, &run, event).await;
                                commands
                                    .lock()
                                    .expect("task command lock poisoned")
                                    .remove(&task.id);
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
