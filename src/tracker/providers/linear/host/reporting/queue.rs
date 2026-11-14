use super::*;

impl RemoteTracker {
    // Answers when a line that says nothing new becomes worth saying again, which is
    // the only clock this pass keeps: a task waiting in a queue would otherwise say
    // nothing at all until it moved.
    pub(in crate::tracker::providers::linear::host) async fn report_queue_standing(
        &self,
        host: &dyn DelegatedWork,
    ) -> Option<Instant> {
        let mut next = None;
        let bound = match host.bound_tasks().await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::debug!(%error, "could not read bound tasks to report queue standing");
                return None;
            }
        };
        // A machine reporting on nothing reads the queue not at all.
        if bound.is_empty() {
            return None;
        }
        let standings = match host.queue_standings().await {
            Ok(standings) => standings,
            Err(error) => {
                tracing::debug!(%error, "could not read the queue to report where tasks stand");
                return None;
            }
        };
        for task in bound {
            let Some(report) = self.report_for_task(&task.task_id) else {
                continue;
            };
            let Some(standing) = standings.get(&task.task_id) else {
                self.queue_notices
                    .lock()
                    .expect("queue notices lock poisoned")
                    .remove(&task.task_id);
                continue;
            };
            let since = self.queue_state_since(&task.task_id, standing);
            let state = queue_notice_state(standing);
            let body = queue_notice_line(&state, since);
            if !self.queue_notice_is_due(&task.task_id, standing.ahead, &state) {
                if let Some(at) = self.queue_notice_due_at(&task.task_id, standing.ahead, &state) {
                    next = Some(next.map_or(at, |held: Instant| held.min(at)));
                }
                continue;
            }
            let stamp = now_ms();
            let activity_id = stable_id(
                &report.session,
                &format!("queue-{stamp}-{}", standing.ahead),
            );
            if let Err(error) = self
                .say_system_ephemerally(&report, &activity_id, body)
                .await
            {
                tracing::debug!(task = %task.task_id, %error, "could not say where a task stands");
                continue;
            }
            self.queue_notices
                .lock()
                .expect("queue notices lock poisoned")
                .insert(
                    task.task_id.clone(),
                    QueueNotice {
                        starting: standing.start.is_some(),
                        state,
                        since,
                        at: Instant::now(),
                    },
                );
        }
        next
    }

    // When the line started saying this, so the number on it is the operator's own
    // clock. It resets when the task moves from waiting to starting and not when the
    // machine's counts move under it: a second slot filling is not a new wait.
    pub(in crate::tracker::providers::linear::host) fn queue_state_since(
        &self,
        task_id: &str,
        standing: &QueueStanding,
    ) -> Instant {
        let notices = self
            .queue_notices
            .lock()
            .expect("queue notices lock poisoned");
        match notices.get(task_id) {
            Some(notice) if notice.starting == standing.start.is_some() => notice.since,
            _ => Instant::now(),
        }
    }

    // When a line that says what the last one said becomes worth saying again.
    fn queue_notice_due_at(&self, task_id: &str, ahead: usize, state: &str) -> Option<Instant> {
        let notices = self
            .queue_notices
            .lock()
            .expect("queue notices lock poisoned");
        let last = notices.get(task_id)?;
        if last.state != state {
            return None;
        }
        Some(last.at + Self::queue_notice_interval(ahead))
    }

    pub(in crate::tracker::providers::linear::host) fn queue_notice_is_due(
        &self,
        task_id: &str,
        ahead: usize,
        state: &str,
    ) -> bool {
        let notices = self
            .queue_notices
            .lock()
            .expect("queue notices lock poisoned");
        let Some(last) = notices.get(task_id) else {
            return true;
        };
        // What the machine is doing changing is news even when the queue number
        // did not move: it is what tells a wait apart from a machine that stopped.
        if last.state != state {
            return true;
        }
        // The age is the only thing the line says while nothing moves, and it is
        // worth a refresh on its own for as long as a task is the next one up.
        last.at.elapsed() >= Self::queue_notice_interval(ahead)
    }

    fn queue_notice_interval(ahead: usize) -> Duration {
        if ahead == 0 {
            NEXT_UP_NOTICE_INTERVAL
        } else {
            QUEUE_NOTICE_INTERVAL
        }
    }
}

// Where a queued task stands, in one line, with how long it has stood there: the
// queue says what is in front of it, the machine says why it is not moving, and the
// age is what tells a slow wait apart from one nothing is going to end.
fn queue_notice_line(state: &str, since: Instant) -> String {
    match waited_words(since.elapsed()) {
        Some(waited) => format!("{state} · {waited}"),
        None => state.to_owned(),
    }
}

fn queue_notice_state(standing: &QueueStanding) -> String {
    // A task the machine has picked up is not waiting on the queue any more. The
    // minutes it spends starting are the ones an operator is most likely to be
    // watching, so they say what is being done rather than what is being waited for.
    match &standing.start {
        // Nothing has said what it is doing yet: claiming to be launching a sandbox
        // would be a guess, and a wrong one while a worker is still being fetched.
        Some(Start::Provisioning) => return "Starting on the remote".to_owned(),
        // No count: what an operator wants is what is being done now, and a number out
        // of a total that the machine doing its own work would not have is noise.
        Some(Start::Building(progress)) => {
            return format!("Starting on the remote · {}", progress.label);
        }
        None => {}
    }
    let place = match standing.ahead {
        0 => "Next up on the remote".to_owned(),
        1 => "Queued on the remote · 1 task ahead".to_owned(),
        ahead => format!("Queued on the remote · {ahead} tasks ahead"),
    };
    // What is in the way, when something is: the machine's own reason if it has one,
    // and otherwise the work it is already carrying.
    match standing.holding {
        Some(pressure) => format!("{place} · {}", pressure_words(pressure)),
        None => match standing.on_machine {
            0 => place,
            1 => format!("{place} · 1 task is on the machine"),
            on_machine => format!("{place} · {on_machine} tasks are on the machine"),
        },
    }
}

// The machine says what it is busy with; the operator reads why their task is waiting.
fn pressure_words(pressure: Pressure) -> &'static str {
    match pressure {
        Pressure::Cpu => "the machine is using as much cpu as it will take",
        Pressure::Memory => "the machine is using as much memory as it will take",
        Pressure::Disk => "the machine is low on disk",
        Pressure::Promised => "the machine has promised all the memory it can to the tasks on it",
    }
}

// Minutes, because that is the unit an operator waiting on work counts in, and the
// first minute is left to the next line that has something to say.
fn waited_words(elapsed: Duration) -> Option<String> {
    match elapsed.as_secs() / 60 {
        0 => None,
        minutes => Some(format!("{minutes}m")),
    }
}
