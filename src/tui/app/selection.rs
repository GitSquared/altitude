use crate::project::Project;

use super::*;

impl App {
    pub(super) fn clamp_selection(&mut self) {
        let (local, remote) = self.visible_counts();
        self.selected = self.selected.min((local + remote).saturating_sub(1));
    }

    pub(in crate::tui) fn select_adjacent(&mut self, step: isize, columns: usize) {
        let (local, remote) = self.visible_counts();
        let count = local + remote;
        if count == 0 {
            return;
        }
        let moved = self.selected as isize + step;
        self.selected = moved.clamp(0, count as isize - 1) as usize;
        let _ = columns;
    }

    // A task this machine is holding for someone is not the operator's to schedule
    // and not theirs to move: it goes back where it came from when the desk stops,
    // which is not a key.
    pub(super) fn can_send_selected(&self) -> bool {
        let Some(task) = self
            .selected_task()
            .filter(|task| !task.facts.is_borrowed())
        else {
            return false;
        };
        task.project()
            .is_some_and(|project| matches!(self.project_machine(project), Ok(Some(_))))
    }

    pub(super) fn project_machine(&self, project: &Project) -> Result<Option<String>, String> {
        // The machine is the operator's setting for the project, not a fact the
        // task carries. A task taken over from a remote was built with that
        // machine's copy of the project, which names no machine at all, because a
        // remote does not name itself — so the setting decides and the record is
        // the fallback.
        let setting = self
            .projects()
            .into_iter()
            .find(|held| held.id == project.id);
        let Some(address) = setting
            .as_ref()
            .and_then(|held| held.host.as_ref())
            .or(project.host.as_ref())
        else {
            return Ok(None);
        };
        match self
            .task_hosts()
            .into_iter()
            .find(|host| host.address.as_deref() == Some(address.as_str()))
        {
            Some(host) if host.ready => Ok(Some(address.clone())),
            Some(host) => Err(format!(
                "New tasks for this project start on {}. It cannot take work yet.",
                host.label
            )),
            None => Err(format!(
                "New tasks for this project start on {address}, which is not a configured \
                 remote. Add it in Settings → Remotes, or change the project's machine."
            )),
        }
    }

    pub(in crate::tui) fn grid_columns(&self) -> usize {
        self.grid_columns.max(1)
    }
}
