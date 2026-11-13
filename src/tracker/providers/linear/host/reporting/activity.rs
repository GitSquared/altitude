use super::*;

impl RemoteTracker {
    // A write that fails is not a session that has ended: the line is only recorded once
    // Linear has it, so the next pass says the same thing in the same thread. What is
    // left is asking on the way up whether a session a previous run left behind can still
    // receive, which `verify_sessions` does.
    async fn write_activity(
        &self,
        report: &Cursor,
        activity_id: &str,
        content: AgentActivityContent,
    ) -> Result<(), TrackerError> {
        self.config
            .automation
            .create_activity(&report.session, activity_id, content)
            .await
    }

    pub(in crate::tracker::providers::linear::host) async fn say_system(
        &self,
        report: &Cursor,
        activity_id: &str,
        content: AgentActivityContent,
    ) -> Result<(), TrackerError> {
        self.write_activity(report, activity_id, content.system())
            .await
    }

    pub(in crate::tracker::providers::linear::host) async fn say_agent(
        &self,
        report: &Cursor,
        activity_id: &str,
        content: AgentActivityContent,
    ) -> Result<(), TrackerError> {
        self.write_activity(report, activity_id, content).await
    }

    pub(in crate::tracker::providers::linear::host) async fn say_system_ephemerally(
        &self,
        report: &Cursor,
        activity_id: &str,
        body: String,
    ) -> Result<(), TrackerError> {
        self.write_ephemeral(report, activity_id, brand::mark(body))
            .await
    }

    pub(in crate::tracker::providers::linear::host) async fn say_agent_ephemerally(
        &self,
        report: &Cursor,
        activity_id: &str,
        body: String,
    ) -> Result<(), TrackerError> {
        self.write_ephemeral(report, activity_id, body).await
    }

    async fn write_ephemeral(
        &self,
        report: &Cursor,
        activity_id: &str,
        body: String,
    ) -> Result<(), TrackerError> {
        self.config
            .automation
            .create_ephemeral_thought(&report.session, activity_id, body)
            .await
    }

    // The paths that write into a session before a report exists still sign the
    // machine's words; they just have no report to mark broken if the write fails.
    pub(in crate::tracker::providers::linear::host) async fn create_system_activity(
        &self,
        session_id: &str,
        activity_id: &str,
        content: AgentActivityContent,
    ) -> Result<(), TrackerError> {
        self.config
            .automation
            .create_activity(session_id, activity_id, content.system())
            .await
    }

    pub(in crate::tracker::providers::linear::host) async fn set_plan(
        &self,
        report: &Cursor,
        steps: &[TaskPlanStep],
    ) -> Result<(), TrackerError> {
        self.config
            .automation
            .update_session_plan(&report.session, steps)
            .await
    }

    // What the agent says and what it is thinking both go to the session as they happen:
    // the operator watching the issue should not have to take the task over to find out
    // whether it is still working. They are different things and they are published
    // differently — what it says is part of the transcript, and what it is thinking is
    // the state it is in, replaced by the next thing this machine says. Neither is a
    // reply: nothing here is addressed to anybody and nothing here is news worth a
    // notification.
    pub(in crate::tracker::providers::linear::host) async fn report_activity(
        &self,
        host: &dyn DelegatedWork,
    ) {
        let bound = match host.bound_tasks().await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::debug!(%error, "could not read bound tasks to report activity");
                return;
            }
        };
        for task in bound {
            let activity = match host.current_activity(task.task_id.clone()).await {
                Ok(Some(activity)) if !activity.text.trim().is_empty() => activity,
                _ => continue,
            };
            let Some(report) = self.report_for_task(&task.task_id) else {
                continue;
            };
            // The count and the last line are the report's own bookkeeping rather than
            // this process's: naming an activity from a counter kept in memory meant a
            // restart reused ids Linear already had, and every publish after it was
            // accepted as a duplicate of something nobody could see.
            let (said, last_line) = self.activity_state(&task.task_id);
            if last_line.as_deref() == Some(activity.text.as_str()) {
                continue;
            }
            let published = said.saturating_add(1);
            let activity_id = stable_id(
                &report.task_id,
                &format!("activity-{}:{published}", self.started_at_ms),
            );
            let published_activity = match activity.kind {
                ActivityKind::Thinking => {
                    self.say_agent_ephemerally(&report, &activity_id, activity.text.clone())
                        .await
                }
                // What the agent says is what it has concluded, and a conclusion is
                // the one thing in a run that is addressed to a person: as a message it
                // is read where the operator already is, and in the tracker it lands in
                // the session's own thread instead of being folded into the reasoning.
                // The machine's own news is written where it does not read as a
                // message, and it already carries the mountain.
                ActivityKind::Altitude => {
                    self.say_system(
                        &report,
                        &activity_id,
                        AgentActivityContent::Thought {
                            body: activity.text.clone(),
                        },
                    )
                    .await
                }
                ActivityKind::Speech | ActivityKind::Remote => {
                    let said = self
                        .say_agent(
                            &report,
                            &activity_id,
                            AgentActivityContent::Response {
                                body: activity.text.clone(),
                            },
                        )
                        .await;
                    // A message finishes the turn it is sent in, and a run says several
                    // things before it stops: without this the session shows "finished"
                    // between the agent's own sentences. The note after it is the
                    // machine saying it has not stopped, and it is replaced by the next
                    // thing the run says either way.
                    if said.is_ok() && task.status == TaskExecutionStatus::Running {
                        self.say_system_ephemerally(
                            &report,
                            &stable_id(
                                &report.task_id,
                                &format!("working-{}:{published}", self.started_at_ms),
                            ),
                            "Working\u{2026}".to_owned(),
                        )
                        .await
                    } else {
                        said
                    }
                }
            };
            if let Err(error) = published_activity {
                tracing::debug!(task = %task.task_id, %error, "could not say what an agent is doing");
                continue;
            }
            if let Err(error) = self.record_activity(&task.task_id, published, activity.text) {
                tracing::warn!(task = %task.task_id, %error, "could not record what was said in a session");
            }
        }
    }
}
