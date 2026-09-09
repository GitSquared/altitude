use std::{
    error::Error,
    fmt,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::future::join_all;
use sqlx::{FromRow, SqlitePool, Type};
use uuid::Uuid;

use crate::{
    agent::{AgentEngine, AgentError, AgentKind, ClaudeCode},
    db::Database,
    isolation::{
        CommandEvent, CommandSession, IsolationEngine, IsolationError, OutputStream, SandboxOptions,
    },
};

#[derive(Clone, Debug, FromRow)]
pub struct Task {
    pub id: String,
    pub command: String,
    pub agent_kind: Option<AgentKind>,
    pub agent_conversation_id: Option<String>,
    pub status: TaskStatus,
    pub sandbox_id: Option<String>,
    pub snapshot_path: Option<String>,
    pub last_error: Option<String>,
    pub last_output: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct TaskRun {
    pub id: String,
    pub task_id: String,
}

pub struct OutputChunk {
    pub sequence: i64,
    pub recorded_at_ms: i64,
    pub stream: OutputStream,
    pub bytes: Vec<u8>,
}

impl OutputChunk {
    pub fn new(sequence: i64, stream: OutputStream, bytes: Vec<u8>) -> Self {
        Self {
            sequence,
            recorded_at_ms: now_ms(),
            stream,
            bytes,
        }
    }
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
            "SELECT tasks.id, command, agent_kind, agent_conversation_id, status, sandbox_id, snapshot_path, last_error, \
             (SELECT bytes FROM run_output WHERE run_id = \
                 (SELECT id FROM task_runs WHERE task_id = tasks.id \
                  ORDER BY started_at_ms DESC LIMIT 1) \
              ORDER BY sequence DESC LIMIT 1) AS last_output \
             FROM tasks ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(TaskError::Database)
    }

    pub async fn create_with_agent(
        &self,
        command: String,
        agent_kind: Option<AgentKind>,
        agent_conversation_id: Option<String>,
    ) -> Result<Task, TaskError> {
        let task = Task {
            id: Uuid::new_v4().to_string(),
            command,
            agent_kind,
            agent_conversation_id,
            status: TaskStatus::Pending,
            sandbox_id: None,
            snapshot_path: None,
            last_error: None,
            last_output: None,
        };
        sqlx::query(
            "INSERT INTO tasks (id, title, command, agent_kind, agent_conversation_id, status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 'pending', datetime('now'), datetime('now'))",
        )
        .bind(&task.id)
        .bind(&task.command)
        .bind(&task.command)
        .bind(task.agent_kind)
        .bind(&task.agent_conversation_id)
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
            "UPDATE tasks SET status = 'errored', sandbox_id = NULL, last_error = ?, updated_at = datetime('now') \
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

    pub async fn begin_run(&self, task_id: &str) -> Result<TaskRun, TaskError> {
        let run = TaskRun {
            id: Uuid::new_v4().to_string(),
            task_id: task_id.to_owned(),
        };
        sqlx::query("INSERT INTO task_runs (id, task_id, started_at_ms) VALUES (?, ?, ?)")
            .bind(&run.id)
            .bind(&run.task_id)
            .bind(now_ms())
            .execute(&self.pool)
            .await
            .map_err(TaskError::Database)?;
        Ok(run)
    }

    pub async fn append_output(
        &self,
        run_id: &str,
        chunks: &[OutputChunk],
    ) -> Result<(), TaskError> {
        let mut transaction = self.pool.begin().await.map_err(TaskError::Database)?;
        for chunk in chunks {
            sqlx::query(
                "INSERT INTO run_output (run_id, sequence, recorded_at_ms, stream, bytes) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(run_id)
            .bind(chunk.sequence)
            .bind(chunk.recorded_at_ms)
            .bind(match chunk.stream {
                OutputStream::Stdout => "stdout",
                OutputStream::Stderr => "stderr",
            })
            .bind(&chunk.bytes)
            .execute(&mut *transaction)
            .await
            .map_err(TaskError::Database)?;
        }
        transaction.commit().await.map_err(TaskError::Database)
    }

    pub async fn output_history(&self, task_id: &str) -> Result<Vec<Vec<u8>>, TaskError> {
        sqlx::query_scalar(
            "SELECT bytes FROM run_output WHERE run_id = \
             (SELECT id FROM task_runs WHERE task_id = ? ORDER BY started_at_ms DESC LIMIT 1) \
             ORDER BY sequence",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await
        .map_err(TaskError::Database)
    }

    pub async fn finish_run(
        &self,
        run_id: &str,
        outcome: &str,
        exit_code: Option<i32>,
    ) -> Result<(), TaskError> {
        sqlx::query(
            "UPDATE task_runs SET ended_at_ms = ?, outcome = ?, exit_code = ? WHERE id = ?",
        )
        .bind(now_ms())
        .bind(outcome)
        .bind(exit_code)
        .bind(run_id)
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_millis()
        .try_into()
        .expect("millisecond timestamp does not fit in i64")
}

#[derive(Clone)]
pub struct TaskService<E> {
    store: TaskStore,
    isolation: E,
    snapshots_dir: PathBuf,
    claude: ClaudeCode,
}

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone,
{
    pub fn new(store: TaskStore, isolation: E, snapshots_dir: PathBuf, claude: ClaudeCode) -> Self {
        Self {
            store,
            isolation,
            snapshots_dir,
            claude,
        }
    }

    pub async fn list(&self) -> Result<Vec<Task>, TaskServiceError> {
        self.store.list().await.map_err(TaskServiceError::Task)
    }

    pub async fn create_claude(&self, prompt: String) -> Result<Task, TaskServiceError> {
        tracing::info!("creating Claude task record");
        self.store
            .create_with_agent(
                prompt,
                Some(AgentKind::Claude),
                Some(self.claude.new_conversation_id()),
            )
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn start(
        &self,
        mut task: Task,
    ) -> Result<(Task, TaskRun, CommandSession), TaskServiceError> {
        let sandbox_id = format!("altitude-{}", Uuid::new_v4());
        let resuming_agent_session = task.snapshot_path.is_some();
        tracing::info!(task_id = %task.id, sandbox_id = %sandbox_id, agent = ?task.agent_kind, "starting task");
        if matches!(task.agent_kind, Some(AgentKind::Claude))
            && let Err(error) = self.claude.prepare(&self.isolation).await
        {
            self.store
                .mark_errored(&task.id, &error.to_string())
                .await
                .map_err(TaskServiceError::Task)?;
            return Err(TaskServiceError::Agent(error));
        }
        let command = match task.agent_kind {
            Some(AgentKind::Claude) => {
                let Some(conversation_id) = task.agent_conversation_id.as_deref() else {
                    let error = AgentError::new("Claude task has no conversation ID");
                    self.store
                        .mark_errored(&task.id, &error.to_string())
                        .await
                        .map_err(TaskServiceError::Task)?;
                    return Err(TaskServiceError::Agent(error));
                };
                self.claude
                    .command(conversation_id, &task.command, resuming_agent_session)
            }
            None => task.command.clone(),
        };
        if let Err(error) = self
            .isolation
            .create(
                sandbox_id.clone(),
                match task.agent_kind {
                    Some(AgentKind::Claude) => {
                        self.claude.sandbox_options(task.snapshot_path.clone())
                    }
                    None => SandboxOptions::isolated(task.snapshot_path.clone()),
                },
            )
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
        let run = self
            .store
            .begin_run(&task.id)
            .await
            .map_err(TaskServiceError::Task)?;
        let session = match self
            .isolation
            .run_command(sandbox_id.clone(), command)
            .await
        {
            Ok(session) => session,
            Err(error) => {
                if let Err(remove_error) = self.isolation.remove(sandbox_id).await {
                    tracing::warn!(task_id = %task.id, error = %remove_error, "could not remove sandbox after command launch failure");
                }
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
        tracing::info!(task_id = %task.id, sandbox_id = ?task.sandbox_id, "task started");
        Ok((task, run, session))
    }

    pub async fn append_output(
        &self,
        run: &TaskRun,
        chunks: &[OutputChunk],
    ) -> Result<(), TaskServiceError> {
        self.store
            .append_output(&run.id, chunks)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn output_history(&self, task_id: &str) -> Result<Vec<Vec<u8>>, TaskServiceError> {
        self.store
            .output_history(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn finish(
        &self,
        task: &Task,
        run: &TaskRun,
        event: CommandEvent,
    ) -> Result<Task, TaskServiceError> {
        tracing::info!(task_id = %task.id, event = command_event_name(&event), "finishing task run");
        match event {
            CommandEvent::Exited(0) => {
                self.store
                    .finish_run(&run.id, "completed", Some(0))
                    .await
                    .map_err(TaskServiceError::Task)?;
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
                self.store
                    .finish_run(&run.id, "errored", Some(code))
                    .await
                    .map_err(TaskServiceError::Task)?;
                self.fail(task, format!("command exited with code {code}"))
                    .await
            }
            CommandEvent::Failed(error) => {
                self.store
                    .finish_run(&run.id, "errored", None)
                    .await
                    .map_err(TaskServiceError::Task)?;
                self.fail(task, error).await
            }
            CommandEvent::Output { .. } => unreachable!("output is handled while the command runs"),
        }
    }

    pub async fn remove(&self, task: Task) -> Result<(), TaskServiceError> {
        tracing::info!(task_id = %task.id, "removing task");
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

    pub async fn resume_all(
        &self,
    ) -> Vec<Result<(Task, TaskRun, CommandSession), TaskServiceError>> {
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
        tracing::info!(count = tasks.len(), "suspending resumable tasks");
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
        tracing::error!(task_id = %task.id, error = %error, "task failed");
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

fn command_event_name(event: &CommandEvent) -> &'static str {
    match event {
        CommandEvent::Output { .. } => "output",
        CommandEvent::Exited(_) => "exited",
        CommandEvent::Failed(_) => "failed",
    }
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

impl Error for TaskError {}

#[derive(Debug)]
pub enum TaskServiceError {
    Agent(AgentError),
    Task(TaskError),
    Isolation(IsolationError),
}

impl fmt::Display for TaskServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(error) => error.fmt(formatter),
            Self::Task(error) => error.fmt(formatter),
            Self::Isolation(error) => error.fmt(formatter),
        }
    }
}

impl Error for TaskServiceError {}
