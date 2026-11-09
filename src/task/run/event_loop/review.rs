use std::{collections::HashSet, sync::atomic::Ordering};

use tokio::time;

use crate::{
    isolation::IsolationEngine,
    task::mirror::review::{OBSERVATION_ATTEMPTS, REVIEW_POLL_INTERVAL, news, unreadable},
    task::{
        DeferredReview, Task, TaskAskAuthor, TaskAskQuestion, TaskExecutionPauseReason,
        TaskExecutionStatus, TaskHistoryEvent, TaskInput, TaskInputAuthor, TaskInputSource,
    },
    vcs::{ContributionObservation, ContributionOutlook, ContributionReadiness, now_ms},
};

use super::{TaskEventLoop, TaskExecutionUpdate, watcher_key};

impl<E> TaskEventLoop<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub(super) fn watch_held_reviews(&self) {
        let event_loop = self.clone();
        tokio::spawn(async move {
            let mut tick = time::interval(REVIEW_POLL_INTERVAL);
            tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if event_loop.watchers.is_stopped() {
                    return;
                }
                // The runtime may not be ready yet; a held review waits for it
                // rather than being observed through a sandbox that cannot be
                // made.
                if !event_loop.admission_enabled.load(Ordering::Acquire) {
                    continue;
                }
                event_loop.observe_held_reviews().await;
            }
        });
    }

    pub(super) async fn observe_held_reviews(&self) {
        let tasks = match self.service.store.list_held_reviews().await {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "could not read held reviews");
                return;
            }
        };
        // Only a task that has actually stopped for its review is watched: the hold is
        // recorded the moment the agent asks, and the turn that asked it may still be
        // running when the next look comes round.
        let watching: Vec<&Task> = tasks
            .iter()
            .filter(|task| {
                task.facts.execution_status == TaskExecutionStatus::Paused
                    && task.facts.execution_pause_reason
                        == Some(TaskExecutionPauseReason::AwaitingReviewChecks)
            })
            .collect();
        let owners: HashSet<String> = watching
            .iter()
            .filter_map(|task| watcher_key(task))
            .collect();
        self.watchers.retain(&owners).await;

        let watched: HashSet<String> = watching.iter().map(|task| task.id.clone()).collect();
        let due = self.review_waits.due(&watched).await;
        let waiting: Vec<&Task> = watching
            .into_iter()
            .filter(|task| due.contains(&task.id))
            .collect();
        // One request per owner covers every review that owner is waiting on, so
        // there is no per-review budget to keep: the pass is bounded by how many
        // owners have something due, and each of those is asked once.
        let mut owner_names: Vec<String> = waiting
            .iter()
            .filter_map(|task| watcher_key(task))
            .collect();
        owner_names.sort();
        owner_names.dedup();
        for owner in owner_names {
            if self.watchers.is_stopped() {
                return;
            }
            let theirs: Vec<&Task> = waiting
                .iter()
                .copied()
                .filter(|task| watcher_key(task).as_deref() == Some(owner.as_str()))
                .collect();
            self.observe_owner(&owner, &theirs).await;
        }
    }

    pub(super) async fn decide_held_review(
        &self,
        task: &Task,
        observations: &[ContributionObservation],
    ) {
        let Some(deferred) = task.facts.deferred_review.clone() else {
            return;
        };
        let mine: Vec<ContributionObservation> = observations
            .iter()
            .filter(|observation| {
                task.code_contributions
                    .iter()
                    .any(|contribution| contribution.id == observation.contribution_id)
            })
            .cloned()
            .collect();
        let Some((task, moved)) = self.record_observations(task, &mine).await else {
            return;
        };
        // The operator can take over or remove a task while a pass is in flight;
        // acting on a hold that is gone would raise a signal nobody owns.
        match self.service.store.is_kept(&task.id).await {
            Ok(true) => {}
            Ok(false) | Err(_) => return,
        }
        if task
            .facts
            .deferred_review
            .as_ref()
            .map(|hold| &hold.signal.id)
            != Some(&deferred.signal.id)
        {
            return;
        }
        self.review_waits.looked(&task.id, moved).await;
        self.resolve_held_review(&task, &deferred, &mine).await;
    }

    pub(super) async fn resolve_held_review(
        &self,
        task: &Task,
        deferred: &DeferredReview,
        observations: &[ContributionObservation],
    ) {
        let outlook = ContributionOutlook::of(observations, now_ms());
        match outlook.readiness {
            ContributionReadiness::NeedsAction => {
                let news = news(&outlook.needing_action);
                // A wake says something. If it would say what the machine already
                // said, and nothing on the pull requests has moved since, there is
                // nothing to say: the hold stands, the poll keeps looking, and the
                // next thing anybody hears is the change itself. Saying it again
                // would spend a turn to get the same answer.
                if task.facts.last_wake_news.as_deref() == Some(news.as_str()) {
                    tracing::debug!(
                        task.id = %task.id,
                        "a linked pull request still needs attention, and the machine has already said so"
                    );
                    return;
                }
                if self.wake_review(task, deferred, news).await {
                    // The task is queued; admission decides whether the host has
                    // room, so a review that needs work does not jump the queue.
                    self.admit();
                }
            }
            ContributionReadiness::Ready => {
                self.deliver_review(task, deferred.signal.clone()).await
            }
            // Checks are still running. The wait is already recorded, and the
            // review's own cadence decides when it is worth looking again.
            ContributionReadiness::Pending => {}
        }
    }

    pub(super) async fn abandon_review(
        &self,
        task: &Task,
        deferred: &DeferredReview,
        reason: &str,
    ) {
        tracing::warn!(
            task_id = %task.id,
            attempts = OBSERVATION_ATTEMPTS,
            %reason,
            "giving up on linked contributions"
        );
        self.deliver_review(task, unreadable(&deferred.signal, reason))
            .await;
    }

    pub(super) async fn deliver_review(&self, task: &Task, signal: TaskAskQuestion) {
        let events = [
            TaskHistoryEvent::OperatorSignalRaised { signal },
            TaskHistoryEvent::ExecutionPaused {
                execution_status: TaskExecutionStatus::Paused,
                sandbox_available: false,
                snapshot_available: task.facts.snapshot_available,
                execution_pause_reason: TaskExecutionPauseReason::AwaitingOperator,
                snapshot_path: task.facts.snapshot_path.clone().unwrap_or_default(),
            },
        ];
        let result = self.service.record(&task.id, &events).await;
        match &result {
            Ok(_) => tracing::info!(task_id = %task.id, "delivered a held review request"),
            Err(error) => {
                tracing::warn!(task_id = %task.id, %error, "could not deliver a held review request")
            }
        }
        let _ = self
            .events
            .send(TaskExecutionUpdate::Finished(task.id.clone(), result));
    }

    pub(super) async fn wake_review(
        &self,
        task: &Task,
        deferred: &DeferredReview,
        reason: String,
    ) -> bool {
        // A turn that is already running does not need waking, and stopping it would
        // lose the work in flight to say what it is about to be told anyway: the news
        // is left where the next turn reads it, the way an operator's message is.
        let is_running = task.facts.execution_status == TaskExecutionStatus::Running;
        let mut events = vec![
            TaskHistoryEvent::ReviewDeferralCleared {
                signal_id: deferred.signal.id.clone(),
                reason: reason.clone(),
            },
            // The review never reached the operator, and the agent is being given
            // the work it was asking about. Whatever thread the hold was continuing
            // ends here; the next request opens a new one.
            TaskHistoryEvent::OperatorSignalResolved {
                signal_id: deferred.signal.id.clone(),
                resolved: true,
                by: TaskAskAuthor::Agent,
            },
        ];
        if is_running {
            events.push(TaskHistoryEvent::TaskInputExpressed {
                input: TaskInput {
                    id: uuid::Uuid::new_v4().to_string(),
                    at_ms: now_ms(),
                    author: TaskInputAuthor::Operator {
                        id: task.facts.operator_id.clone().unwrap_or_default(),
                        label: task.facts.operator_label.clone().unwrap_or_default(),
                    },
                    source: TaskInputSource::Notice,
                    body: reason,
                    external_id: None,
                },
            });
        } else {
            events.push(TaskHistoryEvent::ExecutionQueued {
                execution_status: TaskExecutionStatus::Pending,
            });
        }
        match self.service.record(&task.id, &events).await {
            Ok(_) if is_running => {
                tracing::info!(
                    task_id = %task.id,
                    "a linked pull request needs attention while the agent is working; its next turn reads it"
                );
                false
            }
            Ok(_) => {
                tracing::info!(task_id = %task.id, "woke a task because a linked pull request needs attention");
                true
            }
            Err(error) => {
                tracing::warn!(task_id = %task.id, %error, "could not clear a held review request");
                false
            }
        }
    }
}
