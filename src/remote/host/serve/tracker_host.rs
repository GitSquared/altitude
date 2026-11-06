use crate::remote::{StartFuture, StartRefusal};

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use crate::task::tasks_waiting_for_room;
use crate::task::{TaskExecutionPauseReason, TaskExecutionStatus};
use iroh::EndpointId;
use sha2::{Digest, Sha256};

use tokio::{net::TcpListener, time::interval};

use crate::{
    config::{Config, InstallationIdentity, LinearSettings},
    task::{TaskHistoryEvent, TaskOperatorSignalThread, TrackerHost},
    tracker::{
        IssueSnapshot, Linear, TrackerProvider, TrackerStatus,
        providers::linear::{automation::LinearAutomation, oauth},
    },
};

use super::OwnTasks;
use crate::remote::host::tracker;

pub(super) struct PushedTrackers {
    config: Config,
    opened: Mutex<HashMap<(String, String), Opened>>,
}

struct Opened {
    tracker: Arc<Linear>,
    signed_in_at: Option<SystemTime>,
}

pub(super) struct LinearRuntime {
    pub(super) automation: Arc<LinearAutomation>,
    pub(super) tracker: Arc<tracker::RemoteTracker>,
    pub(super) listener: TcpListener,
    pub(super) workspace_name: String,
}

// Six bytes of the digest: enough to tell two secrets apart, far too little to be
// a secret itself.
fn fingerprint(secret: &str) -> String {
    hex::encode(&Sha256::digest(secret.as_bytes())[..6])
}

pub(super) async fn connect_tracker(
    config: &Config,
    settings: &LinearSettings,
    credentials_path: &Path,
) -> Result<LinearRuntime, String> {
    let mut credentials = oauth::AppCredentials::load(credentials_path)?;
    credentials
        .refresh_if_needed(settings, credentials_path)
        .await?;
    let workspace_name = credentials.workspace_name.clone();
    let automation = Arc::new(
        credentials
            .client()
            .await
            .map_err(|error| error.to_string())?,
    );
    let listener = TcpListener::bind(&settings.listen)
        .await
        .map_err(|error| format!("could not listen for Linear webhooks: {error}"))?;
    let webhook_secret = settings.webhook_secret()?;
    // A fingerprint rather than the secret: when Linear rejects a delivery the
    // question an operator has is which value this machine is holding, and twelve
    // hex characters answer it against `printf '%s' '<the app secret>' | sha256sum`
    // without a secret reaching a log.
    tracing::info!(
        fingerprint = %fingerprint(&webhook_secret),
        "Linear webhook secret loaded"
    );
    let tracker = tracker::RemoteTracker::new(tracker::RemoteTrackerConfig {
        automation: automation.clone(),
        webhook_secret: webhook_secret.into_bytes(),
        oauth_client_id: settings.client_id.clone(),
        storage_path: config.data_dir().join("trackers/linear/deliveries.json"),
        attachments_dir: config.attachments_dir(),
        operators_dir: config.data_dir().join("operators"),
        host_label: whoami::hostname().unwrap_or_else(|_| "remote".to_owned()),
    })?;
    let settings = settings.clone();
    let path = credentials_path.to_owned();
    let refreshed_automation = automation.clone();
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            match credentials.refresh_if_needed(&settings, &path).await {
                Ok(true) => {
                    if let Err(error) =
                        refreshed_automation.replace_token(credentials.access_token().to_owned())
                    {
                        tracing::warn!(%error, "Linear login needs attention");
                    }
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, "Linear login refresh remains queued"),
            }
        }
    });
    Ok(LinearRuntime {
        automation,
        tracker,
        listener,
        workspace_name,
    })
}

impl tracker::Host for OwnTasks {
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

