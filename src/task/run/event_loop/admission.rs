use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::time;
use uuid::Uuid;

use crate::isolation::capacity::SAMPLE_INTERVAL;
use crate::isolation::{CommandSession, IsolationEngine, Pressure};

use crate::task::{
    Task, TaskAgentRun, TaskExecutionUpdate, TaskService, TaskServiceError,
    queue::{tasks_waiting_to_start, waits_for_room},
};

use crate::task::TaskEventLoop;

// What a bounded admission call hands back: the task it claimed, and the claim that
// authorises starting it. The caller owns the start from here; a successful return
// means the claim is durable and nothing more, not that a sandbox or an agent started.
// A caller that cannot dispatch what it was given must give the claim back, and a
// caller that exits first leaves it to the next startup, whose single-owner recovery
// releases it.
pub(crate) struct ClaimedTask {
    pub task: Task,
    pub claim_id: String,
}

// A start that stops getting further is one nothing is going to finish: provisioning
// or the runtime has wedged, and the future holding the task at the tip of the queue
// would never return, so nothing after it on the machine would move either. What the
// budget measures is progress and not duration, because a cold environment build is
// minutes of honest work: a start that keeps reaching new stages is a start working.
// Long enough to outlast the slowest single step a start has, which is an install of a
// project's dependencies on a host that has to build the image's layer for the first
// time. That step carries its own patience and reports it; this reads the stage, and a
// stage cannot change while one step is running. A budget shorter than that step turns a
// slow first build into a task that never builds the layer everyone after it restores.
const START_BUDGET: Duration = Duration::from_secs(35 * 60);

// A start is minutes of silence in a log. At debug, saying so every half minute is
// what tells a slow start from a stopped one, which otherwise read the same.
const START_HEARTBEAT: Duration = Duration::from_secs(30);

