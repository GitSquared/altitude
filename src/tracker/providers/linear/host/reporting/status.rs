use super::*;

impl RemoteTracker {
    pub(in crate::tracker::providers::linear::host) async fn write_status(
        &self,
        host: &dyn DelegatedWork,
        report: &Cursor,
        status: TrackerStatus,
    ) -> Result<(), String> {
        let held = self.report_for_task(&report.task_id);
        if held.as_ref().and_then(|held| held.last_status) == Some(status) {
            return Ok(());
        }
        self.config
            .automation
            .status(&self.issue_reference(&report.issue_id), status)
            .await
            .map_err(|error| error.to_string())?;
        // The task's own history says what the issue says, so a card reads it
        // from the record rather than a second field that can disagree.
        if let Err(error) = host.record_status(report.task_id.clone(), status).await {
            tracing::warn!(task = %report.task_id, %error, "could not record the issue's state");
        }
        // The write happened, so a failure to remember it costs one repeat at
        // most, which is not worth failing the pass over.
        if let Err(error) = self.mutate(|ledger| {
            if let Some(held) = ledger
                .cursors
                .iter_mut()
                .find(|held| held.task_id == report.task_id)
            {
                held.last_status = Some(status);
            }
        }) {
            tracing::warn!(task = %report.task_id, %error, "could not record a status already written");
        }
        Ok(())
    }

    pub(in crate::tracker::providers::linear::host) fn forget_status(
        &self,
        issue_id: &str,
    ) -> Result<(), String> {
        self.mutate(|ledger| {
            for held in ledger
                .cursors
                .iter_mut()
                .filter(|held| held.issue_id == issue_id)
            {
                held.last_status = None;
            }
        })
    }

    pub(in crate::tracker::providers::linear::host) async fn finish_accepted(
        &self,
        host: &dyn DelegatedWork,
        report: &Cursor,
        events: &[(i64, TaskHistoryEvent)],
    ) -> Result<(), String> {
        // Accepted is finished, whichever door the acceptance came through, so the
        // threads the task was waiting in close here rather than in each caller.
        self.resolve_signal_comments(report, events).await;
        if !self.is_settled(&report.issue_id) {
            self.write_status(host, report, TrackerStatus::Done).await?;
            self.settle(&report.issue_id)?;
        }
        // The activity is what the session shows, not the issue's state: a failure
        // to say it must not hold the cursor on an ending that has been written.
        if let Err(error) = self
            .say_system(
                report,
                &stable_id(&report.task_id, "accepted"),
                AgentActivityContent::Response {
                    body: accepted_body(&contribution_urls(events)),
                },
            )
            .await
        {
            tracing::warn!(task = %report.task_id, %error, "could not post an acceptance answer");
        }
        self.finish_report(report)
    }

    pub(in crate::tracker::providers::linear::host) async fn finish_transferred(
        &self,
        report: &Cursor,
    ) -> Result<(), String> {
        self.say_system(
            report,
            &stable_id(&report.task_id, "transferred"),
            AgentActivityContent::Response {
                body: "The operator handed this task to another machine, and it keeps \
                       running there."
                    .to_owned(),
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        self.finish_report(report)
    }
}