    fn complete(&self, task_id: String) -> tracker::HostFuture<'_, ()> {
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
    ) -> tracker::HostFuture<'_, ()> {
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

    fn record_status(&self, task_id: String, status: TrackerStatus) -> tracker::HostFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .record_status(&task_id, status)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn signal_threads(
        &self,
        task_id: String,
    ) -> tracker::HostFuture<'_, Vec<TaskOperatorSignalThread>> {
        Box::pin(async move {
            self.tasks
                .service()
                .signal_threads(&task_id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn reopen_signal(&self, task_id: String, signal_id: String) -> tracker::HostFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .service()
                .resolve_signal(&task_id, &signal_id, false)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    fn events(&self, task_id: String) -> tracker::HostFuture<'_, Vec<(i64, TaskHistoryEvent)>> {
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
    fn steer(&self, task_id: String, message: String) -> tracker::HostFuture<'_, ()> {
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
    ) -> tracker::HostFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .reply(&task_id, &signal_id, answer)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn accept(&self, task_id: String, signal_id: String) -> tracker::HostFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .service()
                .confirm_operator_signal(&task_id, &signal_id)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    fn delegation_of(&self, task_id: String) -> tracker::HostFuture<'_, Option<String>> {
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
    ) -> tracker::HostFuture<'_, Option<tracker::BoundTask>> {
        Box::pin(async move {
            let wanted: Vec<&str> = delegations.iter().map(String::as_str).collect();
            let task = self
                .tasks
                .work_for_issue(&issue_id, &wanted)
                .await
                .map_err(|error| error.to_string())?;
            Ok(task.and_then(|task| {
                let issue = task.facts.tracker_issue.clone()?;
                Some(tracker::BoundTask {
                    task_id: task.id.clone(),
                    owner: task.operator_id()?.to_owned(),
                    issue,
                    delegation: task.facts.tracker_delegation.clone(),
                    session_agent: task.session_agent(),
                })
            }))
        })
    }

    fn session_agent(&self, task_id: String) -> tracker::HostFuture<'_, Option<String>> {
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

    fn current_activity(&self, task_id: String) -> tracker::HostFuture<'_, Option<String>> {
        Box::pin(async move { Ok(self.tasks.current_activity(&task_id).map(|line| line.text)) })
    }

    // One read of the queue for every task on it, rather than one read per task: the
    // queue, the machine and the start are the same three facts for all of them.
    fn queue_standings(&self) -> tracker::HostFuture<'_, HashMap<String, tracker::QueueStanding>> {
        Box::pin(async move {
            let tasks = self.tasks.list().await.map_err(|error| error.to_string())?;
            // A task waiting for room has no capsule, so every capsule counted here
            // belongs to work that is ahead of it.
            let on_machine = tasks
                .iter()
                .filter(|task| task.facts.sandbox_available)
                .count();
            let holding = self.tasks.pressure();
            Ok(tasks_waiting_for_room(&tasks)
                .into_iter()
                .enumerate()
                .map(|(ahead, task)| {
                    let start = self.tasks.is_starting(&task.id).then(|| {
                        match self.tasks.service().environment_step(&task.id) {
                            Some(progress) => tracker::Start::Building(progress),
                            None => tracker::Start::Provisioning,
                        }
                    });
                    (
                        task.id.clone(),
                        tracker::QueueStanding {
                            ahead,
                            on_machine,
                            holding,
                            start,
                        },
                    )
                })
                .collect())
        })
    }

    fn bound_tasks(&self) -> tracker::HostFuture<'_, Vec<tracker::BoundTask>> {
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
                    Some(tracker::BoundTask {
                        task_id: task.id.clone(),
                        owner,
                        issue,
                        delegation: task.facts.tracker_delegation.clone(),
                        session_agent: task.session_agent(),
                    })
                })
                .collect())
        })
    }

    fn is_kept(&self, task_id: String) -> tracker::HostFuture<'_, bool> {
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
        let directory = self
            .config
            .for_operator(operator)
            .ok()?
            .profile_dir(credential);
        let tracker = Linear::from_directory(&directory).ok()?;
        Some(Arc::new(tracker) as Arc<dyn TrackerProvider>)
    }

    fn for_credential(&self, operator: &str, credential: &str) -> Option<Arc<dyn TrackerProvider>> {
        let key = (operator.to_owned(), credential.to_owned());
        if let Some(opened) = self.opened.lock().ok()?.get(&key)
            && opened.signed_in_at == opened.tracker.sign_in_changed_at()
        {
            return Some(opened.tracker.clone() as Arc<dyn TrackerProvider>);
        }
        let directory = self
            .config
            .for_operator(operator)
            .ok()?
            .profile_dir(credential);
        let tracker = Linear::from_directory(&directory).ok()?;
        // A peer that pushed a tracker profile without signing in has nothing to
        // report through, and is not an error worth warning about every pass.
        if !tracker.is_signed_in() {
            return None;
        }
        let signed_in_at = tracker.sign_in_changed_at();
        let tracker = Arc::new(tracker);
        self.opened.lock().ok()?.insert(
            key,
            Opened {
                tracker: Arc::clone(&tracker),
                signed_in_at,
            },
        );
        Some(tracker as Arc<dyn TrackerProvider>)
    }
}
