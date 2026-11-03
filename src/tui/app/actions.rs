use ratatui::DefaultTerminal;
use tokio::{process::Command as HostCommand, sync::mpsc};

use crate::{
    credentials::Service,
    isolation::IsolationEngine,
    project::{Project, ProjectCommits, TrackerBinding, normalize_repository},
    task::{TaskEventLoop, TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus},
    tui::popups,
    tui::screens,
};

use popups::confirm::PendingConfirm;
use screens::settings::{Form, GitIdentity, RowKind, Setting, Target};
use screens::shell::Shell;

use super::*;

impl App {
    pub(in crate::tui) fn edit_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        self.panel.form = Some(match row.kind {
            RowKind::Switch { setting, on } => {
                self.toggle(setting, !on);
                return;
            }
            // The installation's key is made, not signed in to, so there is no
            // terminal to open for it.
            RowKind::GitIdentity(identity) => {
                match identity.saved {
                    Some(commits) => self.open_saved_commit_identity(commits),
                    None => self.open_git_identity(),
                }
                return;
            }
            RowKind::Credential(_) => {
                self.say("This is how remotes recognise this machine, and it is not edited here");
                return;
            }
            RowKind::Project { project } => Form::for_project(
                Target::Project {
                    id: project.id.clone(),
                },
                &project,
                &self.scope_label(),
                &self.remotes(),
                self.project_defaults(),
                self.saved_identities(),
                self.described_credentials(),
            ),
            RowKind::Discovered(_) => return,
            RowKind::Remote { address, .. } => Form::for_remote(
                Target::Remote {
                    original: address.clone(),
                },
                &address,
            ),
        });
    }

    pub(in crate::tui) fn toggle(&mut self, setting: Setting, on: bool) {
        let Some(file) = self.settings_file.as_mut() else {
            self.problem("Settings are unavailable; fix the file and reopen");
            return;
        };
        let mut proposed = file.settings().clone();
        match setting {
            Setting::KeyboardHints => proposed.show_keyboard_hints(on),
            Setting::Animations => proposed.animate_interface(on),
        }
        match file.save(proposed) {
            Ok(()) => self.alert = None,
            Err(error) => self.problem(error.to_string()),
        }
    }

    pub(in crate::tui) fn save_form(&mut self, form: &Form) -> bool {
        // Refused before the file is touched, so the reason is the form's rather
        // than a validation message about a name that was never given.
        if let Some(refusal) = form.refusal() {
            self.say(refusal.to_owned());
            return false;
        }
        // A project's machine is where its work is. Moving it while tasks are in
        // play would leave them running somewhere the project no longer points,
        // and their builds on a machine that is about to forget them.
        if let Some(id) = form.target.project_id() {
            let current = self
                .settings_file
                .as_ref()
                .and_then(|file| file.settings().project(id))
                .and_then(|project| project.host.clone());
            if current != form.host_value() && self.project_has_tasks(id) {
                self.say("That project has tasks in play; accept or delete them before moving it");
                return false;
            }
        }
        // Canonical before the file is borrowed, because a bad URL is the form's
        // answer to give rather than the file's.
        let repository = {
            let value = form.repository.value().trim();
            if value.is_empty() {
                None
            } else {
                match normalize_repository(value) {
                    Ok(repository) => Some(repository),
                    Err(error) => {
                        self.say(error.to_string());
                        return false;
                    }
                }
            }
        };
        let Some(file) = self.settings_file.as_mut() else {
            self.problem("Settings are unavailable; fix the file and reopen");
            return false;
        };
        let mut proposed = file.settings().clone();
        match &form.target {
            Target::NewProject | Target::Project { .. } => {
                let name = form.primary.value().trim().to_owned();
                // The page edits some fields; everything else on the project is
                // carried across so a rename is not a reset.
                let mut project = form
                    .target
                    .project_id()
                    .and_then(|id| proposed.project(id).cloned())
                    .unwrap_or_else(|| Project::new(name.clone()));
                project.name = name;
                project.repository = repository;
                project.host = form.host_value();
                project.agent = form.agent();
                project.agent_credential = form.agent_credential();
                project.vcs_credential = form.vcs_credential();
                project.vcs = form
                    .vcs_credential()
                    .as_deref()
                    .and_then(Service::from_id)
                    .and_then(|service| match service {
                        Service::VersionControl(kind) => Some(kind),
                        _ => None,
                    });
                project.commits = form.commits();
                if let Some(commits) = &project.commits {
                    proposed.remember_identity(commits);
                }
                project.setup_command = form.setup.value();
                project.caches = form.cache_paths();
                project.instructions = form.instruction_text();
                let scope = form.scope.value().trim();
                // A project that accepts the whole account is a binding too, and
                // says so; one that claims nothing is not. A project with no
                // tracker has nothing to bind, whatever the fields still hold.
                project.tracker = (form.has_tracker()
                    && (!scope.is_empty() || form.accepts_delegated_work))
                    .then(|| TrackerBinding {
                        credential: form.tracker_credential(),
                        scope: (!scope.is_empty()).then(|| scope.to_owned()),
                        accepts_delegated_work: form.accepts_delegated_work,
                    });
                proposed.add_project(project);
            }
            Target::NewRemote | Target::Remote { .. } => {
                if let Some(original) = form.target.original() {
                    proposed.remove_remote(original);
                }
                proposed.add_remote(form.primary.value());
            }
        }
        match file.save(proposed) {
            Ok(()) => {
                self.panel.form = None;
                self.alert = None;
                true
            }
            Err(error) => {
                self.problem(error.to_string());
                false
            }
        }
    }

    pub(in crate::tui) async fn delete(&mut self, target: Target) {
        // Stopping the watch is not the settings file's business, so it happens
        // before the file is borrowed.
        if let Target::Remote { original } = &target {
            if self
                .projects()
                .iter()
                .any(|project| project.host.as_deref() == Some(original.as_str()))
            {
                self.say("That remote runs a project; move the project to another machine first");
                return;
            }
            self.unwatch_remote(original);
        }
        // A project that a task in play belongs to stays: removing it would leave
        // that task with no tab to be found in, which is not a tidy-up.
        if let Target::Project { id } = &target {
            if self.projects().len() == 1 {
                self.say("A project is required; make another one first");
                return;
            }
            let in_use = self
                .tasks
                .iter()
                .any(|task| task.project().map(|project| &project.id == id) == Some(true))
                || self
                    .remote_tasks
                    .iter()
                    .any(|held| held.summary.project_id.as_ref() == Some(id));
            if in_use {
                self.say("That project still has tasks; accept or delete them first");
                return;
            }
        }
        let Some(file) = self.settings_file.as_mut() else {
            return;
        };
        let mut proposed = file.settings().clone();
        let mut forget_error = None;
        match &target {
            Target::Project { id } => proposed.remove_project(id),
            Target::Remote { original } => {
                proposed.remove_remote(original);
                self.remote_synced.remove(original);
                self.configuration_digests.remove(original);
                self.remote_status.remove(original);
                self.remote_config_inflight.remove(original);
                self.remote_trackers.remove(original);
                self.remote_tasks
                    .retain(|held| !held.address.eq_ignore_ascii_case(original));
                // A forgotten remote's key is forgotten with it: the next
                // sighting is a first sighting again, which is what forgetting
                // means.
                forget_error =
                    self.remote_keys.forget(original).await.err().map(|error| {
                        format!("could not forget the identity for {original}: {error}")
                    });
            }
            _ => {}
        }
        let removed_project = match &target {
            Target::Project { id } => Some(id.clone()),
            _ => None,
        };
        match file.save(proposed) {
            Ok(()) => {
                let index = self.panel.selected().saturating_sub(1);
                self.panel.select_row(index);
                self.alert = None;
                // The project is gone from the file, so its builds and caches are
                // state nothing will name again. Losing them costs a rebuild.
                if let Some(id) = removed_project
                    && let Err(error) = self.config.forget_project(&id)
                {
                    tracing::warn!(%error, project = %id, "could not remove a deleted project's state");
                }
            }
            Err(error) => self.problem(error.to_string()),
        }
        if let Some(message) = forget_error {
            self.problem(message);
        }
    }

    pub(in crate::tui) fn ask_to_delete(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        // The question is built first: it borrows the name the target then moves.
        let (question, target) = match row.kind {
            RowKind::GitIdentity(identity) => {
                let Some(commits) = identity.saved else {
                    return;
                };
                self.foreground = Foreground::Confirm(PendingConfirm::deleting_commit_identity(
                    commits.clone(),
                    format!(
                        "Delete the {} commit identity? Projects using it fall back to \
                             the installation's identity.",
                        if commits.label.is_empty() {
                            commits.name.clone()
                        } else {
                            commits.label.clone()
                        }
                    ),
                ));
                return;
            }
            RowKind::Switch { .. } | RowKind::Credential(_) | RowKind::Discovered(_) => {
                return;
            }
            RowKind::Project { project } => (
                format!("Delete the {} project?", project.name),
                Target::Project {
                    id: project.id.clone(),
                },
            ),
            RowKind::Remote { address, .. } => (
                format!(
                    "Delete {address}? This machine forgets it, and the identity it \\
                         answered with."
                ),
                Target::Remote { original: address },
            ),
        };
        self.foreground = Foreground::Confirm(PendingConfirm::deleting(target, question));
    }

    pub(in crate::tui) fn take_over<E>(
        &mut self,
        id: &str,
        terminal: &DefaultTerminal,
        event_loop: &TaskEventLoop<E>,
        landings: &mpsc::UnboundedSender<Result<String, String>>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        // Two takes at once pack the same task twice and race the sandbox each one
        // makes, so a move that is already running has to finish.
        if self.transfer.is_some() {
            self.say("A task is already on its way; let that finish first");
            return;
        }
        // A task on another machine has to travel before it can be taken over, and
        // travelling is a wait rather than a keystroke.
        if let Some(held) = self
            .remote_tasks
            .iter()
            .find(|held| held.summary.id == id)
            .cloned()
        {
            self.ask_for_task(&held, event_loop, landings, true);
            return;
        }
        let Some(task) = self.tasks.iter().find(|task| task.id == id).cloned() else {
            return;
        };
        if task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::Transfer) {
            self.say("This task is being moved");
            return;
        }
        if matches!(
            task.facts.execution_status,
            TaskExecutionStatus::Paused | TaskExecutionStatus::Failed
        ) {
            event_loop.start_operator_control(task);
            return;
        }
        if !matches!(task.facts.execution_status, TaskExecutionStatus::Running) {
            self.say("This task is not running");
            return;
        }
        if matches!(
            task.facts.execution_mode,
            TaskExecutionMode::Autopilot | TaskExecutionMode::AutopilotReport
        ) {
            if !event_loop.take_control(&task.id) {
                self.say("This task is not running, so there is nothing to take over");
            }
            return;
        }
        // Already the operator's: put its terminal back in front.
        let shell_id = ShellId::Task(task.id.clone());
        if !self.shells.contains_key(&shell_id) && !event_loop.has_active_run(&task.id) {
            self.say("This task has no run to attach to");
            return;
        }
        self.foreground = Foreground::Shell(shell_id.clone());
        self.alert = None;
        if self.shells.contains_key(&shell_id) {
            let Some((rows, cols)) = self.shell_size(terminal) else {
                return;
            };
            event_loop.resize_terminal(&task.id, rows, cols);
            let id = shell_id;
            if let Some(shell) = self.shells.get_mut(&id) {
                shell.resize(rows, cols);
            }
            self.foreground = Foreground::Shell(id);
            return;
        }
        let Some((rows, cols)) = self.shell_size(terminal) else {
            return;
        };
        let history = event_loop.cached_terminal_output(&task.id);
        event_loop.resize_terminal(&task.id, rows, cols);
        let id = ShellId::Task(task.id.clone());
        self.shells.insert(
            id.clone(),
            Shell::new(rows, cols, history, task_shell_title(&task)),
        );
        self.foreground = Foreground::Shell(id);
    }

    pub(in crate::tui) fn git_identity(&self) -> GitIdentity {
        self.git_identity.clone()
    }

    pub(in crate::tui) fn default_commits(&self) -> Option<ProjectCommits> {
        Some(ProjectCommits {
            label: self
                .git_identity
                .name
                .clone()
                .unwrap_or_else(|| "Git identity".to_owned()),
            name: self.git_identity.name.clone()?,
            email: self.git_identity.email.clone()?,
            signing_key: self
                .git_identity
                .signs_commits
                .then(|| self.git_identity.signing_key.clone())
                .flatten(),
        })
    }

    pub(in crate::tui) fn saved_identities(&self) -> Vec<ProjectCommits> {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().identities().to_vec())
            .unwrap_or_default()
    }

    pub(in crate::tui) async fn refresh_git_identity(&mut self) {
        let path = self.config.profile_dir("vcs/github").join("git/config");
        let output = HostCommand::new("git")
            .args(["config", "--no-includes", "--file"])
            .arg(path)
            .args([
                "--get-regexp",
                "^(user\\.(name|email|signingkey)|commit\\.gpgsign)$",
            ])
            .output()
            .await;
        let mut identity = GitIdentity::default();
        if let Ok(output) = output {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let Some((key, value)) = line.split_once(' ') else {
                    continue;
                };
                match key {
                    "user.name" => identity.name = Some(value.to_owned()),
                    "user.email" => identity.email = Some(value.to_owned()),
                    "user.signingkey" => identity.signing_key = Some(value.to_owned()),
                    "commit.gpgsign" => {
                        identity.signs_commits = matches!(value, "true" | "yes" | "on" | "1")
                    }
                    _ => {}
                }
            }
        }
        self.git_identity = identity;
    }
}
