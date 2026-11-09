use super::ledger::{CommentInput, CommentResolution, SessionEvent};
use crate::remote::StartRefusal;
use crate::remote::host::admission::{self, Actor, Admission, InputFacts};
use crate::tracker::ask_marker;
use crate::{
    task::TaskAsk,
    tracker::{TrackerKind, providers::linear::automation::AgentActivityContent},
};

use super::ledger::ReactionInput;
use super::*;
impl RemoteTracker {
    pub(super) async fn answer_prompt(
        &self,
        event: &SessionEvent,
        host: &dyn DelegatedWork,
    ) -> Result<(), String> {
        let Some(answer) = event.prompt_body.as_deref() else {
            return Ok(());
        };
        let Some(report) = self.report_for_session(&event.session_id) else {
            // No report for this session: it is either an operator's own prompt on
            // an issue still delegated to this app — which is how a session opened
            // from the issue starts work, and the retry path after a failed start —
            // or a session this machine is done with. Only a delegation the tracker
            // is actually asking for becomes work: a prompt on anything else must
            // not found a task nobody asked for.
            let Some(issue_id) = event.issue_id.as_deref() else {
                tracing::info!(session = %event.session_id, "a prompt arrived in a session with no issue");
                return Ok(());
            };
            let delegation = self
                .config
                .automation
                .issue_delegation(issue_id)
                .await
                .map_err(|error| error.to_string())?;
            // The tracker has to still be asking for this issue's work. Otherwise
            // the prompt is a memory of a session this machine is done with, and
            // founding a task from it is work nobody asked for.
            if delegation.delegate_id.as_deref() != Some(self.config.automation.app_user_id()) {
                tracing::info!(
                    session = %event.session_id,
                    issue = %issue_id,
                    "a prompt arrived in a session whose issue this app is not delegated"
                );
                return Ok(());
            }
            if let Some(author) = event.prompt_author_id.as_deref()
                && event.creator_id.as_deref() == Some(author)
                && self.owner_for_user(author).is_some()
            {
                let label = event.creator_name.as_deref().unwrap_or(author);
                return self
                    .delegate(issue_id, &event.session_id, author, label, host)
                    .await;
            }
            return Ok(());
        };
        // Only the current delegation operator's reply counts; a teammate's
        // prompt is not a decision. Say so on the session and name the operator
        // rather than silently ignoring a human who spoke to the app.
        if !event.is_from(&report.tracker_user_id) {
            let body = self.boundary_reply(&report.tracker_user_id).await;
            let discriminator = event
                .activity_id
                .as_deref()
                .unwrap_or(event.session_id.as_str());
            self.create_system_activity(
                &event.session_id,
                &stable_id(
                    &event.session_id,
                    &format!("teammate-redirect:{discriminator}"),
                ),
                AgentActivityContent::Response { body },
            )
            .await
            .map_err(|error| error.to_string())?;
            return Ok(());
        }
        let signals = host.signal_threads(report.task_id.clone()).await?;
        // A prompt answers only what was already waiting when it was written. A
        // prompt that predates a signal is stale, and letting it resolve the
        // newer question would answer something the operator never saw.
        let waiting: Vec<&TaskAsk> = signals
            .iter()
            .filter(|signal| signal.is_unresolved())
            .filter(|signal| signal.last_raised_at_ms <= event.occurred_at_ms)
            .collect();
        // An answer in the issue belongs in the ask it answers. One is open at a time,
        // so there is nothing to choose between; a task with none is steered directly.
        let Some(signal) = waiting.first().copied() else {
            self.note_wake(&report, "the operator answered").await;
            return host.steer(report.task_id.clone(), answer.to_owned()).await;
        };
        self.note_wake(&report, "the operator answered").await;
        host.reply(report.task_id.clone(), signal.id.clone(), answer.to_owned())
            .await
    }

