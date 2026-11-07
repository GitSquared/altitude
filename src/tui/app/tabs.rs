use crate::{remote::TaskSummary, task::Task, tracker::TrackerKind, tui::screens::tasks::Standing};

use super::*;

impl App {
    pub(super) fn visible_counts(&self) -> (usize, usize) {
        let project = self.active_project_id();
        (
            self.tasks_in(&project).count(),
            self.remote_tasks_in(&project).count(),
        )
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

    // What a tab holds: its project's work, and the work that has no project, which
    // has no other tab to be in. One copy of a task that exists twice, too, because a
    // task taken over here is not also listed under the machine that still holds it.
    // The list, the counts and the number on the tab are all read from here, so none
    // of them can name work the others do not.
    pub(in crate::tui) fn tasks_in(&self, project: &str) -> impl Iterator<Item = &Task> {
        self.tasks
            .iter()
            .filter(move |task| belongs_to(task.project().map(|own| own.id.as_str()), project))
    }

    pub(in crate::tui) fn remote_tasks_in(&self, project: &str) -> impl Iterator<Item = &HeldTask> {
        self.remote_tasks
            .iter()
            .filter(|held| !self.is_taken_over(&held.address, &held.summary.id))
            .filter(move |held| belongs_to(held.summary.project_id.as_deref(), project))
    }

    // What a tab's number counts: the rows in it that are waiting on a person rather
    // than on themselves, ranked the way the list ranks them.
    pub(in crate::tui) fn attention_in_project(&self, project: &str) -> usize {
        let local = self
            .tasks_in(project)
            .map(|task| Standing::of(&TaskSummary::of(task)));
        let remote = self
            .remote_tasks_in(project)
            .map(|held| Standing::of(&held.summary));
        local
            .chain(remote)
            .filter(|standing| standing.wants_a_person())
            .count()
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
