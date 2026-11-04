use super::{Task, TaskExecutionStatus};

pub fn tasks_waiting_to_start(tasks: &[Task]) -> Vec<&Task> {
    let mut waiting: Vec<&Task> = tasks
        .iter()
        .filter(|task| task.facts.execution_status == TaskExecutionStatus::Pending)
        // A task with a held review is the scheduler's: it is not waiting for
        // room, it is waiting for a person or a check.
        .filter(|task| task.facts.deferred_review.is_none())
        // A borrowed task is worked at a desk or not at all, so it never enters a
        // queue: scheduling it would be this machine running someone else's work.
        .filter(|task| !task.facts.is_borrowed())
        .collect();
    waiting.reverse();
    waiting
}

pub fn waits_for_room(task: &Task) -> bool {
    !task.facts.sandbox_available
}

// The order the machine would take these in, and the one the screens show. It is one
// list because a queue numbered in one place and served from another is two answers
// to the same question.
pub fn tasks_waiting_for_room(tasks: &[Task]) -> Vec<&Task> {
    tasks_waiting_to_start(tasks)
        .into_iter()
        .filter(|task| waits_for_room(task))
        .collect()
}

pub fn place_in_queue(tasks: &[Task], task_id: &str) -> Option<usize> {
    tasks_waiting_for_room(tasks)
        .iter()
        .position(|task| task.id == task_id)
}
