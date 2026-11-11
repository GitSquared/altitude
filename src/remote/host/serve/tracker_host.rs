use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use crate::agent::Activity;
use crate::remote::{StartFuture, StartRefusal};
use crate::task::{QueueStanding, TaskExecutionPauseReason, TaskExecutionStatus};
use iroh::EndpointId;

use crate::{
    config::{Config, InstallationIdentity},
    credentials::Service,
    task::{TaskAsk, TaskHistoryEvent, TrackerHost},
    tracker::{
        self, BoundTask, DelegatedWork, DelegatedWorkFuture, IssueSnapshot, TrackerProvider,
        TrackerStatus,
    },
};

use super::OwnTasks;

pub(super) struct PushedTrackers {
    config: Config,
    opened: Mutex<HashMap<(String, String), Opened>>,
}

struct Opened {
    tracker: Arc<dyn TrackerProvider>,
    signed_in_at: Option<SystemTime>,
}

impl DelegatedWork for OwnTasks {
    fn start_issue(
        &self,
        peer: EndpointId,
        issue: IssueSnapshot,
        label: String,
        delegation: Option<String>,
    ) -> StartFuture<'_, String> {
        Box::pin(async move {
            if self.delegation_paused {
                return Err(StartRefusal::Paused);
            }
            let owner = peer.to_string();
            let scoped =
                self.config
                    .for_operator(&owner)
                    .map_err(|error| StartRefusal::Unavailable {
                        reason: error.to_string(),
                    })?;
            if !scoped.settings_path().is_file() {
                return Err(StartRefusal::NoSetup);
            }
            let settings =
                scoped
                    .load_or_create_settings()
                    .map_err(|error| StartRefusal::Unavailable {
                        reason: error.to_string(),
                    })?;
            let Some(scope) = issue.issue.scope.as_ref() else {
                return Err(StartRefusal::NoScope);
            };
            // An exact claim beats a project that claims its whole account, so a
            // workspace that grows a second scope does not start sending both to
            // whichever project was there first.
            let projects = settings.settings().projects();
            let project = projects
                .iter()
                .find(|project| {
                    project
                        .tracker
                        .as_ref()
                        .is_some_and(|route| route.claims(&scope.id, &scope.aliases))
                })
                .or_else(|| {
                    projects.iter().find(|project| {
                        project
                            .tracker
                            .as_ref()
                            .is_some_and(|route| route.claims_anything())
                    })
                })
                .cloned();
            let Some(project) = project else {
                return Err(StartRefusal::NoProject {
                    scope: scope.label.clone(),
                });
            };
            if !project
                .tracker
                .as_ref()
                .is_some_and(|route| route.accepts_delegated_work)
            {
                return Err(StartRefusal::NotAccepting {
                    project: project.name.clone(),
                    scope: scope.label.clone(),
                });
            }
            let agent = project
                .agent
                .or_else(|| settings.settings().agent())
                .ok_or(StartRefusal::NoAgent)?;
            let task = self
                .tasks
                .create_issue_for_operator(
                    issue,
                    project,
                    agent,
                    InstallationIdentity {
                        id: owner.clone(),
                        label,
                    },
                    delegation,
                )
                .await
                .map_err(|error| StartRefusal::Unavailable {
                    reason: error.to_string(),
                })?;
            Ok(task.id)
        })
    }

    fn complete(&self, task_id: String) -> DelegatedWorkFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .complete_task(&task_id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn stop(
        &self,
        task_id: String,
        reason: TaskExecutionPauseReason,
    ) -> DelegatedWorkFuture<'_, ()> {
        Box::pin(async move {
            if let Some(task) = self
                .tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|task| task.id == task_id)
            {
                self.tasks
                    .stop(task, reason)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        })
    }

    fn record_status(&self, task_id: String, status: TrackerStatus) -> DelegatedWorkFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .record_status(&task_id, status)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn signal_threads(&self, task_id: String) -> DelegatedWorkFuture<'_, Vec<TaskAsk>> {
        Box::pin(async move {
            self.tasks
                .service()
                .signal_threads(&task_id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn set_signal_resolved(
        &self,
        task_id: String,
        signal_id: String,
        resolved: bool,
    ) -> DelegatedWorkFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .close_signal(&task_id, &signal_id, resolved)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn events(&self, task_id: String) -> DelegatedWorkFuture<'_, Vec<(i64, TaskHistoryEvent)>> {
        Box::pin(async move {
            self.tasks
                .events(&task_id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    // The same thing the interface does with a message typed into a task: it is
    // recorded as an operator input and the agent reads it on its next turn. What it
    // must not do is stop the run to rewrite the intent, which throws away the
    // sandbox it was working in.
    fn steer(&self, task_id: String, message: String) -> DelegatedWorkFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .express_input(&task_id, message)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    fn reply(
        &self,
        task_id: String,
        signal_id: String,
        answer: String,
    ) -> DelegatedWorkFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .reply(&task_id, &signal_id, answer)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn accept(&self, task_id: String, signal_id: String) -> DelegatedWorkFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .accept(&task_id, &signal_id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn react(&self, task_id: String, note: String) -> DelegatedWorkFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .react(&task_id, note)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn delegation_of(&self, task_id: String) -> DelegatedWorkFuture<'_, Option<String>> {
        Box::pin(async move {
            Ok(self
                .tasks
                .task(&task_id)
                .await
                .map_err(|error| error.to_string())?
                .and_then(|task| task.facts.tracker_delegation.clone()))
        })
    }

    fn work_for_issue(
        &self,
        issue_id: String,
        delegations: Vec<String>,
    ) -> DelegatedWorkFuture<'_, Option<BoundTask>> {
        Box::pin(async move {
            let wanted: Vec<&str> = delegations.iter().map(String::as_str).collect();
            let task = self
                .tasks
                .work_for_issue(&issue_id, &wanted)
                .await
                .map_err(|error| error.to_string())?;
            Ok(task.and_then(|task| {
                let issue = task.facts.tracker_issue.clone()?;
                Some(BoundTask {
                    task_id: task.id.clone(),
                    owner: task.operator_id()?.to_owned(),
                    issue,
                    delegation: task.facts.tracker_delegation.clone(),
                    session_agent: task.session_agent(),
                    status: task.facts.execution_status,
                    pause_reason: task.facts.execution_pause_reason.clone(),
                    ask: task.facts.leading_ask.as_ref().map(|ask| ask.kind),
                })
            }))
        })
    }

    fn session_agent(&self, task_id: String) -> DelegatedWorkFuture<'_, Option<String>> {
        Box::pin(async move {
            Ok(self
                .tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|task| task.id == task_id)
                .and_then(|task| task.session_agent()))
        })
    }

    fn current_activity(&self, task_id: String) -> DelegatedWorkFuture<'_, Option<Activity>> {
        Box::pin(async move { Ok(self.tasks.current_activity(&task_id)) })
    }

    fn queue_standings(&self) -> DelegatedWorkFuture<'_, HashMap<String, QueueStanding>> {
        Box::pin(async move {
            self.tasks
                .queue_standings()
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn bound_tasks(&self) -> DelegatedWorkFuture<'_, Vec<BoundTask>> {
        Box::pin(async move {
            let tasks = self.tasks.list().await.map_err(|error| error.to_string())?;
            Ok(tasks
                .into_iter()
                .filter_map(|task| {
                    // Only a task still in play has anything to report. One that
                    // ended — accepted, completed or failed — has told its story,
                    // and adopting it again opens a second session and replays an
                    // acceptance the operator has already moved past.
                    if !matches!(
                        task.facts.execution_status,
                        TaskExecutionStatus::Pending
                            | TaskExecutionStatus::Running
                            | TaskExecutionStatus::Paused
                    ) {
                        return None;
                    }
                    // Paused is not the same as in play: a task the tracker ended,
                    // replaced or undelegated is a record, and adopting it again
                    // hands a new delegation's session to work that is over.
                    if task
                        .facts
                        .execution_pause_reason
                        .as_ref()
                        .is_some_and(TaskExecutionPauseReason::is_out_of_play)
                    {
                        return None;
                    }
                    let issue = task.facts.tracker_issue.clone()?;
                    let owner = task.operator_id()?.to_owned();
                    Some(BoundTask {
                        task_id: task.id.clone(),
                        owner,
                        issue,
                        delegation: task.facts.tracker_delegation.clone(),
                        session_agent: task.session_agent(),
                        status: task.facts.execution_status,
                        pause_reason: task.facts.execution_pause_reason.clone(),
                        ask: task.facts.leading_ask.as_ref().map(|ask| ask.kind),
                    })
                })
                .collect())
        })
    }

    fn is_kept(&self, task_id: String) -> DelegatedWorkFuture<'_, bool> {
        Box::pin(async move {
            Ok(self
                .tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .iter()
                .any(|task| task.id == task_id))
        })
    }
}

impl PushedTrackers {
    pub(super) fn new(config: Config) -> Self {
        Self {
            config,
            opened: Mutex::new(HashMap::new()),
        }
    }
}

impl TrackerHost for PushedTrackers {
    fn connectable(&self, operator: &str, credential: &str) -> Option<Arc<dyn TrackerProvider>> {
        let Service::Tracker(kind) = Service::from_id(credential)? else {
            return None;
        };
        let directory = self
            .config
            .for_operator(operator)
            .ok()?
            .profile_dir(credential);
        tracker::provider(kind, &directory)
    }

    fn for_credential(&self, operator: &str, credential: &str) -> Option<Arc<dyn TrackerProvider>> {
        let Service::Tracker(kind) = Service::from_id(credential)? else {
            return None;
        };
        let key = (operator.to_owned(), credential.to_owned());
        if let Some(opened) = self.opened.lock().ok()?.get(&key)
            && opened.signed_in_at == opened.tracker.sign_in_changed_at()
        {
            return Some(Arc::clone(&opened.tracker));
        }
        let directory = self
            .config
            .for_operator(operator)
            .ok()?
            .profile_dir(credential);
        let tracker = tracker::provider(kind, &directory)?;
        // A peer that pushed a tracker profile without signing in has nothing to
        // report through, and is not an error worth warning about every pass.
        if !tracker.is_signed_in() {
            return None;
        }
        let signed_in_at = tracker.sign_in_changed_at();
        self.opened.lock().ok()?.insert(
            key,
            Opened {
                tracker: Arc::clone(&tracker),
                signed_in_at,
            },
        );
        Some(tracker)
    }
}
