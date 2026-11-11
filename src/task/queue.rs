use crate::{
    isolation::{EnvironmentProgress, Pressure},
    task::{Task, TaskExecutionStatus},
};

// Where a task stands on the machine: what is in front of it in the queue, what the
// machine already has on it, and what is being done to this task if the machine has
// picked it up. One answer, because a queue number alone cannot say whether a wait is
// a queue, a machine with no room, or a start under way.
pub struct QueueStanding {
    pub ahead: usize,
    pub on_machine: usize,
    pub holding: Option<Pressure>,
    pub start: Option<Start>,
}

// A start is minutes of work before the first sandbox exists, and the steps of an
// environment build are what fills them. Calling that a wait for a sandbox told the
// operator nothing about a machine that was working the whole time.
pub enum Start {
    Provisioning,
    Building(EnvironmentProgress),
}

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
