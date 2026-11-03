use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use crate::{
    agent::AgentKind,
    project::Project,
    task::{Task, TaskExecutionStatus},
    tui::screens,
};

use crate::tui::widgets::alert::Alert;
use screens::settings::{Depth, Focus, Section, SettingsHit};

use super::*;

impl App {
    pub(in crate::tui) fn say(&mut self, message: impl Into<String>) {
        self.alert = Some(Alert::note(message));
    }

    pub(in crate::tui) fn problem(&mut self, message: impl Into<String>) {
        self.alert = Some(Alert::problem(message));
    }

    pub(in crate::tui) fn dismiss_alert(&mut self) {
        self.alert = None;
    }

    pub(in crate::tui) fn replace_task(&mut self, task: Task) {
        let Some(current) = self.tasks.iter_mut().find(|current| current.id == task.id) else {
            self.tasks.insert(0, task);
            return;
        };
        // A task that has just been completed leaves the grid the moment the
        // record says so, rather than waiting for the next refresh.
        let stays = is_listed(&task);
        *current = task;
        if !stays {
            self.tasks.retain(is_listed);
            self.clamp_selection();
        }
    }

    pub(in crate::tui) fn list_tasks(&mut self, tasks: Vec<Task>) {
        self.tasks = listed(tasks);
        self.clamp_selection();
    }

