use std::time::{Duration, Instant};

use reqwest::Method;
use serde_json::{Value, json};

use crate::task::{TaskPlanStatus, TaskPlanStep};
use crate::tracker::{
    TrackerError,
    host::automation::{AgentActivityContent, SessionStanding},
};

use super::super::issues::IssueAddress;
use super::GitHubAutomation;

// GitHub has no agent sessions, so a session is one comment this app keeps current:
// what the work is, the line it is on, the plan, and what the agent has said. Edited
// in place, it is one notification for the whole run instead of one per sentence.
// Questions, reviews and failures are addressed to a person and get comments of
// their own.
pub(super) const SESSION_MARKER: &str = "<!-- altitude:session ";
const MARKER_END: &str = " -->";
const ENTRY_MARKER: &str = "<!-- altitude:entry ";
const KEPT_ENTRIES: usize = 15;
const ENTRY_RULE: &str = "---";

// A line that is replaced by the next one is not worth an edit every second: GitHub
// counts every request against the app, and the next line says more anyway.
const EPHEMERAL_INTERVAL: Duration = Duration::from_secs(5);

// A session is named by its issue and the delegation that opened it, so the issue is
// read off the name rather than kept beside it.
pub(super) fn session_issue(session_id: &str) -> Result<IssueAddress, TrackerError> {
    session_id
        .rsplit_once('@')
        .and_then(|(issue, _)| IssueAddress::parse(issue))
        .ok_or_else(|| TrackerError::new("this is not a GitHub session"))
}

pub(crate) fn session_id(issue: &IssueAddress, delegation: &str) -> String {
    format!("{}@{delegation}", issue.id())
}

#[derive(Default, PartialEq)]
struct StatusComment {
    summary: String,
    line: String,
    plan: String,
    entries: Vec<(String, String)>,
}

impl StatusComment {
    fn parse(body: &str) -> Self {
        let entries = section(body, "log")
            .unwrap_or_default()
            .split(ENTRY_MARKER)
            .filter_map(|entry| {
                let (id, text) = entry.split_once(MARKER_END)?;
                // The rule between two entries is the rendering's, not the entry's:
                // read back as text, it would gain another one on every edit.
                let mut text = text.trim();
                while let Some(before) = text.strip_suffix(ENTRY_RULE) {
                    text = before.trim_end();
                }
                Some((id.trim().to_owned(), text.to_owned()))
            })
            .collect();
        Self {
            summary: section(body, "summary").unwrap_or_default(),
            line: section(body, "line").unwrap_or_default(),
            plan: section(body, "plan").unwrap_or_default(),
            entries,
        }
    }

    fn render(&self, session_id: &str) -> String {
        let mut body = format!(
            "{}\n",
            fenced("summary", &format!("**⛰ altitude** · {}", self.summary))
        );
        if !self.line.is_empty() {
            body.push_str(&format!("\n{}\n", fenced("line", &quoted(&self.line))));
        }
        if !self.plan.is_empty() {
            body.push_str(&format!("\n{}\n", fenced("plan", &self.plan)));
        }
        if !self.entries.is_empty() {
            let log = self
                .entries
                .iter()
                .map(|(id, text)| format!("{ENTRY_MARKER}{id}{MARKER_END}\n{text}"))
                .collect::<Vec<_>>()
                .join(&format!("\n\n{ENTRY_RULE}\n\n"));
            body.push_str(&format!(
                "\n<details><summary>What the agent said</summary>\n\n{}\n\n</details>\n",
                fenced("log", &log)
            ));
        }
        body.push_str(&format!("\n{SESSION_MARKER}{session_id}{MARKER_END}"));
        body
    }

    fn remember(&mut self, id: &str, text: &str) {
        if self.entries.iter().any(|(held, _)| held == id) {
            return;
        }
        self.entries.push((id.to_owned(), text.trim().to_owned()));
        let excess = self.entries.len().saturating_sub(KEPT_ENTRIES);
        self.entries.drain(..excess);
    }
}

fn fenced(name: &str, text: &str) -> String {
    format!("<!-- altitude:{name} -->\n{text}\n<!-- /altitude:{name} -->")
}

