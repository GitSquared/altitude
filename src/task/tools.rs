//! What the `tasks` subsystem offers the agent running a task.
//!
//! The agent tools are declared and answered here, beside the task record they
//! act on, under the `tasks` namespace. The MCP server binds them by name and
//! knows none of them.

use serde_json::{Value, json};
use std::fmt;

use super::{TaskHistoryEvent, TaskStore, normalize_title};
use crate::agent::tools::{AgentTool, AgentToolError, AgentToolFuture, AgentToolProvider};

/// The `tasks` agent tools, bound to one task's record.
pub struct TaskAgentTools {
    store: TaskStore,
    task_id: String,
}

impl TaskAgentTools {
    pub fn new(store: TaskStore, task_id: String) -> Self {
        Self { store, task_id }
    }

    fn no_arguments() -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn get_title(&self) -> Result<Value, AgentToolError> {
        let task = self.task().await?;
        Ok(json!({ "title": task.title() }))
    }

    async fn set_title(&self, arguments: &Value) -> Result<Value, AgentToolError> {
        let requested = arguments
            .get("title")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentToolError::new("tasks_set_title needs a title"))?;
        let title = normalize_title(requested)
            .ok_or_else(|| AgentToolError::new("a task title cannot be blank"))?;
        let task = self.task().await?;
        if task.facts.title.as_deref() == Some(title.as_str()) {
            return Ok(json!({ "title": title, "changed": false }));
        }
        self.store
            .append_events(
                &self.task_id,
                &[TaskHistoryEvent::TaskTitleSet {
                    title: title.clone(),
                }],
            )
            .await
            .map_err(tool_failure)?;
        Ok(json!({ "title": title, "changed": true }))
    }

    async fn list_signals(&self) -> Result<Value, AgentToolError> {
        let signals = self
            .store
            .operator_signals(&self.task_id)
            .await
            .map_err(tool_failure)?;
        Ok(json!({ "signals": signals }))
    }

    async fn task(&self) -> Result<super::Task, AgentToolError> {
        self.store
            .get(&self.task_id)
            .await
            .map_err(tool_failure)?
            .ok_or_else(|| AgentToolError::new("this task is no longer in play"))
    }
}

fn tool_failure(error: impl fmt::Display) -> AgentToolError {
    AgentToolError::new(error.to_string())
}

impl AgentToolProvider for TaskAgentTools {
    fn namespace(&self) -> &'static str {
        "tasks"
    }

    fn tools(&self) -> Vec<AgentTool> {
        vec![
            AgentTool::new(
                "tasks_get_title",
                "The task's current title, as the operator's list shows it.",
                Self::no_arguments(),
            ),
            AgentTool::new(
                "tasks_set_title",
                "Name the task for the operator's list: at most six words for what this task has become from the work done so far, not a restatement of the original request. Refresh it as the work progresses so the list stays a current summary.",
                json!({
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "The new title, at most six words." }
                    },
                    "required": ["title"],
                    "additionalProperties": false
                }),
            ),
            AgentTool::new(
                "tasks_list_signals",
                "The operator signals raised on this task that are still open. Consult it when you want to know what the operator has been asked, instead of guessing.",
                Self::no_arguments(),
            ),
        ]
    }

    fn call(&self, name: &str, arguments: Value) -> AgentToolFuture<'_> {
        let name = name.to_owned();
        Box::pin(async move {
            match name.as_str() {
                "tasks_get_title" => self.get_title().await,
                "tasks_set_title" => self.set_title(&arguments).await,
                "tasks_list_signals" => self.list_signals().await,
                other => Err(AgentToolError::new(format!("unknown tasks tool {other}"))),
            }
        })
    }
}