    pub(in crate::tui) fn task_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = self.project_targets.at(mouse.column, mouse.row).copied() {
                    let id = self.projects().get(index).map(|project| project.id.clone());
                    if let Some(id) = id {
                        self.focus_project(&id);
                    }
                } else if let Some(index) = self.task_targets.at(mouse.column, mouse.row).copied() {
                    self.selected = index;
                }
            }
            MouseEventKind::ScrollUp => {
                let columns = self.grid_columns();
                self.select_adjacent(-(columns as isize), columns);
            }
            MouseEventKind::ScrollDown => {
                let columns = self.grid_columns();
                self.select_adjacent(columns as isize, columns);
            }
            _ => {}
        }
    }

    pub(in crate::tui) fn panel_mouse(&mut self, mouse: MouseEvent) {
        if self.panel.form.is_some() {
            let has_repositories = self
                .panel
                .form
                .as_ref()
                .is_some_and(|form| form.repositories.is_some());
            if has_repositories {
                let chosen = self
                    .panel
                    .form
                    .as_mut()
                    .and_then(|form| form.repository_mouse(mouse));
                if let Some(repository) = chosen
                    && let Some(form) = self.panel.form.as_mut()
                {
                    form.choose_repository(&repository.url);
                }
                return;
            }
            // A click on a list row is a choice, and it goes to the list before
            // it goes to whatever the row is drawn over.
            if let Some(form) = self.panel.form.as_mut()
                && form.pill_mouse(mouse)
            {
                return;
            }
            let target = self
                .panel
                .form
                .as_ref()
                .and_then(|form| form.hit(mouse.column, mouse.row));
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if let Some(form) = self.panel.form.as_mut() {
                        form.scroll_by(true);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(form) = self.panel.form.as_mut() {
                        form.scroll_by(false);
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    let Some(focus) = target else {
                        return;
                    };
                    if let Some(form) = self.panel.form.as_mut() {
                        form.focus = focus;
                        // A pill is pressed rather than selected: one click opens
                        // the list it says it opens.
                        if focus.is_a_pill() {
                            form.open_pill();
                        }
                    }
                    match focus {
                        Focus::Cancel => self.panel.form = None,
                        Focus::Save => {
                            if let Some(form) = self.panel.form.take()
                                && !self.save_form(&form)
                            {
                                self.panel.form = Some(form);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(target) = self.settings_targets.at(mouse.column, mouse.row).copied()
                else {
                    return;
                };
                match target {
                    SettingsHit::Section(index) => {
                        if let Some(section) = Section::ALL.get(index) {
                            self.panel.section = *section;
                            self.panel.cursor.reset();
                            self.panel.depth = Depth::Rail;
                        }
                    }
                    SettingsHit::Row(index) => {
                        self.panel.cursor.select(index);
                        self.panel.depth = Depth::List;
                    }
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => self.scroll_mouse(mouse),
            _ => {}
        }
    }

    pub(in crate::tui) fn remote_runs_projects(&self, address: &str) -> bool {
        self.projects()
            .iter()
            .any(|project| project.host.as_deref() == Some(address))
    }

    pub(in crate::tui) fn remote_was_given_config(&self, address: &str) -> bool {
        self.remote_synced.contains_key(address)
    }

    pub(in crate::tui) fn active_project_id(&self) -> String {
        let projects = self.projects();
        self.active_project
            .as_ref()
            .filter(|id| projects.iter().any(|project| &project.id == *id))
            .cloned()
            .or_else(|| projects.first().map(|project| project.id.clone()))
            .unwrap_or_default()
    }

    pub(in crate::tui) fn focus_project(&mut self, id: &str) {
        if self.active_project.as_deref() != Some(id) {
            self.active_project = Some(id.to_owned());
            self.selected = 0;
        }
    }

    pub(in crate::tui) fn switch_project(&mut self, forward: bool) {
        let projects = self.projects();
        if projects.len() < 2 {
            return;
        }
        let active = self.active_project_id();
        let at = projects
            .iter()
            .position(|project| project.id == active)
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % projects.len()
        } else {
            (at + projects.len() - 1) % projects.len()
        };
        self.focus_project(&projects[next].id);
    }

    pub(in crate::tui) fn projects(&self) -> Vec<Project> {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().projects().to_vec())
            .unwrap_or_default()
    }

    pub(in crate::tui) fn reload_settings(&mut self) -> bool {
        match self.config.load_or_create_settings() {
            Ok(file) => {
                self.settings_file = Some(file);
                self.alert = None;
                true
            }
            Err(error) => {
                self.settings_file = None;
                self.problem(format!(
                    "Altitude could not read {}.\n\n{error}",
                    self.config.settings_path().display()
                ));
                false
            }
        }
    }

    pub(in crate::tui) fn agent(&self) -> Option<AgentKind> {
        self.settings_file
            .as_ref()
            .and_then(|file| file.settings().agent())
    }

    pub(in crate::tui) fn choose_agent(&mut self, agent: AgentKind) {
        let Some(file) = self.settings_file.as_mut() else {
            self.problem("Settings are unavailable; fix the file and reopen");
            return;
        };
        let mut proposed = file.settings().clone();
        proposed.choose_agent(agent);
        match file.save(proposed) {
            Ok(()) => self.alert = None,
            Err(error) => self.problem(error.to_string()),
        }
    }

    pub(in crate::tui) fn selected_task(&self) -> Option<Task> {
        self.visible_tasks().nth(self.selected).cloned()
    }

    pub(in crate::tui) fn remove_task(&mut self, id: &str) {
        self.removing.remove(id);
        if let Some(index) = self.tasks.iter().position(|task| task.id == id) {
            self.tasks.remove(index);
            self.clamp_selection();
        }
    }

    pub(in crate::tui) fn begin_removing(&mut self, id: &str) {
        self.removing.insert(id.to_owned());
    }

    pub(in crate::tui) fn cancel_removing(&mut self, id: &str) {
        self.removing.remove(id);
    }

    pub(in crate::tui) fn is_removing(&self, id: &str) -> bool {
        self.removing.contains(id)
    }

    pub(in crate::tui) fn is_preparing(&self) -> bool {
        self.tasks
            .iter()
            .any(|task| task.facts.execution_status == TaskExecutionStatus::Pending)
    }

    pub(in crate::tui) fn is_awaiting_progress(&self) -> bool {
        self.is_preparing()
            || self.is_asking_remotes()
            || self.is_asking_credentials()
            || self.is_awaiting_tracker()
            || self.is_connecting_tracker()
            || matches!(self.credential_shell, CredentialShellState::Opening { .. })
    }

    pub(in crate::tui) fn open_shell(&self) -> Option<&ShellId> {
        match &self.foreground {
            Foreground::Shell(id) => Some(id),
            _ => None,
        }
    }

    pub(in crate::tui) fn credentials_over_card(&self) -> bool {
        matches!(self.foreground, Foreground::Shell(ShellId::Credentials(_)))
            && self
                .shell_behind
                .under()
                .is_some_and(Foreground::shows_get_started)
    }
}
