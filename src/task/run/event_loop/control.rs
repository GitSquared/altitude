use crate::{
    isolation::{Command, IsolationEngine},
    task::{Task, TaskExecutionMode},
};

use super::{ActiveRun, RequestedControl, TaskEventLoop, TaskExecutionUpdate};

impl<E> TaskEventLoop<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub fn has_active_run(&self, task_id: &str) -> bool {
        self.runs
            .lock()
            .expect("task run lock poisoned")
            .contains_key(task_id)
    }

    pub fn take_control(&self, task_id: &str) -> bool {
        self.request_control(task_id, TaskExecutionMode::Operator)
    }

    pub fn start_operator_control(&self, task: Task) {
        let event_loop = self.clone();
        tokio::spawn(async move {
            let result = event_loop.service.start_operator(task).await;
            let operator_task = result.as_ref().ok().map(|(task, _, _)| task.clone());
            event_loop.watch_agent_run(result);
            if let Some(task) = operator_task {
                let _ = event_loop
                    .events
                    .send(TaskExecutionUpdate::OperatorReady(task));
            }
        });
    }

    pub fn return_to_autopilot(&self, task_id: &str) -> bool {
        self.request_control(task_id, TaskExecutionMode::Autopilot)
    }

    pub(super) fn request_control(&self, task_id: &str, requested_mode: TaskExecutionMode) -> bool {
        let agent_run_id = {
            let runs = self.runs.lock().expect("task run lock poisoned");
            let Some(run) = runs.get(task_id) else {
                tracing::warn!(task_id = %task_id, ?requested_mode, "refusing a control request: the task has no run to receive it");
                return false;
            };
            run.agent_run_id.clone()
        };
        // Register before interrupting, so a run that ends on the interrupt cannot
        // terminate before its request is on record.
        self.control_requests
            .lock()
            .expect("task control request lock poisoned")
            .insert(
                task_id.to_owned(),
                RequestedControl {
                    agent_run_id: agent_run_id.clone(),
                    requested_mode,
                },
            );
        let service = self.service.clone();
        let id = task_id.to_owned();
        tokio::spawn(async move {
            if let Err(error) = service
                .request_control(&id, requested_mode, &agent_run_id)
                .await
            {
                tracing::error!(task_id = %id, %error, "could not record a control request");
            }
        });
        if let Some(run) = self
            .runs
            .lock()
            .expect("task run lock poisoned")
            .get(task_id)
        {
            let _ = run.commands.send(Command::Interrupt);
        }
        true
    }

    pub(super) fn take_active_run(&self, task_id: &str) -> Option<ActiveRun> {
        self.runs
            .lock()
            .expect("task run lock poisoned")
            .remove(task_id)
    }

    pub(super) fn take_control_request(
        &self,
        task_id: &str,
        agent_run_id: &str,
    ) -> Option<TaskExecutionMode> {
        let mut requests = self
            .control_requests
            .lock()
            .expect("task control request lock poisoned");
        match requests.remove(task_id) {
            Some(request) if request.agent_run_id == agent_run_id => Some(request.requested_mode),
            Some(_) => {
                tracing::debug!(task_id = %task_id, "dropping a control request addressed to an earlier run");
                None
            }
            None => None,
        }
    }

    pub(super) fn mark_suspending(&self, task_id: &str) {
        if let Some(run) = self
            .runs
            .lock()
            .expect("task run lock poisoned")
            .get_mut(task_id)
        {
            run.suspending = true;
        }
    }
}
