//! The order work is served in, in one place.
//!
//! The scheduler and the screens both need to know what is waiting and in what
//! order. A second copy of this rule would be the copy that drifts, and it would
//! drift in the worst way: a client saying "3 ahead" while the machine walks a
//! different queue.

use super::{Task, TaskExecutionStatus};

/// Tasks that want to start and are not waiting on a review, oldest first.
///
/// This is the order admission serves them in, and it is the list a queue
/// position is counted from. The service lists newest first; the queue runs the
/// other way.
pub fn tasks_waiting_to_start(tasks: &[Task]) -> Vec<&Task> {
    let mut waiting: Vec<&Task> = tasks
        .iter()
        .filter(|task| task.facts.execution_status == TaskExecutionStatus::Pending)
        // A task with a held review is the scheduler's: it is not waiting for
        // room, it is waiting for a person or a check.
        .filter(|task| task.facts.deferred_review.is_none())
        .collect();
    waiting.reverse();
    waiting
}

/// Whether this task needs room on the host before it can start.
///
/// A task that already holds a capsule starts immediately: its room is taken.
pub fn waits_for_room(task: &Task) -> bool {
    !task.facts.sandbox_available
}

/// How many tasks are ahead of this one in the queue, if it is in the queue.
///
/// `None` means it is not waiting for room — it is starting, running, or done.
pub fn tasks_ahead_in_queue(tasks: &[Task], task_id: &str) -> Option<usize> {
    tasks_waiting_to_start(tasks)
        .into_iter()
        .filter(|task| waits_for_room(task))
        .enumerate()
        .find(|(_, task)| task.id == task_id)
        .map(|(ahead, _)| ahead)
}