    pub(super) async fn answer_comment(
        &self,
        event: &CommentInput,
        host: &dyn DelegatedWork,
    ) -> Result<(), String> {
        // Everything this machine says on an issue comes back as a delivery too. There
        // is nothing to read in it, and asking the tracker which thread it belongs to
        // is the only cost of finding that out the slow way.
        let app_user_id = self.config.automation.app_user_id();
        if event.actor_id == app_user_id {
            return Ok(());
        }
        let site = self
            .config
            .automation
            .comment_site(&event.comment_id)
            .await
            .map_err(|error| error.to_string())?;
        // The generated session root is Linear's own transcript surface, not a
        // signal comment. Its replies arrive as prompts on the session.
        if site.is_artificial_session_root {
            return Ok(());
        }
        let issue_id = site.issue_id.clone().or_else(|| event.issue_id.clone());
        let Some(issue_id) = issue_id else {
            tracing::debug!(comment = %event.comment_id, "a Linear comment belongs to no issue");
            return Ok(());
        };
        // The task holding the issue is what decides whether a comment has anywhere
        // to go. Asking whether *this pass* already reports on it made a message
        // reach nothing whenever the report was not there yet, and said nothing about
        // it: an operator writing on a delegated issue is the one thing this machine
        // must never swallow.
        let Some(task) = host.work_for_issue(issue_id.clone(), Vec::new()).await? else {
            tracing::info!(
                comment = %event.comment_id,
                issue = %issue_id,
                "a Linear comment reached no task on this machine"
            );
            return Ok(());
        };
        if !host.is_kept(task.task_id.clone()).await? {
            tracing::info!(comment = %event.comment_id, task = %task.task_id, "a Linear comment reached a task that is gone");
            return Ok(());
        }
        let report = self.report_for_task(&task.task_id);
        let operator = self.user_for_owner(&task.owner);
        let actor = if operator.as_deref() == Some(event.actor_id.as_str()) {
            Actor::Operator
        } else {
            Actor::OtherHuman
        };
        // A non-operator speaking is enough to make the thread shared, even if
        // Linear's author list is incomplete.
        let has_other_humans = actor == Actor::OtherHuman
            || site.authors_truncated
            || site
                .human_authors
                .iter()
                .any(|id| Some(id.as_str()) != operator.as_deref() && id.as_str() != app_user_id);
        // A comment on a thread this machine closed is the thread being reopened.
        // The tracker does not tell a separate story about an un-resolve, but a
        // reply in a closed thread is exactly that: nobody writes into an answered
        // question unless it is being asked again.
        self.reopen_closed_thread(&task.task_id, &site, host).await;
        // A reply inside a thread this machine published is an answer to the
        // question that thread asked, and the thread is which question it is. The
        // ids are derived from the task and the signal, so either side can work
        // them out and no table of published comments has to be kept.
        let root = site.root_comment_id.clone();
        let answered = host
            .signal_threads(task.task_id.clone())
            .await?
            .into_iter()
            .find(|thread| thread.is_unresolved() && ask_marker(&task.task_id, &thread.id) == root);
        let facts = InputFacts {
            actor,
            is_mention: event.is_mention,
            is_agent_thread: site.root_author_id.as_deref() == Some(app_user_id),
            is_agent_document_comment: site.document_content_id.is_some()
                && site.parent_id.is_none()
                && site.document_creator_id.as_deref() == Some(app_user_id),
            is_tracked_signal: answered.is_some(),
            is_task_issue: site.document_content_id.is_none()
                && site.issue_id.as_deref() == Some(issue_id.as_str())
                && site.parent_id.is_none(),
            has_other_humans,
        };
        let decision = admission::decide(&facts);
        tracing::info!(
            comment = %event.comment_id,
            root = %site.root_comment_id,
            site_comment = %site.comment_id,
            task = %task.task_id,
            ?decision,
            reason = decision.reason(),
            mention = event.is_mention,
            "Linear comment admitted"
        );
        match decision {
            Admission::Ignore | Admission::Context => Ok(()),
            Admission::Wake => {
                self.react_eyes(&event.comment_id).await;
                let body = if event.body.trim().is_empty() {
                    site.body.clone()
                } else {
                    event.body.clone()
                };
                if let Some(report) = &report {
                    self.note_wake(report, "the operator wrote on the issue")
                        .await;
                }
                if let Some(thread) = &answered {
                    // The reply answers the thread it is in. Whether an answer
                    // settles a question is the agent's to judge and it says so with
                    // its own resolve, and whether the *work* is accepted is not
                    // something words decide at all: the operator says that by
                    // moving the issue to Done, which is theirs to do and cannot be
                    // misread as a comment.
                    return host
                        .reply(task.task_id.clone(), thread.id.clone(), body)
                        .await;
                }
                let source = if site.document_id.is_some() {
                    "document"
                } else {
                    "issue"
                };
                let author = site.author_name.as_deref().unwrap_or("The operator");
                let instruction = format!(
                    "A comment from {author} on the task's {source}:\n\n{body}\n\n\
                     This comment was addressed to you. Continue the task with it."
                );
                host.steer(task.task_id, instruction).await
            }
            Admission::Redirect => {
                self.react_eyes(&event.comment_id).await;
                let Some(operator) = operator else {
                    tracing::info!(comment = %event.comment_id, "a teammate wrote on an issue this machine has no operator for");
                    return Ok(());
                };
                let body = self.boundary_reply(&operator).await;
                self.config
                    .automation
                    .comment_reply(
                        site.issue_id.as_deref(),
                        site.document_content_id.as_deref(),
                        &event.comment_id,
                        &body,
                        &stable_id("comment", &format!("{}:redirect", event.comment_id)),
                    )
                    .await
                    .map_err(|error| error.to_string())
            }
        }
    }

