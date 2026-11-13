use super::*;

impl RemoteTracker {
    pub(in crate::tracker::providers::linear::host) async fn publish_signal(
        &self,
        report: &Cursor,
        signal_id: &str,
        kind: TaskAskKind,
        title: &str,
        body: &str,
        contributions: &[String],
    ) -> Result<Option<TrackerStatus>, String> {
        let marker = ask_marker(&report.task_id, signal_id);
        let body = self.hosted_body(body).await;
        let (body, status) = match kind {
            TaskAskKind::BlockingQuestion => {
                // A question is not a review: the work is still in progress, and
                // moving the issue would tell a reader the milestone is finished.
                // The ask is what asks for attention.
                let who = self.operator_mention(&report.tracker_user_id);
                (
                    format!("**? Needs you:** {}\n\n{who} {}", title.trim(), body.trim()),
                    None,
                )
            }
            TaskAskKind::ReadyForReview => {
                let who = self.operator_mention(&report.tracker_user_id);
                let mut body = format!(
                    "**◆ Ready to review:** {}\n\n{who} {}",
                    title.trim(),
                    body.trim()
                );
                append_contributions(&mut body, contributions);
                (body, Some(TrackerStatus::InReview))
            }
        };
        self.config
            .automation
            .open_ask(&self.issue_reference(&report.issue_id), &marker, &body)
            .await
            .map_err(|error| error.to_string())?;
        Ok(status)
    }

    // A reply is posted into the discussion it answers, idempotently: a cursor can
    // replay an event it never managed to finish, and a second copy of a sentence is
    // the one thing a conversation cannot recover from.
    pub(in crate::tracker::providers::linear::host) async fn post_signal_reply(
        &self,
        report: &Cursor,
        signal_id: &str,
        body: &str,
    ) {
        let marker = ask_marker(&report.task_id, signal_id);
        match self.config.automation.has_ask(&marker).await {
            Ok(true) => {}
            // A discussion this machine never opened has nowhere to put a reply.
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(task = %report.task_id, signal = signal_id, %error, "could not read a signal's discussion");
                return;
            }
        }
        let id = ask_reply_id(&report.task_id, signal_id, body);
        if let Err(error) = self
            .config
            .automation
            .answer_ask(&self.issue_reference(&report.issue_id), &marker, &id, body)
            .await
        {
            tracing::warn!(
                task = %report.task_id,
                signal = signal_id,
                %error,
                "could not post the agent's reply to a discussion"
            );
        }
    }

    pub(in crate::tracker::providers::linear::host) async fn follow_signal_thread(
        &self,
        report: &Cursor,
        signal_id: &str,
        resolved: bool,
        settled: Option<&TaskAskMessage>,
    ) {
        let marker = ask_marker(&report.task_id, signal_id);
        // A signal resolved before this machine could report it has no discussion to
        // close: asking first is how that stays a fact rather than an error the
        // provider might also be giving for a real failure.
        match self.config.automation.has_ask(&marker).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(task = %report.task_id, signal = signal_id, %error, "could not read a signal's discussion");
                return;
            }
        }
        // Which comment closed the thread, when the agent named one of its own words.
        let resolving = settled.and_then(|message| match message.author {
            // A reply this machine published has an id only this side derives, and it
            // was published under exactly that id.
            TaskAskAuthor::Agent => Some(ask_reply_id(&report.task_id, signal_id, &message.body)),
            // A person's comment has an id in the tracker already, and this machine
            // never kept it: the provider names their last word in the thread itself.
            TaskAskAuthor::Operator => None,
        });
        if let Err(error) = self
            .config
            .automation
            .close_ask(&marker, resolved, resolving.as_deref())
            .await
        {
            tracing::warn!(
                task = %report.task_id,
                signal = signal_id,
                resolved,
                %error,
                "could not follow a signal's discussion in the tracker"
            );
        }
    }

    pub(in crate::tracker::providers::linear::host) async fn resolve_signal_comments(
        &self,
        report: &Cursor,
        events: &[(i64, TaskHistoryEvent)],
    ) {
        let mut seen = HashSet::new();
        for (_, event) in events {
            let TaskHistoryEvent::OperatorSignalRaised { signal } = event else {
                continue;
            };
            if !seen.insert(signal.id.clone()) {
                continue;
            }
            self.follow_signal_thread(report, &signal.id, true, None)
                .await;
        }
    }

    // A picture in a signal becomes a picture in Linear's storage, which is the only
    // form an issue comment can show. The bytes were vetted when they entered
    // altitude, so nothing is checked again here.
    pub(in crate::tracker::providers::linear::host) async fn hosted_body(
        &self,
        body: &str,
    ) -> String {
        let (text, pictures) = picture::take(body);
        if pictures.is_empty() {
            return body.to_owned();
        }
        let mut hosted = text;
        for stored in pictures {
            let alt = stored.alt;
            let bytes = picture::read(&self.config.attachments_dir, &stored.id);
            let Ok(bytes) = bytes else {
                hosted.push_str(&format!("\n\n_(the picture {alt} is not on this machine)_"));
                continue;
            };
            let mime = picture::mime(&bytes).unwrap_or("image/png");
            let name = format!("{}.{}", stored.id, mime.rsplit('/').next().unwrap_or("png"));
            match self
                .config
                .automation
                .upload_picture(&bytes, mime, &name)
                .await
            {
                // Linear keeps an upload to itself: the URL is readable by whoever can
                // read the issue, and by nobody else. It matters less than it sounds,
                // because a synced issue mirror carries no comments at all — the
                // picture lives where the comment lives, and nowhere else.
                Ok(url) => hosted.push_str(&format!("\n\n![{alt}]({url})")),
                Err(error) => {
                    tracing::warn!(%error, "could not give the tracker a picture");
                    hosted.push_str(&format!("\n\n_(the picture {alt} could not be sent)_"));
                }
            }
        }
        hosted
    }

    // What the tracker says a task is waiting on is the only record of whether the
    // question reached the operator. A pass that failed to publish an ask leaves a
    // signal nobody has seen and a thread that does not exist, which then reads as an
    // error for as long as the task lives; asking the tracker where each open question
    // stands is cheaper than keeping a second copy of the answer here. The marker is
    // the comment's own id, so publishing one twice says the same thing once.
    pub(in crate::tracker::providers::linear::host) async fn ensure_asks_said(
        &self,
        host: &dyn DelegatedWork,
        report: &Cursor,
        threads: &[TaskAsk],
        events: &[(i64, TaskHistoryEvent)],
    ) -> Result<(), String> {
        let contributions = contribution_details(events);
        for signal in threads.iter().filter(|signal| signal.is_unresolved()) {
            let marker = ask_marker(&report.task_id, &signal.id);
            match self.config.automation.has_ask(&marker).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => {
                    tracing::debug!(task = %report.task_id, signal = %signal.id, %error, "could not ask the tracker about an open question");
                    continue;
                }
            }
            let Some(message) = signal.latest_agent_message() else {
                continue;
            };
            tracing::info!(
                task = %report.task_id,
                signal = %signal.id,
                "a question never reached the issue, so it is asked again"
            );
            if let Some(status) = self
                .publish_signal(
                    report,
                    &signal.id,
                    signal.kind,
                    &signal.title,
                    &message.body,
                    &contributions,
                )
                .await?
            {
                self.write_status(host, report, status).await?;
            }
        }
        Ok(())
    }
}