impl<E: IsolationEngine + Clone + 'static> TaskEventLoop<E> {
    pub fn pressure(&self) -> Option<Pressure> {
        self.service.isolation.pressure()
    }

    // The machine's own answer, taken on a clock: this is the whole of what a queue
    // knows about a host it does not measure itself.
    pub(crate) fn watch_pressure(&self) {
        let event_loop = self.clone();
        tokio::spawn(async move {
            let mut tick = time::interval(SAMPLE_INTERVAL);
            loop {
                tick.tick().await;
                let pressure = event_loop.service.isolation.observe_pressure();
                let room = event_loop.service.isolation.sandbox_room();
                tracing::debug!(
                    ?pressure,
                    promised_mib = room.promised_mib,
                    ceiling_mib = ?room.offered.map(|offered| offered.memory_mib),
                    "measured the machine"
                );
                event_loop.admit();
            }
        });
    }

    // A start in flight has nothing to say until it ends, which is why a wedged one
    // used to look exactly like a working one from the outside. It carries its own
    // clock: the stage it has reached is read from the record and said at debug, and
    // a stage that has not moved for the budget is the start that is over.
    async fn start_unless_wedged(
        &self,
        task: &Task,
    ) -> Option<Result<(Task, TaskAgentRun, CommandSession), TaskServiceError>> {
        let start = self.service.start(task.clone());
        tokio::pin!(start);
        let mut tick = time::interval(START_HEARTBEAT);
        tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        tick.tick().await;
        let mut stage = self.starting_stage(&task.id, Some(task));
        let mut reached_at = Instant::now();
        loop {
            tokio::select! {
                started = &mut start => return Some(started),
                _ = tick.tick() => {
                    let known = self.task(&task.id).await.ok().flatten();
                    let now = self.starting_stage(&task.id, known.as_ref());
                    if now == stage {
                        if reached_at.elapsed() >= START_BUDGET {
                            return None;
                        }
                    } else {
                        stage = now;
                        reached_at = Instant::now();
                    }
                    tracing::debug!(
                        task = %task.id,
                        stage = %stage,
                        unchanged_ms = reached_at.elapsed().as_millis() as u64,
                        "a start is still going"
                    );
                }
            }
        }
    }

    // Where a start has got to, read from what is recorded about the task rather than
    // from the future doing it: the whole point is to say something at a moment when
    // that future is not saying anything.
    fn starting_stage(&self, task_id: &str, task: Option<&Task>) -> String {
        if self
            .runs
            .lock()
            .expect("task run lock poisoned")
            .contains_key(task_id)
        {
            return "the agent is running".to_owned();
        }
        if let Some(step) = self.service.environment_step(task_id) {
            return format!(
                "building the environment · {} · step {} of {}",
                step.label,
                step.completed + 1,
                step.total
            );
        }
        match task {
            Some(task) if task.facts.sandbox_available => "launching the agent".to_owned(),
            Some(_) => "making the sandbox".to_owned(),
            None => "a task that is no longer recorded".to_owned(),
        }
    }

    pub fn enable_admission(&self) {
        self.admission_enabled.store(true, Ordering::Release);
        self.admit();
    }

    pub fn admit(&self) {
        if !self.admission_enabled.load(Ordering::Acquire) {
            return;
        }
        let event_loop = self.clone();
        tokio::spawn(async move { event_loop.admit_waiting().await });
    }

    // The bounded half of admission: choose the next task the machine may start,
    // claim it durably, and return before any sandbox is provisioned or a future is
    // spawned. `None` says neither the queue nor the machine had an offer for this
    // call. The claim re-checks the queue's own rule under a write lock, so an
    // overlapping call cannot take the same task and a task that was removed, held,
    // borrowed or already claimed is refused there.
    pub(crate) async fn claim_due(&self) -> Result<Option<ClaimedTask>, TaskServiceError> {
        let tasks = self.service.list_in_play().await?;
        // The tasks already claimed are being started. The claim is the durable fact
        // that replaced the process-local set, so a restart and a second caller read
        // the same answer.
        let claimed: HashSet<&str> = tasks
            .iter()
            .filter(|task| task.facts.active_start_claim().is_some())
            .map(|task| task.id.as_str())
            .collect();
        // A machine busy with something that is not ours must not starve the queue, so
        // the valve only holds work back while the machine is carrying some of it.
        let carrying = tasks.iter().any(|task| task.facts.sandbox_available) || !claimed.is_empty();
        let stalled = self.service.isolation.pressure().filter(|_| carrying);
        // A start that has not booted yet is invisible in the load that would have said
        // no to it, so landings are counted rather than measured.
        let landing = claimed.len();
        let waiting: Vec<&Task> = tasks_waiting_to_start(&tasks)
            .into_iter()
            .filter(|task| !claimed.contains(task.id.as_str()))
            .collect();
        // Asked of the machine rather than taken from the last reading: what has been
        // promised changes when a sandbox is made, and a start admitted in the same
        // second as another would otherwise be admitted against memory that is gone.
        let room = self.service.isolation.sandbox_room();
        let may_land = self.service.isolation.start_landings().min(room.fitting);
        // Why nothing started is the question a quiet machine raises, and this is the
        // call that answers it.
        tracing::debug!(
            ?stalled,
            ceiling_mib = ?room.offered.map(|offered| offered.memory_mib),
            promised_mib = room.promised_mib,
            fitting = room.fitting,
            landing,
            waiting = waiting.len(),
            "an admission call is deciding"
        );
        for task in waiting {
            if waits_for_room(task) && (stalled.is_some() || landing >= may_land) {
                continue;
            }
            let claim_id = Uuid::new_v4().to_string();
            let claimed_task = match self
                .service
                .store
                .claim_start(&task.id, &claim_id, &self.start_owner)
                .await
            {
                Ok(Some(claimed)) => claimed,
                // Another caller got here first, or the task stopped being due between
                // the read and the write. Losing the race is an ordinary answer: this
                // call tries the next candidate, or returns and lets the next pass try.
                Ok(None) => continue,
                Err(error) => return Err(TaskServiceError::Task(error)),
            };
            debug_assert_eq!(
                claimed_task
                    .facts
                    .active_start_claim()
                    .map(|claim| claim.id.as_str()),
                Some(claim_id.as_str())
            );
            return Ok(Some(ClaimedTask {
                task: claimed_task,
                claim_id,
            }));
        }
        Ok(None)
    }

    async fn admit_waiting(&self) {
        if !self.admission_enabled.load(Ordering::Acquire) {
            return;
        }
        let tasks = match self.service.list_in_play().await {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "could not read tasks to admit work");
                return;
            }
        };
        // A limit that has cleared is a task that starts again by itself. It is put
        // back before the queue is read, so the same pass that measures room is the
        // one that starts it, and no clock of its own has to be kept for it.
        for result in self.service.resume_limits_due(&tasks).await {
            match result {
                Ok(task) => tracing::info!(task_id = %task.id, "a usage limit cleared"),
                Err(error) => {
                    tracing::warn!(%error, "could not put back a task whose usage limit cleared");
                }
            }
        }
        // The bounded operation claims one task and returns; keeping the start it
        // authorises alive is this loop's work. The claim is durable, so each pass of
        // the loop sees the tasks it already handed out and counts them against room
        // exactly as one long pass did.
        loop {
            if !self.admission_enabled.load(Ordering::Acquire) {
                break;
            }
            let claimed = match self.claim_due().await {
                Ok(Some(claimed)) => claimed,
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(%error, "could not claim a task to start");
                    break;
                }
            };
            let event_loop = self.clone();
            tokio::spawn(async move { event_loop.advance(claimed).await });
        }
    }

    // The unbounded part: provision and launch, then give the claim back if the run did
    // not spend it. A start that failed or was cancelled releases its claim here rather
    // than leaving a restart to clean it up, and a start that ran out of progress still
    // releases it before failing the task.
    async fn advance(&self, claimed: ClaimedTask) {
        let ClaimedTask { task, claim_id } = claimed;
        // A start is minutes of work with nothing to show for it until it is over,
        // which is the state an operator and a log both need to see.
        let began = Instant::now();
        let task_id = task.id.clone();
        // The first question asked of a machine that ran out of memory later.
        let room = self.service.isolation.sandbox_room();
        tracing::info!(
            task = %task_id,
            claim = %claim_id,
            ceiling_mib = ?room.offered.map(|offered| offered.memory_mib),
            promised_mib = room.promised_mib,
            pressure = ?self.service.isolation.pressure(),
            "starting a task"
        );
        match self.start_unless_wedged(&task).await {
            Some(started) => {
                tracing::info!(
                    task = %task_id,
                    elapsed_ms = began.elapsed().as_millis() as u64,
                    started = started.is_ok(),
                    "a task's start is over"
                );
                self.watch_agent_run(started);
            }
            None => self.give_up_starting(task).await,
        }
        // A start that produced a run has already spent the claim; one that failed,
        // was cancelled, or ran out of progress gives it back so the next pass can try
        // again.
        if let Err(error) = self
            .service
            .store
            .relinquish_start(&task_id, &claim_id)
            .await
        {
            tracing::warn!(task = %task_id, %error, "could not release a start claim");
        }
        // Provisioning may have freed or taken room for something else.
        self.admit();
    }

    // The start future was dropped where it stood, so nothing it was doing will
    // report anything ever again. The task is ended by its record instead: it stops
    // where an operator can see why, and whatever sandbox the future had already made
    // is released from the same record rather than from the future that made it. One
    // a record cannot name — a half-built environment — is removed by the next
    // attempt at that environment, which starts by clearing its own leftovers.
    async fn give_up_starting(&self, task: Task) {
        let held = match self.service.list().await {
            Ok(tasks) => tasks.into_iter().find(|held| held.id == task.id),
            Err(error) => {
                tracing::warn!(task = %task.id, %error, "could not read a task whose start ran out");
                return;
            }
        };
        // A task deleted while it was starting has already said what it has to say.
        let Some(held) = held else {
            return;
        };
        let stage = self.starting_stage(&held.id, Some(&held));
        tracing::error!(
            task = %held.id,
            unchanged_s = START_BUDGET.as_secs(),
            stage = %stage,
            "a start stopped making progress"
        );
        let message = format!(
            "Starting this task made no progress for {} minutes, so the machine stopped \
             trying. It was still {stage}.",
            START_BUDGET.as_secs() / 60
        );
        let held = match self.service.fail(&held, message).await {
            Ok(failed) => failed,
            Err(error) => {
                tracing::warn!(task = %held.id, %error, "could not end a task whose start ran out");
                return;
            }
        };
        let held = match self.service.release_resources(&held).await {
            Ok(released) => released,
            Err(error) => {
                tracing::warn!(task = %held.id, %error, "a task whose start ran out needs cleanup");
                held
            }
        };
        let _ = self
            .events
            .send(TaskExecutionUpdate::Finished(held.id.clone(), Ok(held)));
    }

    pub(crate) fn service(&self) -> &TaskService<E> {
        &self.service
    }
}
