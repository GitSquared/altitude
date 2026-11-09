use serde_json::{Value, json};
use std::fmt;

use super::contribution_from_url;
use crate::agent::tool::{AgentTool, AgentToolError, AgentToolFuture, AgentToolProvider};
use crate::task::TaskStore;

pub struct VcsAgentTools {
    store: TaskStore,
    task_id: String,
}

impl VcsAgentTools {
    pub fn new(store: TaskStore, task_id: String) -> Self {
        Self { store, task_id }
    }

    async fn link_pull_request(&self, arguments: &Value) -> Result<Value, AgentToolError> {
        let url = arguments
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentToolError::new("vcs_link_pull_request needs a url"))?;
        let contribution = contribution_from_url(url).ok_or_else(|| {
            AgentToolError::new(format!(
                "{url:?} is not a pull request URL altitude can link; pass the full https URL the code host shows for a pull request"
            ))
        })?;
        if !self
            .store
            .is_kept(&self.task_id)
            .await
            .map_err(tool_failure)?
        {
            return Err(AgentToolError::new("this task is no longer in play"));
        }
        let (_, newly_linked) = self
            .store
            .link_code_contribution(&self.task_id, &contribution)
            .await
            .map_err(tool_failure)?;
        Ok(json!({
            "id": contribution.id,
            "url": contribution.url,
            "provider": contribution.provider,
            "repository": contribution.repository,
            "reference": contribution.reference,
            "newly_linked": newly_linked,
        }))
    }
}

fn tool_failure(error: impl fmt::Display) -> AgentToolError {
    AgentToolError::new(error.to_string())
}

impl AgentToolProvider for VcsAgentTools {
    fn namespace(&self) -> &'static str {
        "vcs"
    }

    fn tools(&self) -> Vec<AgentTool> {
        vec![AgentTool::new(
            "vcs_link_pull_request",
            "Link a pull request to this task so altitude can watch it. Call this once for every pull request you create or find, including ones opened from a fork or a different branch. Pass the full https URL; the result carries the canonical identity and whether this call is the one that linked it. altitude watches linked pull requests and wakes you when one needs attention, so do not run your own CI watcher.",
            json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The full https URL the code host shows for a pull request."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        )]
    }

    fn call(&self, name: &str, arguments: Value) -> AgentToolFuture<'_> {
        let name = name.to_owned();
        Box::pin(async move {
            match name.as_str() {
                "vcs_link_pull_request" => self.link_pull_request(&arguments).await,
                other => Err(AgentToolError::new(format!("unknown vcs tool {other}"))),
            }
        })
    }
}
