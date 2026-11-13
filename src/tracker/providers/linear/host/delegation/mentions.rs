use super::*;

impl RemoteTracker {
    // Both kinds of session event arrive this way, which is why this answers whether
    // there was a mention rather than doing something and saying so.
    pub(in crate::tracker::providers::linear::host) async fn mention_beside_task(
        &self,
        event: &SessionEvent,
        author: Option<&str>,
        body: Option<&str>,
        host: &dyn DelegatedWork,
    ) -> Result<bool, String> {
        let (Some(issue_id), true) = (
            event.issue_id.as_deref(),
            self.is_mention_session(event).await?,
        ) else {
            return Ok(false);
        };
        // The task holding the issue is what a mention is for. Asking for the report
        // as well made the message reach nothing whenever a task existed but this
        // machine was not reporting on it yet.
        let Some(task) = host.work_for_issue(issue_id.to_owned(), Vec::new()).await? else {
            return Ok(false);
        };
        if let Some(source_comment_id) = event
            .source_comment_id
            .as_ref()
            .or(event.comment_id.as_ref())
        {
            self.react_eyes(source_comment_id).await;
        }
        self.mention_into_task(
            &event.session_id,
            &task,
            author.unwrap_or_default(),
            body,
            host,
        )
        .await?;
        Ok(true)
    }

    async fn mention_into_task(
        &self,
        session_id: &str,
        task: &BoundTask,
        author: &str,
        body: Option<&str>,
        host: &dyn DelegatedWork,
    ) -> Result<(), String> {
        // A teammate's mention is a boundary case: acknowledge it and name the
        // operator, but do not run work for them.
        let operator = self.user_for_owner(&task.owner);
        if operator.as_deref() != Some(author) {
            let Some(operator) = operator else {
                tracing::info!(task = %task.task_id, "a mention arrived for work with no operator on this machine");
                return Ok(());
            };
            let body = self.boundary_reply(&operator).await;
            self.create_system_activity(
                session_id,
                &stable_id(session_id, "redirect"),
                AgentActivityContent::Response { body },
            )
            .await
            .map_err(|error| error.to_string())?;
            return Ok(());
        }
        let body = body.unwrap_or_default();
        if body.trim().is_empty() {
            return Ok(());
        }
        host.steer(
            task.task_id.clone(),
            format!(
                "A mention from the task's operator:\n\n{body}\n\n\
                 This comment was addressed to you. Continue the task with it."
            ),
        )
        .await?;
        self.create_system_activity(
            session_id,
            &stable_id(session_id, "already-running"),
            AgentActivityContent::Response {
                body: "Added to the task already running for this issue.".to_owned(),
            },
        )
        .await
        .map_err(|error| error.to_string())
    }

    // A delegation's comment is the generated session root; a mention's comment is a
    // human thread beside it. That difference is the only thing that tells the two
    // kinds of session apart.
    pub(in crate::tracker::providers::linear::host) async fn is_mention_session(
        &self,
        event: &SessionEvent,
    ) -> Result<bool, String> {
        let Some(comment_id) = event.comment_id.as_deref() else {
            return Ok(false);
        };
        Ok(!self
            .config
            .automation
            .comment_site(comment_id)
            .await
            .map_err(|error| error.to_string())?
            .is_artificial_session_root)
    }
}
