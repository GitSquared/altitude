use crate::vcs::GitHub;
use chrono::{Local, SecondsFormat};
use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
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

#[derive(Clone, Debug, serde::Deserialize)]
pub struct TurnReport {
    pub disposition: TurnDisposition,
    pub operator_attention: Option<OperatorAttention>,
    pub summary: String,
    #[serde(default)]
    pub signals: Vec<Signal>,
}

#[derive(Clone, Debug, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnDisposition {
    Continue,
    AwaitOperator,
    Complete,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct Signal {
    pub level: SignalLevel,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SignalLevel {
    Info,
    Attention,
    Alarm,
}

impl SignalLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Attention => "attention",
            Self::Alarm => "alarm",
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum OperatorAttention {
    #[sqlx(rename = "nominal")]
    Nominal,
    #[sqlx(rename = "attention")]
    Attention,
    #[sqlx(rename = "alarm")]
    Alarm,
}

#[derive(Clone, Debug, FromRow)]
pub struct Task {
    pub id: String,
    pub command: String,
    pub agent_kind: Option<AgentKind>,
    pub agent_conversation_id: Option<String>,
    pub status: TaskStatus,
    pub custody: TaskCustody,
    pub operator_attention: OperatorAttention,
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

#[derive(Clone, Debug, FromRow)]
pub struct TaskSignal {
    pub id: String,
    pub level: String,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct AgentTaskContext {
    execution: TaskStatus,
    custody: TaskCustody,
    operator_attention: OperatorAttention,
    datetime: String,
}

impl fmt::Display for AgentTaskContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "execution={:?}; custody={:?}; operator_attention={:?}; current_datetime={}",
            self.execution, self.custody, self.operator_attention, self.datetime
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskRunMode {
    Autopilot,
    AutopilotReport,
    Operator,
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
    #[sqlx(rename = "awaiting")]
    Awaiting,
    #[sqlx(rename = "completed")]
    Completed,
    #[sqlx(rename = "errored")]
    Errored,
}

#[derive(Clone, Debug, PartialEq, Eq, Type)]
pub enum TaskCustody {
    #[sqlx(rename = "autopilot")]
    Autopilot,
    #[sqlx(rename = "operator")]
    Operator,
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
            "SELECT tasks.id, command, agent_kind, agent_conversation_id, status, custody, operator_attention, sandbox_id, snapshot_path, last_error, \
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
            custody: TaskCustody::Autopilot,
            operator_attention: OperatorAttention::Nominal,
            sandbox_id: None,
            snapshot_path: None,
            last_error: None,
            last_output: None,
        };
        sqlx::query(
            "INSERT INTO tasks (id, title, command, agent_kind, agent_conversation_id, status, custody, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 'pending', 'autopilot', datetime('now'), datetime('now'))",
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

    pub async fn mark_awaiting(&self, id: &str, snapshot_path: &Path) -> Result<(), TaskError> {
        let snapshot_path = snapshot_path.to_string_lossy();
        self.update(id, "awaiting", None, Some(&snapshot_path), None)
            .await
    }

    pub async fn set_custody(&self, id: &str, custody: TaskCustody) -> Result<(), TaskError> {
        sqlx::query("UPDATE tasks SET custody = ?, updated_at = datetime('now') WHERE id = ?")
            .bind(custody)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(TaskError::Database)?;
        Ok(())
    }

    pub async fn record_signals(
        &self,
        task_id: &str,
        run_id: &str,
        operator_attention: Option<&OperatorAttention>,
        signals: &[Signal],
    ) -> Result<Vec<TaskSignal>, TaskError> {
        let mut transaction = self.pool.begin().await.map_err(TaskError::Database)?;
        let mut recorded = Vec::with_capacity(signals.len());
        for signal in signals {
            let item = TaskSignal {
                id: Uuid::new_v4().to_string(),
                level: signal.level.as_str().to_owned(),
                title: signal.title.clone(),
                body: signal.body.clone(),
            };
            sqlx::query(
                "INSERT INTO task_signals (id, task_id, run_id, level, title, body, created_at_ms) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&item.id)
            .bind(task_id)
            .bind(run_id)
            .bind(&item.level)
            .bind(&item.title)
            .bind(&item.body)
            .bind(now_ms())
            .execute(&mut *transaction)
            .await
            .map_err(TaskError::Database)?;
            recorded.push(item);
        }
        if let Some(operator_attention) = operator_attention {
            sqlx::query(
                "UPDATE tasks SET operator_attention = ?, updated_at = datetime('now') WHERE id = ?",
            )
            .bind(operator_attention)
            .bind(task_id)
            .execute(&mut *transaction)
            .await
            .map_err(TaskError::Database)?;
        }
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(recorded)
    }

    pub async fn unacknowledged_signals(
        &self,
        level: SignalLevel,
    ) -> Result<Vec<TaskSignal>, TaskError> {
        sqlx::query_as(
            "SELECT id, level, title, body FROM task_signals \
             WHERE level = ? AND acknowledged_at_ms IS NULL ORDER BY created_at_ms DESC",
        )
        .bind(level.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(TaskError::Database)
    }

    pub async fn acknowledge_signal(&self, id: &str) -> Result<(), TaskError> {
        sqlx::query("UPDATE task_signals SET acknowledged_at_ms = ? WHERE id = ?")
            .bind(now_ms())
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(TaskError::Database)?;
        Ok(())
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

    pub async fn mark_suspended(
        &self,
        id: &str,
        status: TaskStatus,
        snapshot_path: Option<&PathBuf>,
        error: Option<&str>,
    ) -> Result<(), TaskError> {
        let snapshot_path = snapshot_path.map(|path| path.to_string_lossy().into_owned());
        self.update(id, status.as_str(), None, snapshot_path.as_deref(), error)
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

    pub async fn apply_report(
        &self,
        task: &Task,
        run: &TaskRun,
        report: &TurnReport,
    ) -> Result<(Task, Vec<TaskSignal>), TaskServiceError> {
        let operator_attention = report
            .operator_attention
            .as_ref()
            .unwrap_or(&task.operator_attention);
        if (matches!(operator_attention, OperatorAttention::Attention)
            && !matches!(report.disposition, TurnDisposition::AwaitOperator))
            || (matches!(report.disposition, TurnDisposition::AwaitOperator)
                && matches!(operator_attention, OperatorAttention::Nominal))
        {
            return Err(TaskServiceError::InvalidReport(
                "attention requires await_operator; await_operator requires attention or alarm"
                    .into(),
            ));
        }
        let signals = self
            .store
            .record_signals(
                &task.id,
                &run.id,
                report.operator_attention.as_ref(),
                &report.signals,
            )
            .await
            .map_err(TaskServiceError::Task)?;
        Ok((
            Task {
                operator_attention: operator_attention.clone(),
                ..task.clone()
            },
            signals,
        ))
    }

    pub async fn unacknowledged_signals(
        &self,
        level: SignalLevel,
    ) -> Result<Vec<TaskSignal>, TaskServiceError> {
        self.store
            .unacknowledged_signals(level)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn acknowledge_signal(&self, id: &str) -> Result<(), TaskServiceError> {
        self.store
            .acknowledge_signal(id)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn start(
        &self,
        task: Task,
    ) -> Result<(Task, TaskRun, CommandSession), TaskServiceError> {
        let resuming_agent_session = task.snapshot_path.is_some();
        let command = match task.agent_kind {
            Some(AgentKind::Claude) => {
                let conversation_id = self
                    .claude_conversation(&task, resuming_agent_session)
                    .await?;
                self.claude.autopilot_command(
                    &conversation_id,
                    if resuming_agent_session {
                        "Resume the task and continue making progress."
                    } else {
                        &task.command
                    },
                    &task.autopilot_context(),
                    resuming_agent_session,
                )
            }
            None => task.command.clone(),
        };
        let mut task = self.provision(task).await?;
        self.store
            .set_custody(&task.id, TaskCustody::Autopilot)
            .await
            .map_err(TaskServiceError::Task)?;
        task.custody = TaskCustody::Autopilot;
        self.launch(task, command).await
    }

    async fn provision(&self, mut task: Task) -> Result<Task, TaskServiceError> {
        let sandbox_id = format!("altitude-{}", Uuid::new_v4());
        tracing::info!(task_id = %task.id, sandbox_id = %sandbox_id, agent = ?task.agent_kind, "provisioning task sandbox");
        let create = match task.agent_kind {
            Some(AgentKind::Claude) => self
                .isolation
                .provision(
                    sandbox_id.clone(),
                    task.snapshot_path.clone(),
                    vec![
                        self.claude.environment_recipe(),
                        GitHub::environment_recipe(),
                    ],
                )
                .await
                .map_err(TaskServiceError::Isolation),
            None => self
                .isolation
                .create(
                    sandbox_id.clone(),
                    SandboxOptions::isolated(task.snapshot_path.clone()),
                )
                .await
                .map_err(TaskServiceError::Isolation),
        };
        if let Err(error) = create {
            self.store
                .mark_errored(&task.id, &error.to_string())
                .await
                .map_err(TaskServiceError::Task)?;
            return Err(error);
        }
        task.sandbox_id = Some(sandbox_id);
        Ok(task)
    }

    pub async fn continue_autopilot(
        &self,
        task: Task,
    ) -> Result<(Task, TaskRun, CommandSession), TaskServiceError> {
        self.start_autopilot_turn(
            task,
            "Continue the task autonomously from the current state.",
        )
        .await
    }

    pub async fn resume_autopilot_after_operator(
        &self,
        task: Task,
    ) -> Result<(Task, TaskRun, CommandSession), TaskServiceError> {
        self.start_autopilot_turn(
            task,
            "The operator has returned control to Altitude. Review the preceding conversation and current workspace state, then continue the task autonomously wherever the original intent reasonably permits. Do not wait for routine confirmation.",
        )
        .await
    }

    async fn start_autopilot_turn(
        &self,
        mut task: Task,
        prompt: &str,
    ) -> Result<(Task, TaskRun, CommandSession), TaskServiceError> {
        let conversation_id = self.claude_conversation(&task, true).await?;
        self.store
            .set_custody(&task.id, TaskCustody::Autopilot)
            .await
            .map_err(TaskServiceError::Task)?;
        task.custody = TaskCustody::Autopilot;
        let context = task.autopilot_context();
        let command = self
            .claude
            .autopilot_command(&conversation_id, prompt, &context, true);
        self.launch(task, command).await
    }

    pub async fn request_autopilot_report(
        &self,
        task: Task,
    ) -> Result<(Task, TaskRun, CommandSession), TaskServiceError> {
        let conversation_id = self.claude_conversation(&task, true).await?;
        let command = self
            .claude
            .report_command(&conversation_id, &task.autopilot_context());
        self.launch(task, command).await
    }

    pub async fn start_operator(
        &self,
        mut task: Task,
    ) -> Result<(Task, TaskRun, CommandSession), TaskServiceError> {
        let conversation_id = self.claude_conversation(&task, true).await?;
        if task.sandbox_id.is_none() {
            task = self.provision(task).await?;
        }
        self.store
            .set_custody(&task.id, TaskCustody::Operator)
            .await
            .map_err(TaskServiceError::Task)?;
        task.custody = TaskCustody::Operator;
        self.launch(task, self.claude.operator_command(&conversation_id))
            .await
    }

    async fn launch(
        &self,
        mut task: Task,
        command: String,
    ) -> Result<(Task, TaskRun, CommandSession), TaskServiceError> {
        let Some(sandbox_id) = task.sandbox_id.as_deref() else {
            return Err(TaskServiceError::Task(TaskError::MissingSandbox));
        };
        self.store
            .mark_in_progress(&task.id, sandbox_id)
            .await
            .map_err(TaskServiceError::Task)?;
        let run = self
            .store
            .begin_run(&task.id)
            .await
            .map_err(TaskServiceError::Task)?;
        let session = match self
            .isolation
            .run_command(sandbox_id.to_owned(), command)
            .await
        {
            Ok(session) => session,
            Err(error) => {
                if let Err(remove_error) = self.isolation.remove(sandbox_id.to_owned()).await {
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
        task.last_error = None;
        tracing::info!(task_id = %task.id, sandbox_id = ?task.sandbox_id, "task started");
        Ok((task, run, session))
    }

    pub async fn await_operator(
        &self,
        task: &Task,
        run: &TaskRun,
    ) -> Result<Task, TaskServiceError> {
        self.store
            .finish_run(&run.id, "awaiting-operator", Some(0))
            .await
            .map_err(TaskServiceError::Task)?;
        let sandbox_id = task
            .sandbox_id
            .as_ref()
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        let snapshot_path = self
            .isolation
            .suspend(
                sandbox_id.to_owned(),
                snapshot_name(&task.id),
                self.snapshots_dir.clone(),
            )
            .await
            .map_err(TaskServiceError::Isolation)?;
        self.store
            .mark_awaiting(&task.id, &snapshot_path)
            .await
            .map_err(TaskServiceError::Task)?;
        Ok(Task {
            status: TaskStatus::Awaiting,
            sandbox_id: None,
            snapshot_path: Some(snapshot_path.to_string_lossy().into_owned()),
            ..task.clone()
        })
    }

    pub async fn continue_turn(&self, run: &TaskRun) -> Result<(), TaskServiceError> {
        self.store
            .finish_run(&run.id, "continued", Some(0))
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn complete(&self, task: &Task, run: &TaskRun) -> Result<Task, TaskServiceError> {
        self.store
            .finish_run(&run.id, "completed", Some(0))
            .await
            .map_err(TaskServiceError::Task)?;
        if let Some(sandbox_id) = &task.sandbox_id {
            self.isolation
                .remove(sandbox_id.clone())
                .await
                .map_err(TaskServiceError::Isolation)?;
        }
        if let Some(snapshot_path) = &task.snapshot_path {
            self.isolation
                .remove_snapshot(PathBuf::from(snapshot_path))
                .await
                .map_err(TaskServiceError::Isolation)?;
        }
        self.store
            .mark_completed(&task.id)
            .await
            .map_err(TaskServiceError::Task)?;
        Ok(Task {
            status: TaskStatus::Completed,
            sandbox_id: None,
            snapshot_path: None,
            last_error: None,
            ..task.clone()
        })
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
            CommandEvent::Exited(0) => self.complete(task, run).await,
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
        if task.agent_kind == Some(AgentKind::Claude)
            && let Some(conversation_id) = task.agent_conversation_id.as_deref()
        {
            let files = self.claude.conversation_files(conversation_id);
            self.isolation
                .remove_profile_entries(files.profile, files.cleanup_entries)
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
                .filter(|task| task.should_resume())
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
        let suspendable: Vec<_> = tasks.into_iter().filter(Task::should_suspend).collect();
        for task in &suspendable {
            let snapshot_path = task.snapshot_path.as_ref().map(PathBuf::from);
            if let Err(error) = self
                .store
                .mark_suspended(&task.id, task.status.clone(), snapshot_path.as_ref(), None)
                .await
            {
                return vec![Err(TaskServiceError::Task(error))];
            }
        }
        join_all(suspendable.into_iter().map(|task| {
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
                        .mark_suspended(&task.id, task.status.clone(), Some(&snapshot_path), None)
                        .await
                        .map_err(TaskServiceError::Task),
                    Err(error) => service
                        .store
                        .mark_suspended(
                            &task.id,
                            task.status.clone(),
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

    async fn claude_conversation(
        &self,
        task: &Task,
        require_transcript: bool,
    ) -> Result<String, TaskServiceError> {
        let Some(conversation_id) = task.agent_conversation_id.as_deref() else {
            return self
                .conversation_failure(task, "Claude task has no conversation ID")
                .await;
        };
        if !require_transcript {
            return Ok(conversation_id.to_owned());
        }

        let files = self.claude.conversation_files(conversation_id);
        match self
            .isolation
            .profile_regular_file_exists(files.profile, files.transcript)
            .await
        {
            Ok(true) => Ok(conversation_id.to_owned()),
            Ok(false) => {
                self.conversation_failure(
                    task,
                    "Claude conversation transcript is missing; remove this task or restore its session file before resuming it",
                )
                .await
            }
            Err(error) => {
                self.conversation_failure(
                    task,
                    format!("could not read Claude conversation transcript: {error}"),
                )
                .await
            }
        }
    }

    async fn conversation_failure<T>(
        &self,
        task: &Task,
        message: impl Into<String>,
    ) -> Result<T, TaskServiceError> {
        let error = AgentError::new(message);
        self.store
            .mark_errored(&task.id, &error.to_string())
            .await
            .map_err(TaskServiceError::Task)?;
        Err(TaskServiceError::Agent(error))
    }
}

impl Task {
    pub fn autopilot_context(&self) -> AgentTaskContext {
        AgentTaskContext {
            execution: TaskStatus::InProgress,
            custody: TaskCustody::Autopilot,
            operator_attention: self.operator_attention.clone(),
            datetime: Local::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        }
    }

    pub fn should_resume(&self) -> bool {
        matches!(self.status, TaskStatus::Pending | TaskStatus::InProgress)
    }

    pub fn should_suspend(&self) -> bool {
        matches!(self.status, TaskStatus::InProgress | TaskStatus::Awaiting)
    }
}

impl TaskStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in-progress",
            Self::Awaiting => "awaiting",
            Self::Completed => "completed",
            Self::Errored => "errored",
        }
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
    MissingSandbox,
}

impl fmt::Display for TaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "task database error: {error}"),
            Self::MissingSandbox => formatter.write_str("task has no running sandbox"),
        }
    }
}

impl Error for TaskError {}

#[derive(Debug)]
pub enum TaskServiceError {
    Agent(AgentError),
    Task(TaskError),
    Isolation(IsolationError),
    InvalidReport(String),
}

impl fmt::Display for TaskServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(error) => error.fmt(formatter),
            Self::Task(error) => error.fmt(formatter),
            Self::Isolation(error) => error.fmt(formatter),
            Self::InvalidReport(error) => write!(formatter, "invalid agent report: {error}"),
        }
    }
}

impl Error for TaskServiceError {}
