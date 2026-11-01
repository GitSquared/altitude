use std::{collections::HashMap, sync::Arc, time::SystemTime};

use tokio::sync::Mutex;

use crate::{
    agent::AgentKind,
    config::{Config, InstallationIdentity},
    credentials::{Service, Slot},
    isolation::IsolationEngine,
    project::Project,
    tracker::{IssueSnapshot, Linear, TrackerError, TrackerProvider, TrackerStatus},
};

use super::{Task, TaskExecutionPauseReason, TaskHistoryEvent, TaskService, TaskServiceError};

pub trait TrackerHost: Send + Sync + 'static {
    fn for_credential(&self, operator: &str, credential: &str) -> Option<Arc<dyn TrackerProvider>>;

    fn connectable(&self, operator: &str, credential: &str) -> Option<Arc<dyn TrackerProvider>>;
}

pub(crate) struct OwnTrackers {
    config: Config,
    opened: Mutex<HashMap<String, Opened>>,
}

struct Opened {
    tracker: Arc<Linear>,
    signed_in_at: Option<SystemTime>,
}

impl OwnTrackers {
    pub(crate) fn new(config: Config) -> Self {
        Self {
            config,
            opened: Mutex::new(HashMap::new()),
        }
    }
}

impl TrackerHost for OwnTrackers {
    fn connectable(&self, _operator: &str, credential: &str) -> Option<Arc<dyn TrackerProvider>> {
        if Service::from_id(credential)?.slot() != Slot::Tracker {
            return None;
        }
        let tracker = Linear::from_directory(&self.config.profile_dir(credential)).ok()?;
        Some(Arc::new(tracker) as Arc<dyn TrackerProvider>)
    }

