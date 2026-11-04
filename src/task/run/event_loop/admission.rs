use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::time;

use crate::isolation::{IsolationEngine, SandboxCapacity};

use crate::task::{
    Task, TaskExecutionUpdate, TaskService,
    queue::{tasks_waiting_to_start, waits_for_room},
    wants_a_slot,
};

use crate::isolation::capacity::SAMPLE_INTERVAL;
use crate::task::TaskEventLoop;

// A start that is still going after ten minutes is one nothing is going to finish:
// provisioning or the runtime has wedged, and the future holding the task at the tip
// of the queue would never return, so nothing after it on the machine would move
// either. The budget is what turns that into a task that stopped, and a machine that
// takes the next one.
const START_BUDGET: Duration = Duration::from_secs(10 * 60);

// A start is minutes of silence in a log. At debug, saying so every half minute is
// what tells a slow start from a stopped one, which otherwise read the same.
const START_HEARTBEAT: Duration = Duration::from_secs(30);

impl<E: IsolationEngine + Clone + 'static> TaskEventLoop<E> {
    pub fn sandbox_capacity(&self) -> SandboxCapacity {
        self.service.isolation.sandbox_capacity()
    }

    pub(crate) fn watch_host_capacity(&self) {
        let event_loop = self.clone();
        tokio::spawn(async move {
            let mut tick = time::interval(SAMPLE_INTERVAL);
            loop {
                tick.tick().await;
                // Demand is the queue's business; how much room that leaves is
                // the host's, and only the second half is asked for here.
                let demand = match event_loop.service.list().await {
                    Ok(tasks) => tasks
                        .iter()
                        .filter(|task| wants_a_slot(task) && task.facts.deferred_review.is_none())
                        .count(),
                    Err(error) => {
                        tracing::warn!(%error, "could not read tasks to measure demand");
                        continue;
                    }
                };
                let capacity = event_loop.service.isolation.observe_demand(demand);
                tracing::debug!(
                    slots = capacity.slots,
                    ceiling_cpus = capacity.ceiling.cpus,
                    ceiling_mib = capacity.ceiling.memory_mib,
                    demand,
                    "observed host capacity"
                );
                event_loop.admit();
            }
        });
    }

    // A start in flight has nothing to say until it ends, which is why a wedged one
    // used to look exactly like a working one from the outside.
    pub(crate) fn watch_starting(&self) {
        let event_loop = self.clone();
        tokio::spawn(async move {
            let mut tick = time::interval(START_HEARTBEAT);
            tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let starting: Vec<String> = event_loop
                    .starting
                    .lock()
                    .expect("starting lock poisoned")
                    .iter()
                    .cloned()
                    .collect();
                for task_id in starting {
                    let task = match event_loop.task(&task_id).await {
                        Ok(task) => task,
                        Err(error) => {
                            tracing::debug!(task = %task_id, %error, "could not read a task that is starting");
                            continue;
                        }
                    };
                    tracing::debug!(
                        task = %task_id,
                        stage = %event_loop.starting_stage(&task_id, task.as_ref()),
                        "a start is still going"
                    );
                }
            }
        });
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

    // Whether another task would be admitted now. A caller asks this rather than
    // reading a slot count and comparing it: how many sandboxes the machine keeps
    // is its own moving answer, and what that means for the next task is this rule.
    pub(crate) fn has_room_for_another(&self, tasks: &[Task]) -> bool {
        self.free_slots(tasks) > 0
    }

    // A task being started has no capsule yet, so the queue still counts it as
    // waiting for room. What separates it from the tasks behind it is that this
    // machine is already working on it.
    pub(crate) fn is_starting(&self, task_id: &str) -> bool {
        self.starting
            .lock()
            .expect("starting lock poisoned")
            .contains(task_id)
    }

    // A capsule that exists is a slot in use. A task being started has no capsule
    // yet but is about to want one, so it counts too. One count, because a second
    // copy of this arithmetic is one that can disagree with admission.
    fn free_slots(&self, tasks: &[Task]) -> usize {
        let occupied = tasks
            .iter()
            .filter(|task| task.facts.sandbox_available)
            .count();
        let taken = occupied + self.starting.lock().expect("starting lock poisoned").len();
        self.service
            .isolation
            .sandbox_capacity()
            .free_slots_after(taken)
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

    async fn admit_waiting(&self) {
        if !self.admission_enabled.load(Ordering::Acquire) {
            return;
        }
        let mut tasks = match self.service.list().await {
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
                Ok(task) => {
                    if let Some(known) = tasks.iter_mut().find(|known| known.id == task.id) {
                        tracing::info!(task_id = %task.id, "a usage limit cleared");
                        *known = task;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "could not put back a task whose usage limit cleared");
                }
            }
        }
        let room = self.free_slots(&tasks);
        // The queue is the one the screens show: same order, same rule about who
        // is waiting for room. Only the tasks already being started drop out.
        let starting = self
            .starting
            .lock()
            .expect("starting lock poisoned")
            .clone();
        let waiting: Vec<&Task> = tasks_waiting_to_start(&tasks)
            .into_iter()
            .filter(|task| !starting.contains(&task.id))
            .collect();
        // Why nothing started is the question a quiet machine raises, and this is the
        // pass that answers it.
        tracing::debug!(
            room,
            occupied = tasks
                .iter()
                .filter(|task| task.facts.sandbox_available)
                .count(),
            starting = starting.len(),
            waiting = waiting.len(),
            "an admission pass is deciding"
        );
        let mut fresh = 0;
        for task in waiting {
            if waits_for_room(task) {
                if fresh >= room {
                    continue;
                }
                fresh += 1;
            }
            if !self.admission_enabled.load(Ordering::Acquire) {
                break;
            }
            if !self
                .starting
                .lock()
                .expect("starting lock poisoned")
                .insert(task.id.clone())
            {
                continue;
            }
            let event_loop = self.clone();
            let task = task.clone();
            tokio::spawn(async move {
                // A start is minutes of work with nothing to show for it until it is
                // over, which is the state an operator and a log both need to see.
                let began = Instant::now();
                let task_id = task.id.clone();
                tracing::info!(task = %task_id, "starting a task");
                let result =
                    time::timeout(START_BUDGET, event_loop.service.start(task.clone())).await;
                match result {
                    Ok(started) => {
                        tracing::info!(
                            task = %task_id,
                            elapsed_ms = began.elapsed().as_millis() as u64,
                            started = started.is_ok(),
                            "a task's start is over"
                        );
                        event_loop.watch_agent_run(started);
                    }
                    Err(_) => event_loop.give_up_starting(task).await,
                }
                event_loop
                    .starting
                    .lock()
                    .expect("starting lock poisoned")
                    .remove(&task_id);
                // Provisioning may have freed or taken room for something else.
                event_loop.admit();
            });
        }
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
            budget_s = START_BUDGET.as_secs(),
            stage = %stage,
            "a start outlived its budget"
        );
        let message = format!(
            "Starting this task took longer than {} minutes without finishing, so the machine \
             stopped trying. It was still {stage}.",
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
