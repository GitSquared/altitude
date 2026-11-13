use super::*;

// Long enough for one request per bound task over a network that is about to go
// away, short enough to leave the guest's own shutdown most of its minute.
const SHUTDOWN_NOTICE_BUDGET: Duration = Duration::from_secs(10);

impl RemoteTracker {
    pub async fn start_reporting(
        &self,
        owner: &str,
        task_id: String,
        issue: IssueReference,
        delegation: Option<String>,
        session_agent: Option<String>,
    ) -> Result<(), String> {
        let Some(tracker_user_id) = self.user_for_owner(owner) else {
            return Err("sync a Linear account to this remote before delegating issue work".into());
        };
        // One cursor per task: the task is what a report is about, and a second
        // record keyed by the issue is what used to make two mechanisms disagree
        // about which task was current.
        if self
            .report_for_task(&task_id)
            .is_some_and(|report| !report.finished)
        {
            return Ok(());
        }
        // The session is the issue's, not the delegation's. An issue has one
        // conversation, and what the operator reads is that thread: a task that
        // replaces another on the same issue carries it on. A delegation's own session
        // is where the operator clicked, so it is used the first time and only when the
        // issue has nothing to carry.
        let candidate = match self.session_for_issue(&issue.id) {
            Some(session) => Some(session),
            None => delegation,
        };
        let session_id = match candidate {
            // One the operator or Linear has finished with takes nothing more: asking
            // first is the only way to know, because Linear accepts the activity and
            // then shows it nowhere.
            Some(candidate) => match self.config.automation.session_standing(&candidate).await {
                Ok(standing) if standing.can_receive => candidate,
                Ok(_) => self.open_session(&issue.id).await?,
                // A tracker that cannot be asked is not a reason to report nowhere, and
                // the session is as likely to be usable as not.
                Err(error) => {
                    tracing::debug!(session = %candidate, %error, "could not ask whether a session can receive");
                    candidate
                }
            },
            None => self.open_session(&issue.id).await?,
        };
        self.upsert_report(Cursor {
            issue_id: issue.id.clone(),
            issue_identifier: issue.identifier.clone(),
            session: session_id.clone(),
            owner: owner.to_owned(),
            tracker_user_id: tracker_user_id.clone(),
            task_id,
            created_at_ms: now_ms(),
            reported_events: 0,
            finished: false,
            last_status: None,
            failed_at: None,
            failed_attempts: 0,
            unhooked_at: None,
            activities: 0,
            activity_line: None,
        })?;
        // Work started on an issue is a new cycle: whatever ending was written for
        // the last task there does not bind this one, and the operator asked for
        // this by starting it.
        self.mutate(|ledger| ledger.settled_issues.retain(|id| id != &issue.id))?;
        if let Err(error) = self
            .summarise_session(&session_id, &tracker_user_id, session_agent.as_deref())
            .await
        {
            tracing::warn!(session = %session_id, %error, "could not name a piloted session");
        }
        Ok(())
    }

    pub async fn forget_task(&self, task_id: &str) {
        let Some(report) = self.report_for_task(task_id) else {
            return;
        };
        if let Err(error) = self
            .config
            .automation
            .release_issue(&self.issue_reference(&report.issue_id))
            .await
        {
            tracing::warn!(issue = %report.issue_id, %error, "could not release a deleted task's issue");
        }
        if let Err(error) = self
            .say_system(
                &report,
                &stable_id(task_id, "deleted"),
                AgentActivityContent::Response {
                    body: "This task was deleted, with its sandbox and its record.".to_owned(),
                },
            )
            .await
        {
            tracing::warn!(task = %task_id, %error, "could not say a task was deleted");
        }
        if let Err(error) = self.finish_report(&report) {
            tracing::warn!(task = %task_id, %error, "could not close a deleted task's report");
        }
    }