    fn for_credential(
        &self,
        _operator: &str,
        credential: &str,
    ) -> Option<Arc<dyn TrackerProvider>> {
        if Service::from_id(credential)?.slot() != Slot::Tracker {
            return None;
        }
        if let Ok(opened) = self.opened.try_lock()
            && let Some(opened) = opened.get(credential)
            && opened.signed_in_at == opened.tracker.sign_in_changed_at()
        {
            return Some(opened.tracker.clone() as Arc<dyn TrackerProvider>);
        }
        let directory = self.config.profile_dir(credential);
        let tracker = Linear::from_directory(&directory).ok()?;
        if !tracker.is_signed_in() {
            return None;
        }
        let signed_in_at = tracker.sign_in_changed_at();
        let tracker = Arc::new(tracker);
        if let Ok(mut opened) = self.opened.try_lock() {
            opened.insert(
                credential.to_owned(),
                Opened {
                    tracker: Arc::clone(&tracker),
                    signed_in_at,
                },
            );
        }
        Some(tracker as Arc<dyn TrackerProvider>)
    }
}

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone,
{
    pub async fn create_from_issue(
        &self,
        issue: IssueSnapshot,
        project: Project,
        agent: AgentKind,
    ) -> Result<Task, TaskServiceError> {
        self.create_from_issue_as_operator(issue, project, agent, None)
            .await
    }

    pub async fn create_from_issue_as_operator(
        &self,
        issue: IssueSnapshot,
        project: Project,
        agent: AgentKind,
        requester: Option<InstallationIdentity>,
    ) -> Result<Task, TaskServiceError> {
        self.create_from_issue_bound(issue, project, agent, requester, None)
            .await
    }

    pub async fn create_from_issue_bound(
        &self,
        issue: IssueSnapshot,
        project: Project,
        agent: AgentKind,
        requester: Option<InstallationIdentity>,
        delegation: Option<String>,
    ) -> Result<Task, TaskServiceError> {
        // A requester is what makes this a delegation: the tracker says a person
        // asked for the work, and that person is who the agent is answering. A task
        // started from the operator's own machine has no such person to name.
        let delegated_by = requester.as_ref().map(|identity| identity.label.clone());
        let operator = requester.unwrap_or_else(|| self.identity.clone());
        let intent = issue.intent(delegated_by.as_deref());
        let title = issue.issue.title.trim().to_owned();
        // The issue is what the work is called. A list that names a linked task by
        // its id is a list of file names, and the operator asked for the issue.
        let was_delegated = delegation.is_some();
        let mut arrival = vec![TaskHistoryEvent::TrackerIssueBound { issue, delegation }];
        if !title.is_empty() {
            arrival.push(TaskHistoryEvent::TaskTitleSet { title });
        }
        let task = self
            .adopt(
                self.draft(intent, project, agent, None, Some(operator.clone())),
                None,
                &arrival,
            )
            .await?;
        if task.operator_id() != Some(operator.id.as_str()) {
            return Err(TaskServiceError::Task(super::TaskError::OperationRejected(
                "this issue is already being worked on for another operator",
            )));
        }
        // Nothing was delegated to this machine, so nothing will be reported: the
        // issue is a reference the operator keeps. Putting it on their name is the
        // one thing the tracker still needs to know.
        if !was_delegated {
            self.assign_linked_issue(&task).await;
        }
        Ok(task)
    }

    /// A failed assignment says so in the log and nowhere else: a task the operator
    /// asked for is not worth failing over the tracker's opinion of who owns it.
    async fn assign_linked_issue(&self, task: &Task) {
        let Some(host) = self.trackers.clone() else {
            return;
        };
        let Some(reference) = task.facts.tracker_issue.as_ref() else {
            return;
        };
        let credential = task
            .project()
            .and_then(|project| project.tracker.as_ref())
            .and_then(|binding| binding.credential.as_deref())
            .unwrap_or("trackers/linear");
        let Some(provider) = task
            .operator_id()
            .and_then(|owner| host.for_credential(owner, credential))
        else {
            return;
        };
        if let Err(error) = provider.assign_to_self(reference).await {
            tracing::warn!(task = %task.id, %error, "could not put this issue on the operator's name");
        }
    }

    pub async fn sync_tracker(&self) -> Result<(), TrackerError> {
        let Some(host) = self.trackers.clone() else {
            return Ok(());
        };
        let tasks = self
            .list()
            .await
            .map_err(|error| TrackerError::new(error.to_string()))?;
        let mut first_error = None;
        for task in tasks {
            // Only a delegation makes the tracker a place this task is talked about.
            // A task a client started, or linked to an issue, lives in the client: the
            // issue is a reference it keeps, not somewhere its work is reported.
            if task.facts.tracker_issue.is_none() || task.facts.tracker_delegation.is_none() {
                continue;
            }
            // A task is reported through the account of the peer that owns it,
            // which is the login that peer pushed here.
            let credential = task
                .project()
                .and_then(|project| project.tracker.as_ref())
                .and_then(|route| route.credential.as_deref())
                .unwrap_or("trackers/linear");
            let Some(provider) = task
                .operator_id()
                .and_then(|owner| host.for_credential(owner, credential))
            else {
                continue;
            };
            if let Err(error) = self.sync_task_tracker(provider.as_ref(), &task).await {
                tracing::warn!(task_id = %task.id, %error, "could not sync a task to its tracker");
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn sync_task_tracker(
        &self,
        provider: &dyn TrackerProvider,
        task: &Task,
    ) -> Result<(), TrackerError> {
        let reference = task
            .facts
            .tracker_issue
            .as_ref()
            .expect("callers only sync bound tasks");
        let events = self
            .store
            .events(&task.id)
            .await
            .map_err(|error| TrackerError::new(error.to_string()))?;
        let start = checkpoint(&events);
        if start >= events.len() {
            return Ok(());
        }
        let target = latest_status(&events);
        let reported = latest_status(&events[..start]);
        let status_changed = target.is_some() && target != reported;
        if !status_changed {
            return Ok(());
        }
        // A task removed while this pass was reading is no longer work, so it is
        // not commented on or moved; its record stays.
        let active = self
            .store
            .is_kept(&task.id)
            .await
            .map_err(|error| TrackerError::new(error.to_string()))?;
        if !active {
            return Ok(());
        }
        if let Some(status) = target.filter(|_| status_changed)
            && let Err(error) = provider.status(reference, status).await
        {
            // A tracker that will not take a status will not take it on the next
            // pass either. After a few tries the checkpoint moves past the change
            // so the queue ends, and the operator is told once.
            let refusals = self.record_refusal(&task.id);
            if refusals < STATUS_SYNC_ATTEMPTS {
                return Err(error);
            }
            tracing::warn!(
                task = %task.id,
                %error,
                "stopped trying to set this task's issue status"
            );
            self.store
                .append_events(
                    &task.id,
                    &[TaskHistoryEvent::TrackerSynced {
                        event_count: events.len(),
                        status: reported,
                    }],
                )
                .await
                .map_err(|error| TrackerError::new(error.to_string()))?;
            return Ok(());
        }
        self.forget_refusals(&task.id);
        self.store
            .append_events(
                &task.id,
                &[TaskHistoryEvent::TrackerSynced {
                    event_count: events.len(),
                    status: target,
                }],
            )
            .await
            .map_err(|error| TrackerError::new(error.to_string()))?;
        Ok(())
    }
}

impl<E> TaskService<E> {
    fn record_refusal(&self, task_id: &str) -> usize {
        let Ok(mut refusals) = self.refused_status_syncs.lock() else {
            return STATUS_SYNC_ATTEMPTS;
        };
        let count = refusals.entry(task_id.to_owned()).or_insert(0);
        *count += 1;
        *count
    }

    fn forget_refusals(&self, task_id: &str) {
        if let Ok(mut refusals) = self.refused_status_syncs.lock() {
            refusals.remove(task_id);
        }
    }
}

/// How many passes a refused status change is retried on. The first few can be a
/// tracker having a bad minute; after that it is the tracker saying no.
const STATUS_SYNC_ATTEMPTS: usize = 3;

fn checkpoint(events: &[(i64, TaskHistoryEvent)]) -> usize {
    events
        .iter()
        .rev()
        .find_map(|(_, event)| match event {
            TaskHistoryEvent::TrackerSynced { event_count, .. } => Some(*event_count),
            _ => None,
        })
        .unwrap_or(0)
}

fn latest_status(events: &[(i64, TaskHistoryEvent)]) -> Option<TrackerStatus> {
    let mut status = None;
    for (_, event) in events {
        match event {
            TaskHistoryEvent::AgentRunStarted { .. } => {
                if status != Some(TrackerStatus::Done) {
                    status = Some(TrackerStatus::InProgress);
                }
            }
            TaskHistoryEvent::ExecutionPaused {
                execution_pause_reason,
                ..
            } if *execution_pause_reason == TaskExecutionPauseReason::AwaitingOperator => {
                if status != Some(TrackerStatus::Done) {
                    status = Some(TrackerStatus::InReview);
                }
            }
            TaskHistoryEvent::OperatorSignalConfirmed { .. }
            | TaskHistoryEvent::TaskCompleted { .. } => status = Some(TrackerStatus::Done),
            _ => {}
        }
    }
    status
}