    // Closing a thread in the tracker is the operator saying what they would say in
    // the interface, and it is read the same way: the question needs nothing more, so
    // the task stops waiting on it — and, if that was what held it, it works again.
    pub(super) async fn answer_thread_resolution(
        &self,
        event: &CommentResolution,
        host: &dyn DelegatedWork,
    ) -> Result<(), String> {
        let site = self
            .config
            .automation
            .comment_site(&event.comment_id)
            .await
            .map_err(|error| error.to_string())?;
        let Some(issue_id) = site.issue_id.clone() else {
            return Ok(());
        };
        let Some(task) = host.work_for_issue(issue_id, Vec::new()).await? else {
            tracing::info!(comment = %event.comment_id, "a thread was closed on an issue no task holds");
            return Ok(());
        };
        if !host.is_kept(task.task_id.clone()).await? {
            return Ok(());
        }
        let root = site.root_comment_id.clone();
        let thread = host
            .signal_threads(task.task_id.clone())
            .await?
            .into_iter()
            .find(|thread| ask_marker(&task.task_id, &thread.id) == root);
        let Some(thread) = thread else {
            return Ok(());
        };
        if thread.is_resolved() == event.resolved {
            return Ok(());
        }
        host.set_signal_resolved(task.task_id.clone(), thread.id.clone(), event.resolved)
            .await?;
        tracing::info!(
            task = %task.task_id,
            signal = %thread.id,
            resolved = event.resolved,
            "the operator closed or reopened a thread in the tracker"
        );
        Ok(())
    }

    // A reaction is the operator answering with a symbol. What it means is theirs and
    // the agent's to read: this says what happened in the thread the comment belongs
    // to, and leaves every judgement about it to the one who has the tools to make it.
    pub(super) async fn answer_reaction(
        &self,
        event: &ReactionInput,
        host: &dyn DelegatedWork,
    ) -> Result<(), String> {
        // This machine's own reaction — the acknowledgement it adds to a comment it has
        // read — is not the operator answering anything.
        if event.actor_id == self.config.automation.app_user_id() {
            return Ok(());
        }
        let site = self
            .config
            .automation
            .comment_site(&event.comment_id)
            .await
            .map_err(|error| error.to_string())?;
        if site.is_artificial_session_root {
            return Ok(());
        }
        let issue_id = site.issue_id.clone();
        let Some(issue_id) = issue_id else {
            return Ok(());
        };
        let Some(task) = host.work_for_issue(issue_id, Vec::new()).await? else {
            return Ok(());
        };
        // Only the operator directs this task. A teammate's symbol is not a decision,
        // and this machine's own acknowledgements are not anybody speaking.
        if self.user_for_owner(&task.owner).as_deref() != Some(event.actor_id.as_str()) {
            return Ok(());
        }
        if !host.is_kept(task.task_id.clone()).await? {
            return Ok(());
        }
        let root = site.root_comment_id.clone();
        let thread = host
            .signal_threads(task.task_id.clone())
            .await?
            .into_iter()
            .find(|thread| ask_marker(&task.task_id, &thread.id) == root);
        let Some(thread) = thread else {
            return Ok(());
        };
        let about = site
            .body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("a comment in this thread");
        let said = format!(
            "The operator reacted {} to this comment:\n\n{about}",
            event.emoji
        );
        if let Some(report) = self.report_for_task(&task.task_id) {
            self.note_wake(&report, "the operator reacted to a message")
                .await;
        }
        host.reply(task.task_id, thread.id, said).await
    }

