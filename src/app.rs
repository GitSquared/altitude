use tokio::sync::mpsc;

use crate::{
    isolation::{CommandSession, IsolationEngine, ShellSession},
    tasks::{Task, TaskService, TaskServiceError},
};

pub enum TaskEvent {
    Started(Result<Task, TaskServiceError>),
    Finished(String, Result<Task, TaskServiceError>),
    Removed(String, Result<(), TaskServiceError>),
}

#[derive(Clone)]
pub struct TaskRuntime<E> {
    service: TaskService<E>,
    events: mpsc::UnboundedSender<TaskEvent>,
}

impl<E> TaskRuntime<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub fn new(service: TaskService<E>) -> (Self, mpsc::UnboundedReceiver<TaskEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        (Self { service, events }, receiver)
    }

    pub async fn list(&self) -> Result<Vec<Task>, TaskServiceError> {
        self.service.list().await
    }

    pub async fn open_shell(
        &self,
        sandbox_id: String,
        rows: u16,
        cols: u16,
    ) -> Result<ShellSession, TaskServiceError> {
        self.service.open_shell(sandbox_id, rows, cols).await
    }

    pub fn resume(&self, results: Vec<Result<(Task, CommandSession), TaskServiceError>>) {
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
        for result in self.service.suspend_all(self.list().await?).await {
            result?;
        }
        Ok(())
    }

    fn observe(&self, result: Result<(Task, CommandSession), TaskServiceError>) {
        let (task, mut session) = match result {
            Ok(result) => result,
            Err(error) => {
                let _ = self.events.send(TaskEvent::Started(Err(error)));
                return;
            }
        };
        let _ = self.events.send(TaskEvent::Started(Ok(task.clone())));

        let service = self.service.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            if let Some(event) = session.events.recv().await {
                let result = service.finish(&task, event).await;
                let _ = events.send(TaskEvent::Finished(task.id, result));
            }
        });
    }
}
