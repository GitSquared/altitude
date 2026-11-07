use serde_json::{Value, json};

use crate::task::{TaskPlanStatus, TaskPlanStep};
use crate::tracker::TrackerError;

use super::agent_sessions::AgentActivityContent;
use super::issues::mutation_succeeded;
use super::queries::*;
use super::*;

pub struct SessionStanding {
    pub can_receive: bool,
    pub never_answered: bool,
}

impl LinearAutomation {
    pub async fn update_session_summary(
        &self,
        session_id: &str,
        summary: &str,
    ) -> Result<(), TrackerError> {
        let data = self
            .graphql(
                UPDATE_SESSION_SUMMARY_MUTATION,
                json!({ "id": session_id, "input": { "summary": summary } }),
            )
            .await?;
        mutation_succeeded(&data, "agentSessionUpdate").map(|_| ())
    }

    pub async fn update_session_plan(
        &self,
        session_id: &str,
        steps: &[TaskPlanStep],
    ) -> Result<(), TrackerError> {
        let plan = steps
            .iter()
            .map(|step| {
                json!({
                    "content": step.content,
                    "status": match step.status {
                        TaskPlanStatus::Pending => "pending",
                        TaskPlanStatus::InProgress => "inProgress",
                        TaskPlanStatus::Completed => "completed",
                        TaskPlanStatus::Canceled => "canceled",
                    },
                })
            })
            .collect::<Vec<_>>();
        let data = self
            .graphql(
                UPDATE_SESSION_SUMMARY_MUTATION,
                json!({ "id": session_id, "input": { "plan": plan } }),
            )
            .await?;
        mutation_succeeded(&data, "agentSessionUpdate").map(|_| ())
    }

    pub async fn user_url(&self, user_id: &str) -> Result<String, TrackerError> {
        let data = self
            .graphql(USER_URL_QUERY, json!({ "id": user_id }))
            .await?;
        data.pointer("/user/url")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| TrackerError::new("Linear returned no profile URL for that user"))
    }

    pub async fn user_mention(&self, user_id: &str) -> Result<String, TrackerError> {
        let name = self.user_name(user_id).await?;
        Ok(format!("[@{name}](mention://user/{user_id})"))
    }

    pub async fn user_name(&self, user_id: &str) -> Result<String, TrackerError> {
        let data = self
            .graphql(USER_URL_QUERY, json!({ "id": user_id }))
            .await?;
        Ok(data
            .pointer("/user/name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("the operator")
            .to_owned())
    }

    pub async fn create_session_on_issue(&self, issue_id: &str) -> Result<String, TrackerError> {
        let data = self
            .graphql(
                CREATE_SESSION_ON_ISSUE_MUTATION,
                json!({ "input": { "issueId": issue_id } }),
            )
            .await?;
        let payload = mutation_succeeded(&data, "agentSessionCreateOnIssue")?;
        payload
            .pointer("/agentSession/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| TrackerError::new("Linear created the agent session but returned no id"))
    }

    // What a session is like before anything is said in it. Linear accepts an activity
    // for a session that is complete or dismissed and shows it nowhere, so asking is the
    // only way to know; and one this machine has never answered sits in Linear as
    // "thinking", which is a claim about work rather than a fact about it.
    pub async fn session_standing(
        &self,
        session_id: &str,
    ) -> Result<SessionStanding, TrackerError> {
        let data = self
            .graphql(SESSION_STATE_QUERY, json!({ "id": session_id }))
            .await?;
        let session = data
            .get("agentSession")
            .ok_or_else(|| TrackerError::new("Linear did not return that session"))?;
        let dismissed = session.get("dismissedAt").is_some_and(|at| !at.is_null());
        let status = session
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // `complete` is where a session rests once its last prompt has been answered, and
        // it takes the next activity like any other: what cannot be spoken in is a session
        // the operator dismissed, and the states Linear gives one that stopped working.
        Ok(SessionStanding {
            can_receive: !dismissed && !matches!(status, "stopping" | "error" | "stale"),
            never_answered: session.get("lastAgentReplyAt").is_none_or(Value::is_null),
        })
    }

    pub async fn create_activity(
        &self,
        agent_session_id: &str,
        activity_id: &str,
        content: AgentActivityContent,
    ) -> Result<(), TrackerError> {
        self.create_activity_with(agent_session_id, activity_id, content, false)
            .await
    }

    pub async fn create_ephemeral_thought(
        &self,
        agent_session_id: &str,
        activity_id: &str,
        body: String,
    ) -> Result<(), TrackerError> {
        self.create_activity_with(
            agent_session_id,
            activity_id,
            AgentActivityContent::Thought { body },
            true,
        )
        .await
    }

    async fn create_activity_with(
        &self,
        agent_session_id: &str,
        activity_id: &str,
        content: AgentActivityContent,
        ephemeral: bool,
    ) -> Result<(), TrackerError> {
        let input = json!({
            "agentSessionId": agent_session_id,
            "id": activity_id,
            "content": content,
            "ephemeral": ephemeral,
        });
        let outcome = self
            .graphql(CREATE_ACTIVITY_MUTATION, json!({ "input": input }))
            .await
            .and_then(|data| mutation_succeeded(&data, "agentActivityCreate").map(|_| ()));
        if let Err(error) = outcome {
            // The id belongs to this task's own history, so an activity sitting under
            // it is this activity and there is nothing left to do: a response can be
            // lost after creation, and a pass that stopped before advancing its
            // cursor replays the same event. Asking again would stop reporting for
            // good, which is what a stricter check did.
            let existing = self
                .graphql(
                    "query Activity($id: String!) { agentActivity(id: $id) { id } }",
                    json!({"id": activity_id}),
                )
                .await;
            if existing.as_ref().is_ok_and(|data| {
                data.pointer("/agentActivity/id").and_then(Value::as_str) == Some(activity_id)
            }) {
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    }
}