fn section(body: &str, name: &str) -> Option<String> {
    let open = format!("<!-- altitude:{name} -->");
    let close = format!("<!-- /altitude:{name} -->");
    let start = body.find(&open)? + open.len();
    let end = start + body[start..].find(&close)?;
    let text = body[start..end].trim();
    Some(match name {
        "summary" => text.trim_start_matches("**⛰ altitude** · ").to_owned(),
        "line" => text
            .lines()
            .map(|line| {
                line.strip_prefix("> ")
                    .or(line.strip_prefix(">"))
                    .unwrap_or(line)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => text.to_owned(),
    })
}

fn quoted(text: &str) -> String {
    text.trim()
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn plan_lines(steps: &[TaskPlanStep]) -> String {
    steps
        .iter()
        .map(|step| match step.status {
            TaskPlanStatus::Completed => format!("- [x] {}", step.content),
            TaskPlanStatus::InProgress => format!("- [ ] **{}**", step.content),
            TaskPlanStatus::Pending => format!("- [ ] {}", step.content),
            TaskPlanStatus::Canceled => format!("- [x] ~~{}~~", step.content),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl GitHubAutomation {
    pub(crate) async fn create_session_on_issue(
        &self,
        issue_id: &str,
    ) -> Result<String, TrackerError> {
        let issue = IssueAddress::parse(issue_id)
            .ok_or_else(|| TrackerError::new("this is not a GitHub issue"))?;
        Ok(session_id(&issue, &uuid::Uuid::new_v4().to_string()))
    }

    // A session's comment is written the first time something is said in it, so one
    // nobody has spoken in yet is the "never answered" Linear reports for its own.
    pub(crate) async fn session_standing(
        &self,
        session_id: &str,
    ) -> Result<SessionStanding, TrackerError> {
        let issue = session_issue(session_id)?;
        let token = self.token(&issue).await?;
        let locked = self
            .api
            .get(&token, &issue.path())
            .await?
            .get("locked")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(SessionStanding {
            can_receive: !locked,
            never_answered: !self.has_status_comment(session_id)
                && self.status_comment(session_id).await?.is_none(),
        })
    }

    pub(crate) async fn update_session_summary(
        &self,
        session_id: &str,
        summary: &str,
    ) -> Result<(), TrackerError> {
        self.rewrite(session_id, |status| status.summary = summary.to_owned())
            .await
    }

    pub(crate) async fn update_session_plan(
        &self,
        session_id: &str,
        steps: &[TaskPlanStep],
    ) -> Result<(), TrackerError> {
        let plan = plan_lines(steps);
        self.rewrite(session_id, |status| status.plan = plan).await
    }

    pub(crate) async fn create_activity(
        &self,
        session_id: &str,
        activity_id: &str,
        content: AgentActivityContent,
    ) -> Result<(), TrackerError> {
        match content {
            AgentActivityContent::Thought { body } => {
                self.rewrite(session_id, |status| status.line = body).await
            }
            // What the agent said is the transcript, and it ends the line it was on.
            AgentActivityContent::Response { body } => {
                self.rewrite(session_id, |status| {
                    status.remember(activity_id, &body);
                    status.line.clear();
                })
                .await
            }
            // A failure is the one thing the run says that someone has to act on, so it
            // is a comment of its own, which GitHub tells them about, and not a second
            // copy of it in the transcript.
            AgentActivityContent::Error { body } => {
                let issue = session_issue(session_id)?;
                self.comment_on_issue(&issue.id(), &body, activity_id)
                    .await?;
                self.rewrite(session_id, |status| status.line.clear()).await
            }
        }
    }

    pub(crate) async fn create_ephemeral_thought(
        &self,
        session_id: &str,
        body: String,
    ) -> Result<(), TrackerError> {
        let is_recent = self
            .ephemeral_edits
            .lock()
            .map(|edits| {
                edits
                    .get(session_id)
                    .is_some_and(|at| at.elapsed() < EPHEMERAL_INTERVAL)
            })
            .unwrap_or(false);
        if is_recent {
            return Ok(());
        }
        self.rewrite(session_id, |status| status.line = body)
            .await?;
        if let Ok(mut edits) = self.ephemeral_edits.lock() {
            edits.insert(session_id.to_owned(), Instant::now());
        }
        Ok(())
    }

    fn has_status_comment(&self, session_id: &str) -> bool {
        self.status_comments
            .lock()
            .is_ok_and(|held| held.contains_key(session_id))
    }

    async fn status_comment(&self, session_id: &str) -> Result<Option<Value>, TrackerError> {
        let issue = session_issue(session_id)?;
        self.own_comment_marked(&issue, &format!("{SESSION_MARKER}{session_id}{MARKER_END}"))
            .await
    }

    // One writer at a time: each edit reads the comment, changes one part and writes it
    // back, and two of them interleaved would lose one change. The comment this app
    // last wrote is remembered, so an edit is one request; a restart finds it again by
    // its marker.
    async fn rewrite(
        &self,
        session_id: &str,
        change: impl FnOnce(&mut StatusComment),
    ) -> Result<(), TrackerError> {
        let _writing = self.writing.lock().await;
        let issue = session_issue(session_id)?;
        let remembered = self
            .status_comments
            .lock()
            .ok()
            .and_then(|held| held.get(session_id).cloned());
        let existing = match remembered {
            Some(held) => Some(held),
            None => self.status_comment(session_id).await?.and_then(|comment| {
                let number = comment.get("id").and_then(Value::as_u64)?;
                let body = comment.get("body").and_then(Value::as_str)?.to_owned();
                Some((number, body))
            }),
        };
        let mut status = existing
            .as_ref()
            .map(|(_, body)| StatusComment::parse(body))
            .unwrap_or_default();
        let before = existing.as_ref().map(|_| status.render(session_id));
        change(&mut status);
        let body = status.render(session_id);
        if before.as_deref() == Some(body.as_str()) {
            return Ok(());
        }
        let token = self.token(&issue).await?;
        let written = match existing {
            Some((number, _)) => {
                self.api
                    .send(
                        &token,
                        Method::PATCH,
                        &format!("/repos/{}/issues/comments/{number}", issue.repository),
                        Some(json!({ "body": body })),
                    )
                    .await?
            }
            None => {
                self.api
                    .send(
                        &token,
                        Method::POST,
                        &format!("{}/comments", issue.path()),
                        Some(json!({ "body": body })),
                    )
                    .await?
            }
        };
        if let (Some(number), Ok(mut held)) = (
            written.get("id").and_then(Value::as_u64),
            self.status_comments.lock(),
        ) {
            held.insert(session_id.to_owned(), (number, body));
        }
        Ok(())
    }
}
