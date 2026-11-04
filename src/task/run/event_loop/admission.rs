use std::sync::atomic::Ordering;

use tokio::time;

use crate::isolation::{IsolationEngine, SandboxCapacity};

use crate::task::{
    Task, TaskService,
    queue::{tasks_waiting_to_start, waits_for_room},
    wants_a_slot,
};

use crate::isolation::capacity::SAMPLE_INTERVAL;
use crate::task::TaskEventLoop;

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
                let result = event_loop.service.start(task.clone()).await;
                event_loop.watch_agent_run(result);
                event_loop
                    .starting
                    .lock()
                    .expect("starting lock poisoned")
                    .remove(&task.id);
                // Provisioning may have freed or taken room for something else.
                event_loop.admit();
            });
        }
    }

    pub(crate) fn service(&self) -> &TaskService<E> {
        &self.service
    }
}
