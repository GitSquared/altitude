use tokio::sync::mpsc;

use crate::{
    isolation::IsolationEngine,
    task::{TaskEventLoop, TaskOperatorSignalThread},
};

use super::*;

impl App {
    pub(super) async fn apply_project_proposal<E>(
        &mut self,
        thread: &TaskOperatorSignalThread,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(proposal) = thread.proposal.clone() else {
            return;
        };
        let Some(project_id) = self
            .tasks
            .iter()
            .find(|task| task.id == thread.task_id)
            .and_then(|task| task.project())
            .map(|project| project.id.clone())
        else {
            self.say("This task's project is not in this machine's settings");
            return;
        };
        let Some(file) = self.settings_file.as_mut() else {
            self.problem("Settings are unavailable; fix the file and reopen");
            return;
        };
        let mut settings = file.settings().clone();
        let Some(mut project) = settings.project(&project_id).cloned() else {
            self.say("That project is no longer in the settings file");
            return;
        };
        if let Some(command) = &proposal.setup_command {
            project.setup_command = command.clone();
        }
        if let Some(caches) = &proposal.caches {
            project.caches = caches.clone();
        }
        if let Err(error) = project.validate() {
            self.say(error.to_string());
            return;
        }
        settings.add_project(project);
        if let Err(error) = file.save(settings) {
            self.problem(error.to_string());
            return;
        }
        self.say("Applied the project change · the environment rebuilds on the next task");
        let answer = "Applied the change to the project's setup here.".to_owned();
        if self.remote_signals.is_some() {
            self.act_on_remote_signal(
                thread.id.clone(),
                RemoteSignalAction::Reply,
                Some(answer),
                remote_signals,
            );
            self.act_on_remote_signal(
                thread.id.clone(),
                RemoteSignalAction::Resolve(true),
                None,
                remote_signals,
            );
        } else {
            event_loop.settle_signal(thread.task_id.clone(), thread.id.clone(), answer);
            self.resolve_signal_locally(&thread.id, true);
        }
    }
}