    // The machine is going down under this work, and a session that simply stops
    // reads as a hang to whoever is watching the issue. This says what is happening
    // before the process goes, and claims nothing about what survives: the stop is
    // best effort, and what happens to each task is decided by the boot after it.
    // Bounded, because the guest is shutting down and the tasks matter more.
    pub async fn announce_shutdown(&self, host: &dyn DelegatedWork) {
        let bound = match host.bound_tasks().await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::debug!(%error, "could not read bound tasks to say the machine is stopping");
                return;
            }
        };
        let shutdown = now_ms() / 1000;
        let automation = Arc::clone(&self.config.automation);
        let mut notices = Vec::new();
        for task in bound {
            let Some(report) = self.report_for_task(&task.task_id) else {
                continue;
            };
            // Only a run that is happening stops. A task waiting on the operator or one
            // still queued has nothing for the session to explain, and saying it does is
            // what makes the session contradict itself the next time the task speaks.
            if task.status != TaskExecutionStatus::Running {
                continue;
            }
            let session = report.session;
            let activity = stable_id(&report.task_id, &format!("shutdown:{shutdown}"));
            let automation = Arc::clone(&automation);
            notices.push(async move {
                // A status rather than a message: it says why the session goes quiet, and
                // the first thing this machine says when it is back replaces it.
                automation
                    .create_ephemeral_thought(
                        &session,
                        &activity,
                        brand::mark(
                            "altitude is shutting down, so this run stops here. The task is not \
                             finished: altitude picks it back up when the machine is back.",
                        ),
                    )
                    .await
            });
        }
        if notices.is_empty() {
            return;
        }
        match timeout(SHUTDOWN_NOTICE_BUDGET, join_all(notices)).await {
            Ok(results) => {
                for result in results.into_iter().filter_map(Result::err) {
                    tracing::debug!(%result, "could not say the machine is stopping");
                }
            }
            Err(_) => tracing::warn!(
                budget_s = SHUTDOWN_NOTICE_BUDGET.as_secs(),
                "saying the machine is stopping took too long"
            ),
        }
    }

    async fn open_session(&self, issue_id: &str) -> Result<String, String> {
        self.config
            .automation
            .create_session_on_issue(issue_id)
            .await
            .map_err(|error| error.to_string())
    }

    // A report already in play when this machine starts is checked once, on the way up:
    // Linear accepts activity for a session it has since finished with and shows it
    // nowhere, so a report can look healthy and be talking into a thread nobody has.
    // Checking on every pass would be asking Linear on a clock; asking once is enough
    // for the session a previous run left behind.
    pub(in crate::tracker::providers::linear::host) async fn verify_sessions(
        &self,
        host: &dyn DelegatedWork,
    ) {
        for report in self.cursors().into_iter().filter(|report| !report.finished) {
            match self
                .config
                .automation
                .session_standing(&report.session)
                .await
            {
                Ok(standing) if !standing.can_receive => self.replace_session(host, &report).await,
                Ok(standing) if standing.never_answered => {
                    self.introduce(host, &report).await;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(session = %report.session, %error, "could not ask whether a session can receive");
                }
            }
        }
    }

    // A session with a prompt this machine never answered sits in Linear as "thinking",
    // which says work is happening when the task may have been paused for hours. One
    // line says where it actually stands.
    async fn introduce(&self, host: &dyn DelegatedWork, report: &Cursor) {
        let Ok(bound) = host.bound_tasks().await else {
            return;
        };
        let Some(task) = bound.iter().find(|task| task.task_id == report.task_id) else {
            return;
        };
        self.introduce_task(report, task).await;
    }

    async fn introduce_task(&self, report: &Cursor, task: &BoundTask) {
        let operator = self.operator_name(&report.tracker_user_id);
        let body = match (&task.status, task.pause_reason.as_ref()) {
            (TaskExecutionStatus::Running, _) => {
                format!(
                    "altitude is working on this task on {}.",
                    self.config.host_label
                )
            }
            (TaskExecutionStatus::Paused, Some(reason)) if waits_on_a_person(reason) => {
                pause_message(reason, &operator, task.ask)
            }
            (TaskExecutionStatus::Paused, Some(reason)) => reason.notice().to_owned(),
            (TaskExecutionStatus::Pending, _) => format!(
                "altitude has this task queued on {}; it starts when the machine has room.",
                self.config.host_label
            ),
            _ => format!(
                "altitude is reporting on this task from {}.",
                self.config.host_label
            ),
        };
        if let Err(error) = self
            .say_system(
                report,
                &stable_id(&report.task_id, &format!("introduced:{}", report.session)),
                AgentActivityContent::Response { body },
            )
            .await
        {
            tracing::debug!(task = %report.task_id, %error, "could not say where a task stands");
        }
    }

    // A session can go away under a report: dismissed in Linear, or closed once the work
    // it carried ended. A failure is not how that is known — a line Linear refused is one
    // the next pass says again — so this is only reached by the paths that ask Linear
    // whether a session can still receive: the way up, and the start of a task.
    async fn replace_session(&self, host: &dyn DelegatedWork, report: &Cursor) {
        let session = match self.open_session(&report.issue_id).await {
            Ok(session) => session,
            Err(error) => {
                tracing::warn!(task = %report.task_id, %error, "could not open a session for a report");
                return;
            }
        };
        if let Err(error) = self.move_session(&report.task_id, &session) {
            tracing::warn!(task = %report.task_id, %error, "could not move a report to a new session");
            return;
        }
        let agent = host
            .session_agent(report.task_id.clone())
            .await
            .ok()
            .flatten();
        if let Err(error) = self
            .summarise_session(&session, &report.tracker_user_id, agent.as_deref())
            .await
        {
            tracing::warn!(session = %session, %error, "could not name a new session");
        }
        tracing::info!(
            task = %report.task_id,
            issue = %report.issue_id,
            session = %session,
            "a session had gone away, so the report has a new one"
        );
        // A new session arrives with a prompt nobody has answered, and Linear says it is
        // thinking until something is said in it. The line belongs in the session that was
        // just opened: the one it replaced takes nothing more, and saying it there is how a
        // dead thread looks like it is still in use.
        let moved = self
            .report_for_task(&report.task_id)
            .unwrap_or_else(|| report.clone());
        self.introduce(host, &moved).await;
    }

    // A report that begins on a task which was already running says where that task
    // stands now, rather than replaying the turns it took before this machine was
    // watching: the operator reads the issue as one conversation, and a transcript of
    // last week's turn arriving today is this machine talking to itself.
    pub(in crate::tracker::providers::linear::host) async fn project_state(
        &self,
        host: &dyn DelegatedWork,
        report: &Cursor,
        events: &[(i64, TaskHistoryEvent)],
        threads: &[TaskAsk],
    ) -> Result<(), String> {
        if let Some(steps) = events.iter().rev().find_map(|(_, event)| match event {
            TaskHistoryEvent::PlanUpdated { steps } => Some(steps),
            _ => None,
        }) {
            self.set_plan(report, steps)
                .await
                .map_err(|error| error.to_string())?;
        }
        self.ensure_asks_said(host, report, threads, events).await?;
        let asking = host
            .bound_tasks()
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|task| task.task_id == report.task_id);
        let status = match asking.as_ref().map(|task| (task.status.clone(), task.ask)) {
            Some((TaskExecutionStatus::Running | TaskExecutionStatus::Pending, _)) => {
                Some(TrackerStatus::InProgress)
            }
            Some((TaskExecutionStatus::Paused, Some(TaskAskKind::ReadyForReview))) => {
                Some(TrackerStatus::InReview)
            }
            Some((TaskExecutionStatus::Completed, _)) if !self.is_settled(&report.issue_id) => {
                Some(TrackerStatus::Done)
            }
            _ => None,
        };
        if let Some(status) = status {
            self.write_status(host, report, status).await?;
        }
        if let Some(task) = asking.as_ref() {
            self.introduce_task(report, task).await;
        }
        Ok(())
    }
}