    pub(super) async fn react_eyes(&self, comment_id: &str) {
        let id = stable_id("reaction", &format!("{comment_id}:eyes"));
        if let Err(error) = self
            .config
            .automation
            .react_to_comment(comment_id, "eyes", Some(&id))
            .await
        {
            tracing::debug!(comment = comment_id, %error, "could not add the acknowledgement reaction");
        }
    }

    pub(super) async fn boundary_reply(&self, operator_id: &str) -> String {
        let operator = self.operator_mention(operator_id).await;
        format!(
            "This task belongs to {operator}, so altitude only takes direction from them here. \
             Ask them to reply if you want it changed."
        )
    }

    pub(super) async fn summarise_session(
        &self,
        session_id: &str,
        operator_id: &str,
        agent: Option<&str>,
    ) -> Result<(), String> {
        let operator = self.operator_handle(operator_id).await;
        let agent = agent.unwrap_or("agent");
        let summary = format!("altitude / {operator}@{} / {agent}", self.config.host_label);
        self.config
            .automation
            .update_session_summary(session_id, &summary)
            .await
            .map_err(|error| error.to_string())
    }

    pub(super) async fn operator_handle(&self, user_id: &str) -> String {
        match self.config.automation.user_url(user_id).await {
            Ok(url) => url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|handle| !handle.is_empty())
                .unwrap_or("operator")
                .to_owned(),
            Err(_) => "operator".to_owned(),
        }
    }

    pub(super) async fn operator_mention(&self, user_id: &str) -> String {
        match self.config.automation.user_mention(user_id).await {
            Ok(mention) if !mention.is_empty() => mention,
            Ok(_) | Err(_) => {
                tracing::warn!(user = user_id, "could not read the operator's profile URL");
                "the person who delegated it".to_owned()
            }
        }
    }

    // The operator's own name, for the lines that say who a task is waiting on: a
    // tracker is shared with a team, and "you" is only one of the people reading it.
    pub(super) async fn operator_name(&self, user_id: &str) -> String {
        match self.config.automation.user_name(user_id).await {
            Ok(name) if !name.trim().is_empty() => name,
            Ok(_) | Err(_) => "the operator".to_owned(),
        }
    }

    pub(super) async fn post_sync_guidance(&self, session_id: &str) -> Result<(), String> {
        let tracker = TrackerKind::Linear.label();
        let body = format!(
            "**altitude does not know whose work this is.**\n\n\
             This remote holds no {tracker} login from a person it could report to, so it \
             cannot start a task for you.\n\n\
             On your computer, in Settings → Task tracker, sign in to {tracker} as \
             yourself. The login is passed to this remote in the background; reply here \
             once that's done."
        );
        self.create_system_activity(
            session_id,
            &stable_id(session_id, "sync-error"),
            AgentActivityContent::Error { body },
        )
        .await
        .map_err(|error| error.to_string())
    }

    pub(super) async fn post_start_guidance(
        &self,
        session_id: &str,
        refusal: &StartRefusal,
    ) -> Result<(), String> {
        let body = format!(
            "**Nothing was started.**\n\n{refusal}\n\n\
             Reply here once that's sorted, and altitude will try again."
        );
        self.create_system_activity(
            session_id,
            &stable_id(session_id, "start-error"),
            AgentActivityContent::Error { body },
        )
        .await
        .map_err(|error| error.to_string())
    }
}
