use crate::{task::Task, tracker::TrackerKind};

use super::*;

impl App {
    pub(super) fn visible_counts(&self) -> (usize, usize) {
        (
            self.visible_tasks().count(),
            self.visible_remote_tasks().count(),
        )
    }

    pub(super) fn in_active_tab(&self, project_id: Option<&str>) -> bool {
        match project_id {
            Some(id) => id == self.active_project_id(),
            None => true,
        }
    }

    pub(super) fn is_taken_over(&self, address: &str, id: &str) -> bool {
        self.superseded
            .iter()
            .any(|(from, taken)| from == address && taken == id)
            || self
                .pending_take
                .as_ref()
                .is_some_and(|(from, taking)| from == address && taking == id)
    }

    pub(super) fn scope_label(&self) -> String {
        let kind = TrackerKind::sole();
        format!("Task tracker {}", kind.scope_noun())
    }

    pub(super) fn remotes(&self) -> Vec<String> {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().remotes().to_vec())
            .unwrap_or_default()
    }

    /// The machines this client is waiting on: one that cannot be given work yet,
    /// or one it still owes something. A machine that is ready and owed nothing is
    /// not asked.
    pub(super) fn remotes_to_refresh(&self) -> Vec<String> {
        self.configured_remotes()
            .into_iter()
            .filter(|address| match self.remote_status.get(address) {
                Some(RemoteStatus::Asking | RemoteStatus::Preparing(_) | RemoteStatus::Syncing) => {
                    true
                }
                Some(RemoteStatus::Reached) => self.sync_for(address) == Sync::Configuration,
                _ => false,
            })
            .collect()
    }

    pub(in crate::tui) fn visible_tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks
            .iter()
            .filter(|task| self.in_active_tab(task.project().map(|project| project.id.as_str())))
    }

    pub(in crate::tui) fn visible_remote_tasks(&self) -> impl Iterator<Item = &HeldTask> {
        self.remote_tasks
            .iter()
            .filter(|held| !self.is_taken_over(&held.address, &held.summary.id))
            .filter(|held| self.in_active_tab(held.summary.project_id.as_deref()))
    }

    pub(in crate::tui) fn attention_in_project(&self, id: &str) -> usize {
        let local = self
            .tasks
            .iter()
            .filter(|task| {
                task.project().map(|project| project.id.as_str()) == Some(id)
                    && task.facts.blocking_signal_kind.is_some()
            })
            .count();
        let remote = self
            .remote_tasks
            .iter()
            .filter(|held| {
                held.summary.project_id.as_deref() == Some(id)
                    && held.summary.blocking_signal_kind.is_some()
            })
            .count();
        local + remote
    }

    pub(super) fn selected_ref(&self) -> Option<&TaskRef> {
        self.task_order.get(self.tasks_list.selected())
    }

    pub(super) fn selected_held(&self) -> Option<&HeldTask> {
        let TaskRef::Remote { address, id } = self.selected_ref()? else {
            return None;
        };
        self.remote_tasks
            .iter()
            .find(|held| &held.address == address && &held.summary.id == id)
    }

    pub(super) fn selected_is_remote(&self) -> bool {
        matches!(self.selected_ref(), Some(TaskRef::Remote { .. }))
    }
}
