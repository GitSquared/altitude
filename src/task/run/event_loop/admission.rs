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
        let tasks = match self.service.list().await {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "could not read tasks to admit work");
                return;
            }
        };
        // A capsule that exists is a slot in use. A task being started has no
        // capsule yet but is about to want one, so it counts too.
        let occupied = tasks
            .iter()
            .filter(|task| task.facts.sandbox_available)
            .count();
        let taken = occupied + self.starting.lock().expect("starting lock poisoned").len();
        let room = self
            .service
            .isolation
            .sandbox_capacity()
            .free_slots_after(taken);
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
