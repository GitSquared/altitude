use std::{fmt, path::PathBuf};

use futures_util::future::join_all;
use sqlx::{FromRow, SqlitePool, Type};
use uuid::Uuid;

use crate::{
    db::Database,
    isolation::{CommandEvent, CommandSession, IsolationEngine, IsolationError, ShellSession},
};

#[derive(Clone, Debug, FromRow)]
pub struct Task {
    pub id: String,
    pub command: String,
    pub status: TaskStatus,
    pub sandbox_id: Option<String>,
    pub snapshot_path: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Type)]
pub enum TaskStatus {
    #[sqlx(rename = "pending")]
    Pending,
    #[sqlx(rename = "in-progress")]
    InProgress,
    #[sqlx(rename = "completed")]
    Completed,
    #[sqlx(rename = "errored")]
    Errored,
}

#[derive(Clone)]
pub struct TaskStore {
    pool: SqlitePool,
}

impl TaskStore {
    pub fn new(database: &Database) -> Self {
        Self {
            pool: database.pool().clone(),
        }
    }

    pub async fn list(&self) -> Result<Vec<Task>, TaskError> {
        sqlx::query_as(
            "SELECT id, command, status, sandbox_id, snapshot_path, last_error \
             FROM tasks ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(TaskError::Database)
    }

    pub async fn create(&self, command: String) -> Result<Task, TaskError> {
        let task = Task {
            id: Uuid::new_v4().to_string(),
            command,
            status: TaskStatus::Pending,
            sandbox_id: None,
            snapshot_path: None,
            last_error: None,
        };
        sqlx::query(
            "INSERT INTO tasks (id, title, command, status, created_at, updated_at) \
             VALUES (?, ?, ?, 'pending', datetime('now'), datetime('now'))",
        )
        .bind(&task.id)
        .bind(&task.command)
        .bind(&task.command)
        .execute(&self.pool)
        .await
        .map_err(TaskError::Database)?;
        Ok(task)
    }

    pub async fn mark_in_progress(&self, id: &str, sandbox_id: &str) -> Result<(), TaskError> {
        self.update(id, "in-progress", Some(sandbox_id), None, None)
            .await
    }

    pub async fn mark_completed(&self, id: &str) -> Result<(), TaskError> {
        self.update(id, "completed", None, None, None).await
    }

    pub async fn mark_errored(&self, id: &str, error: &str) -> Result<(), TaskError> {
        sqlx::query(
            "UPDATE tasks SET status = 'errored', last_error = ?, updated_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(error)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(TaskError::Database)?;
        Ok(())
    }

    pub async fn mark_pending(
        &self,
        id: &str,
        snapshot_path: Option<&PathBuf>,
        error: Option<&str>,
    ) -> Result<(), TaskError> {
        let snapshot_path = snapshot_path.map(|path| path.to_string_lossy().into_owned());
        self.update(id, "pending", None, snapshot_path.as_deref(), error)
            .await
    }

    pub async fn delete(&self, id: &str) -> Result<(), TaskError> {
        sqlx::query("DELETE FROM tasks WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(TaskError::Database)?;
        Ok(())
    }

    async fn update(
        &self,
        id: &str,
        status: &str,
        sandbox_id: Option<&str>,
        snapshot_path: Option<&str>,
        last_error: Option<&str>,
    ) -> Result<(), TaskError> {
        sqlx::query(
            "UPDATE tasks SET status = ?, sandbox_id = ?, snapshot_path = ?, last_error = ?, \
             updated_at = datetime('now') WHERE id = ?",
        )
        .bind(status)
        .bind(sandbox_id)
        .bind(snapshot_path)
        .bind(last_error)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(TaskError::Database)?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct TaskService<E> {
    store: TaskStore,
    isolation: E,
    snapshots_dir: PathBuf,
}

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone,
{
    pub fn new(store: TaskStore, isolation: E, snapshots_dir: PathBuf) -> Self {
        Self {
            store,
            isolation,
            snapshots_dir,
        }
    }

    pub async fn list(&self) -> Result<Vec<Task>, TaskServiceError> {
        self.store.list().await.map_err(TaskServiceError::Task)
    }

    pub async fn create_and_start(
        &self,
        command: String,
    ) -> Result<(Task, CommandSession), TaskServiceError> {
        let task = self
            .store
            .create(command)
            .await
            .map_err(TaskServiceError::Task)?;
        self.start(task).await
    }

    pub async fn open_shell(
        &self,
        sandbox_id: String,
        rows: u16,
        cols: u16,
    ) -> Result<ShellSession, TaskServiceError> {
        self.isolation
            .open_shell(sandbox_id, rows, cols)
            .await
            .map_err(TaskServiceError::Isolation)
    }

    pub async fn start(&self, mut task: Task) -> Result<(Task, CommandSession), TaskServiceError> {
        let sandbox_id = format!("altitude-{}", Uuid::new_v4());
        if let Err(error) = self
            .isolation
            .create(sandbox_id.clone(), task.snapshot_path.clone())
            .await
        {
            self.store
                .mark_errored(&task.id, &error.to_string())
                .await
                .map_err(TaskServiceError::Task)?;
            return Err(TaskServiceError::Isolation(error));
        }
        self.store
            .mark_in_progress(&task.id, &sandbox_id)
            .await
            .map_err(TaskServiceError::Task)?;
        let session = match self
            .isolation
            .run_command(sandbox_id.clone(), task.command.clone())
            .await
        {
            Ok(session) => session,
            Err(error) => {
                self.store
                    .mark_errored(&task.id, &error.to_string())
                    .await
                    .map_err(TaskServiceError::Task)?;
                return Err(TaskServiceError::Isolation(error));
            }
        };
        task.status = TaskStatus::InProgress;
        task.sandbox_id = Some(sandbox_id);
        task.last_error = None;
        Ok((task, session))
    }

    pub async fn finish(&self, task: &Task, event: CommandEvent) -> Result<Task, TaskServiceError> {
        match event {
            CommandEvent::Exited(0) => {
                self.store
                    .mark_completed(&task.id)
                    .await
                    .map_err(TaskServiceError::Task)?;
                if let Some(sandbox_id) = &task.sandbox_id {
                    self.isolation
                        .remove(sandbox_id.clone())
                        .await
                        .map_err(TaskServiceError::Isolation)?;
                }
                Ok(Task {
                    status: TaskStatus::Completed,
                    sandbox_id: None,
                    snapshot_path: None,
                    last_error: None,
                    ..task.clone()
                })
            }
            CommandEvent::Exited(code) => {
                self.fail(task, format!("command exited with code {code}"))
                    .await
            }
            CommandEvent::Failed(error) => self.fail(task, error).await,
        }
    }

    pub async fn remove(&self, task: Task) -> Result<(), TaskServiceError> {
        if let Some(sandbox_id) = task.sandbox_id {
            self.isolation
                .remove(sandbox_id)
                .await
                .map_err(TaskServiceError::Isolation)?;
        }
        if let Some(snapshot_path) = task.snapshot_path {
            self.isolation
                .remove_snapshot(PathBuf::from(snapshot_path))
                .await
                .map_err(TaskServiceError::Isolation)?;
        }
        self.store
            .delete(&task.id)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn resume_all(&self) -> Vec<Result<(Task, CommandSession), TaskServiceError>> {
        let tasks = match self.list().await {
            Ok(tasks) => tasks,
            Err(error) => return vec![Err(error)],
        };
        join_all(
            tasks
                .into_iter()
                .filter(|task| task.is_resumable())
                .map(|task| {
                    let service = self.clone();
                    async move {
                        let mut task = task;
                        if let Some(sandbox_id) = &task.sandbox_id {
                            if let Ok(snapshot_path) = service
                                .isolation
                                .suspend(
                                    sandbox_id.clone(),
                                    snapshot_name(&task.id),
                                    service.snapshots_dir.clone(),
                                )
                                .await
                            {
                                service
                                    .store
                                    .mark_pending(&task.id, Some(&snapshot_path), None)
                                    .await
                                    .map_err(TaskServiceError::Task)?;
                                task.snapshot_path =
                                    Some(snapshot_path.to_string_lossy().into_owned());
                            }
                            task.sandbox_id = None;
                            task.status = TaskStatus::Pending;
                        }
                        service.start(task).await
                    }
                }),
        )
        .await
    }

    pub async fn suspend_all(&self, tasks: Vec<Task>) -> Vec<Result<(), TaskServiceError>> {
        let resumable: Vec<_> = tasks.into_iter().filter(Task::is_resumable).collect();
        for task in &resumable {
            let snapshot_path = task.snapshot_path.as_ref().map(PathBuf::from);
            if let Err(error) = self
                .store
                .mark_pending(&task.id, snapshot_path.as_ref(), None)
                .await
            {
                return vec![Err(TaskServiceError::Task(error))];
            }
        }
        join_all(resumable.into_iter().map(|task| {
            let service = self.clone();
            async move {
                let Some(sandbox_id) = task.sandbox_id else {
                    return Ok(());
                };
                match service
                    .isolation
                    .suspend(
                        sandbox_id,
                        snapshot_name(&task.id),
                        service.snapshots_dir.clone(),
                    )
                    .await
                {
                    Ok(snapshot_path) => service
                        .store
                        .mark_pending(&task.id, Some(&snapshot_path), None)
                        .await
                        .map_err(TaskServiceError::Task),
                    Err(error) => service
                        .store
                        .mark_pending(
                            &task.id,
                            task.snapshot_path.as_ref().map(PathBuf::from).as_ref(),
                            Some(&error.to_string()),
                        )
                        .await
                        .map_err(TaskServiceError::Task),
                }
            }
        }))
        .await
    }

    async fn fail(&self, task: &Task, error: String) -> Result<Task, TaskServiceError> {
        self.store
            .mark_errored(&task.id, &error)
            .await
            .map_err(TaskServiceError::Task)?;
        Ok(Task {
            status: TaskStatus::Errored,
            last_error: Some(error),
            ..task.clone()
        })
    }
}

impl Task {
    pub fn is_resumable(&self) -> bool {
        matches!(self.status, TaskStatus::Pending | TaskStatus::InProgress)
    }
}

fn snapshot_name(task_id: &str) -> String {
    format!("task-{task_id}")
}

#[derive(Debug)]
pub enum TaskError {
    Database(sqlx::Error),
}

impl fmt::Display for TaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "task database error: {error}"),
        }
    }
}

impl std::error::Error for TaskError {}

#[derive(Debug)]
pub enum TaskServiceError {
    Task(TaskError),
    Isolation(IsolationError),
}

impl fmt::Display for TaskServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Task(error) => error.fmt(formatter),
            Self::Isolation(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TaskServiceError {}
