use std::mem;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use crate::{
    agent::{AGENTS, AgentKind},
    credentials::{CredentialShellEvent, Slot},
    isolation::IsolationEngine,
    project::Project,
    task::{TaskExecutionStatus, TaskService},
    tui::keys::is_dismiss,
    tui::popups,
    tui::screens,
    vcs::VcsProviderKind,
};

use popups::new_task::NewTask;
use popups::repositories::{Key as RepositoriesKey, Repositories};
use popups::scopes::Scopes;
use screens::get_started::{Focus as SetupFocus, GetStarted, ready as setup_ready, signed_in};
use screens::settings::Section;

use super::*;

impl App {
    pub(super) fn begin_scope_listing<E>(
        &mut self,
        task_service: &TaskService<E>,
        replies: &mpsc::UnboundedSender<TrackerScopesReply>,
        credential: Option<String>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(credential) = credential.or_else(|| self.tracker_credential()) else {
            self.say("Choose a task tracker identity before choosing where its issues come from");
            return;
        };
        let Some(tracker) = task_service.tracker_for(&credential) else {
            self.say("Connect that task tracker identity before choosing a team");
            return;
        };
        if let Some(form) = self.panel.form.as_mut() {
            form.scopes = Some(Scopes::new());
        }
        let replies = replies.clone();
        tokio::spawn(async move {
            let result = tracker.scopes().await.map_err(|error| error.to_string());
            let _ = replies.send(TrackerScopesReply { result });
        });
    }

    pub(in crate::tui) fn begin_repository_listing<E>(
        &mut self,
        task_service: &TaskService<E>,
        replies: &mpsc::UnboundedSender<RepositoryListReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        // The project says which login its repositories come from, the way it
        // says which agent drives it. Until a machine can hold more than one,
        // every project answers with the same one.
        let kind = {
            let Some(form) = self.panel.form.as_ref() else {
                return;
            };
            form.target
                .project_id()
                .and_then(|id| self.settings_file.as_ref()?.settings().project(id))
                .map(Project::vcs_provider)
                .unwrap_or_else(VcsProviderKind::sole)
        };
        let cached = self.repositories.clone().unwrap_or_default();
        let Some(form) = self.panel.form.as_mut() else {
            return;
        };
        let current = form.repository.value();
        form.repositories = Some(Repositories::new(
            &cached,
            (!current.is_empty()).then_some(current),
        ));
        self.spawn_repository_list(kind, task_service, replies);
    }

    pub(super) fn spawn_repository_list<E>(
        &mut self,
        kind: VcsProviderKind,
        task_service: &TaskService<E>,
        replies: &mpsc::UnboundedSender<RepositoryListReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        self.repositories_listing = true;
        self.repositories_requested = true;
        let service = task_service.clone();
        let replies = replies.clone();
        tokio::spawn(async move {
            let result = service
                .repositories(kind)
                .await
                .map_err(|error| error.to_string());
            let _ = replies.send(RepositoryListReply { result });
        });
    }

    pub(in crate::tui) fn prefetch_setup_repositories<E>(
        &mut self,
        task_service: &TaskService<E>,
        replies: &mpsc::UnboundedSender<RepositoryListReply>,
    ) -> bool
    where
        E: IsolationEngine + Clone + 'static,
    {
        if !self.wants_setup_repositories() {
            return false;
        }
        self.spawn_repository_list(self.active_vcs(), task_service, replies);
        true
    }

    pub(super) fn wants_setup_repositories(&self) -> bool {
        matches!(self.foreground, Foreground::GetStarted(_))
            && self.active_repository().is_none()
            && !self.repositories_requested
            && !self.repositories_listing
            && signed_in(
                &self
                    .credential_status
                    .get(self.active_vcs().profile())
                    .cloned()
                    .unwrap_or_default(),
            )
    }

    pub(in crate::tui) fn repositories_listed(&mut self, reply: RepositoryListReply) {
        self.repositories_listing = false;
        match reply.result {
            Ok(repositories) => {
                if let Some(form) = self.panel.form.as_mut()
                    && let Some(browser) = form.repositories.as_mut()
                {
                    browser.fill(&repositories);
                } else if let Foreground::GetStarted(panel) = &mut self.foreground
                    && let Some(browser) = panel.repositories.as_mut()
                {
                    browser.fill(&repositories);
                }
                self.repositories = Some(repositories);
            }
            Err(error) => {
                if let Some(form) = self.panel.form.as_mut()
                    && let Some(browser) = form.repositories.as_mut()
                {
                    browser.failed(error);
                } else if let Foreground::GetStarted(panel) = &mut self.foreground
                    && let Some(browser) = panel.repositories.as_mut()
                {
                    browser.failed(error);
                }
            }
        }
    }

    pub(super) fn choose_active_agent(&mut self, agent: AgentKind) {
        let active = self.active_project_id();
        let pinned = self
            .projects()
            .into_iter()
            .any(|project| project.id == active && project.agent.is_some());
        if !pinned {
            self.choose_agent(agent);
            return;
        }
        let Some(file) = self.settings_file.as_mut() else {
            return;
        };
        let mut proposed = file.settings().clone();
        let Some(mut project) = proposed.project(&active).cloned() else {
            return;
        };
        project.agent = Some(agent);
        proposed.add_project(project);
        let saved = file.save(proposed);
        if let Err(error) = saved {
            self.problem(error.to_string());
        }
    }

    pub(in crate::tui) fn active_agent(&self) -> Option<AgentKind> {
        let active = self.active_project_id();
        self.projects()
            .into_iter()
            .find(|project| project.id == active)
            .and_then(|project| project.agent)
            .or_else(|| self.agent())
    }

    pub(super) fn active_project_name(&self) -> String {
        let active = self.active_project_id();
        self.projects()
            .into_iter()
            .find(|project| project.id == active)
            .map(|project| project.name)
            .unwrap_or_default()
    }

    pub(super) fn rename_project(&mut self, id: &str, name: &str) -> bool {
        let Some(file) = self.settings_file.as_mut() else {
            return false;
        };
        let mut proposed = file.settings().clone();
        let Some(mut project) = proposed.project(id).cloned() else {
            return false;
        };
        project.name = name.to_owned();
        proposed.add_project(project);
        let saved = file.save(proposed);
        match saved {
            Ok(()) => true,
            Err(error) => {
                self.problem(error.to_string());
                false
            }
        }
    }

    pub(super) fn open_setup_repositories<E>(
        &mut self,
        panel: &mut GetStarted,
        task_service: &TaskService<E>,
        replies: &mpsc::UnboundedSender<RepositoryListReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let cached = self.repositories.clone().unwrap_or_default();
        let current = self.active_repository();
        panel.repositories = Some(Repositories::new(&cached, current.as_deref()));
        self.spawn_repository_list(self.active_vcs(), task_service, replies);
    }

    pub(super) fn bind_project_remote(&mut self, address: &str) {
        let active = self.active_project_id();
        let Some(file) = self.settings_file.as_mut() else {
            return;
        };
        let mut proposed = file.settings().clone();
        let Some(mut project) = proposed.project(&active).cloned() else {
            return;
        };
        project.host = Some(address.to_owned());
        proposed.add_project(project);
        let saved = file.save(proposed);
        if let Err(error) = saved {
            self.problem(error.to_string());
        }
    }

    pub(super) fn active_vcs(&self) -> VcsProviderKind {
        let active = self.active_project_id();
        self.projects()
            .into_iter()
            .find(|project| project.id == active)
            .map(|project| project.vcs_provider())
            .unwrap_or_else(VcsProviderKind::sole)
    }

    pub(super) fn active_repository(&self) -> Option<String> {
        let active = self.active_project_id();
        self.projects()
            .into_iter()
            .find(|project| project.id == active)
            .and_then(|project| project.repository)
    }

    pub(super) fn bind_project_repository(&mut self, url: &str) {
        let active = self.active_project_id();
        let Some(file) = self.settings_file.as_mut() else {
            return;
        };
        let mut proposed = file.settings().clone();
        let Some(mut project) = proposed.project(&active).cloned() else {
            return;
        };
        project.repository = Some(url.to_owned());
        proposed.add_project(project);
        let saved = file.save(proposed);
        if let Err(error) = saved {
            self.problem(error.to_string());
        }
    }

    pub(super) fn setup_agent(&self) -> Option<AgentKind> {
        self.active_agent()
            .filter(|agent| !self.signed_out(agent.profile()))
    }

    // A task that has ended is a record of work this machine finished, and its
    // project is the one it was started with, so it does not follow the tab. Only
    // a task that could still put a build on a machine holds a project where it is.
    pub(super) fn project_has_tasks_in_play(&self, id: &str) -> bool {
        let is_in_play = |status: &TaskExecutionStatus| {
            matches!(
                status,
                TaskExecutionStatus::Pending
                    | TaskExecutionStatus::Running
                    | TaskExecutionStatus::Paused
            )
        };
        self.tasks
            .iter()
            .filter(|task| belongs_to(task.project().map(|own| own.id.as_str()), id))
            .any(|task| is_in_play(&task.facts.execution_status))
            || self
                .remote_tasks
                .iter()
                .filter(|held| belongs_to(held.summary.project_id.as_deref(), id))
                .any(|held| is_in_play(&held.summary.status))
    }

    pub(in crate::tui) fn setup_incomplete(&self) -> bool {
        self.setup_next_step().is_some()
    }

    pub(in crate::tui) fn setup_next_step(&self) -> Option<&'static str> {
        if self.active_project_name().trim().is_empty() {
            Some("name the project")
        } else if self.setup_agent().is_none() {
            Some("choose an agent")
        } else if self.active_repository().is_none() {
            Some("choose a repository")
        } else if !self.git_identity.is_set() {
            Some("set the commit identity")
        } else if self.signed_out_vcs() {
            Some("sign in to version control")
        } else {
            None
        }
    }

    pub(in crate::tui) fn open_get_started(&mut self) {
        self.foreground = Foreground::GetStarted(self.started_panel());
    }

    pub(in crate::tui) fn started_panel(&self) -> Box<GetStarted> {
        let repositories = self.repositories.clone().unwrap_or_default();
        Box::new(GetStarted::new(
            &self.active_project_name(),
            self.setup_agent(),
            self.active_repository().as_deref(),
            &repositories,
            &self.git_identity,
            &AGENTS,
        ))
    }

    pub(in crate::tui) async fn started_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
        repository_lists: &mpsc::UnboundedSender<RepositoryListReply>,
        tracker_connects: &mpsc::UnboundedSender<TrackerConnectReply>,
    ) -> bool
    where
        E: IsolationEngine + Clone + 'static,
    {
        let Foreground::GetStarted(mut panel) = mem::take(&mut self.foreground) else {
            return false;
        };
        // A shell being prepared is a wait; leaving it is the only key it has.
        if matches!(self.credential_shell, CredentialShellState::Opening { .. }) {
            if is_dismiss(key) {
                self.close_credential_shell(task_service).await;
            }
            self.foreground = Foreground::GetStarted(panel);
            return false;
        }
        if panel.commits.open {
            let left = self.commits_key(&mut panel, key).await;
            if !left {
                self.foreground = Foreground::GetStarted(panel);
            }
            return false;
        }
        if let Some(browser) = panel.repositories.as_mut() {
            match browser.key(key) {
                RepositoriesKey::Still => {}
                RepositoriesKey::Closed => panel.repositories = None,
                RepositoriesKey::Save => panel.repositories = None,
                RepositoriesKey::Chosen(repository) => {
                    panel.repositories = None;
                    self.bind_project_repository(&repository.url);
                    panel.choose_repository(&repository);
                    panel.focus = SetupFocus::Ready;
                }
            }
            self.foreground = Foreground::GetStarted(panel);
            return false;
        }
        if panel.agent.is_open() {
            let before = panel.agent.chosen_value().copied();
            panel.agent.handle_key(key);
            if !panel.agent.is_open()
                && let Some(chosen) = panel.agent.chosen_value().copied()
                && before != Some(chosen)
            {
                self.choose_active_agent(chosen);
                self.foreground = Foreground::GetStarted(panel);
                // The picker comes first; the sign-in it needs comes after, over
                // the card rather than over anything else.
                let needs_sign_in = !self
                    .credential_status
                    .get(chosen.profile())
                    .is_some_and(signed_in);
                if needs_sign_in {
                    self.open_credential_shell(
                        &chosen.to_string(),
                        chosen.profile().to_owned(),
                        task_service,
                        events,
                    );
                }
                return false;
            }
            self.foreground = Foreground::GetStarted(panel);
            return false;
        }
        if panel.vcs.is_open() {
            // Enter is what closes the list by taking its offer; escape and q
            // close it empty. A list of one is still a list: the choice is the
            // key press, not a different value from the one already shown.
            panel.vcs.handle_key(key);
            if !panel.vcs.is_open()
                && key.code == KeyCode::Enter
                && let Some(chosen) = panel.vcs.chosen_value().copied()
            {
                // Choosing a provider is choosing the sign-in that makes it
                // usable: the repository list comes after it, never before.
                self.foreground = Foreground::GetStarted(panel);
                self.open_credential_shell(
                    chosen.label(),
                    chosen.profile().to_owned(),
                    task_service,
                    events,
                );
                return false;
            }
            self.foreground = Foreground::GetStarted(panel);
            return false;
        }
        if panel.tracker.is_open() {
            panel.tracker.handle_key(key);
            if !panel.tracker.is_open()
                && key.code == KeyCode::Enter
                && panel.tracker.chosen_value().is_some()
            {
                // The only provider this build knows, so choosing it is the
                // sign-in. A second enter while one is out is ignored rather
                // than cancelling: the card does not show a wait, and a key that
                // silently reverses what the browser is doing is a trap.
                if self.tracker_state != TrackerState::Connecting
                    && self.tracker_state != TrackerState::Connected
                {
                    self.connect_tracker(panel.tracker_kind(), task_service, tracker_connects);
                }
            }
            self.foreground = Foreground::GetStarted(panel);
            return false;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        // Typing is a state the name is put into, so j and k keep moving the
        // caret between rows until enter says otherwise.
        if panel.name_editing {
            match key.code {
                KeyCode::Enter => self.finish_name(&mut panel),
                KeyCode::Tab | KeyCode::Down => {
                    self.finish_name(&mut panel);
                    panel.move_focus(true);
                }
                KeyCode::BackTab | KeyCode::Up => {
                    self.finish_name(&mut panel);
                    panel.move_focus(false);
                }
                // Escape leaves too, and leaving is saving: a field that throws
                // away what was typed is a field nobody trusts to hold it.
                KeyCode::Esc => self.finish_name(&mut panel),
                _ => panel.name.handle_key(key),
            }
            self.foreground = Foreground::GetStarted(panel);
            return false;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return true,
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => panel.move_focus(true),
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => panel.move_focus(false),
            KeyCode::Char(' ') if panel.focus == SetupFocus::Agent => panel.agent.open(),
            KeyCode::Enter => match panel.focus {
                SetupFocus::Name => panel.name_editing = true,
                SetupFocus::Agent => match self.setup_agent() {
                    Some(kind) => {
                        // The card waits behind the shell, and comes back when it
                        // closes: this is the one sign-in that does not start at
                        // Settings.
                        self.foreground = Foreground::GetStarted(panel);
                        self.open_credential_shell(
                            &kind.to_string(),
                            kind.profile().to_owned(),
                            task_service,
                            events,
                        );
                        return false;
                    }
                    None => {
                        if self.open_import_logins(Slot::Agent) {
                            return false;
                        }
                        panel.agent.open();
                    }
                },
                SetupFocus::VersionControl => {
                    if self
                        .credential_status
                        .get(self.active_vcs().profile())
                        .is_some_and(signed_in)
                    {
                        self.open_setup_repositories(&mut panel, task_service, repository_lists);
                        // The card stays behind the list it just opened: taking the
                        // foreground leaves the default (the task list) in front
                        // unless it is put back.
                        self.foreground = Foreground::GetStarted(panel);
                        return false;
                    }
                    // The provider list first, even with one provider in it: it
                    // is where a second one will appear, and the row already
                    // says which one is signed in.
                    if self.open_import_logins(Slot::VersionControl) {
                        return false;
                    }
                    panel.vcs.open();
                }
                SetupFocus::Commits => {
                    self.open_onboarding_identity();
                    return false;
                }
                SetupFocus::Tracker => {
                    if self.tracker_state == TrackerState::Connected {
                        self.foreground = Foreground::GetStarted(panel);
                        open_panel_at(self, task_service, Section::Trackers).await;
                        return false;
                    }
                    if self.tracker_state != TrackerState::Connecting {
                        if self.open_import_logins(Slot::Tracker) {
                            return false;
                        }
                        panel.tracker.open();
                    }
                }
                SetupFocus::Remote => match self.setup().offer {
                    Some(offer) if self.setup().host.is_none() => {
                        if self.adopt_remote(&offer) {
                            self.bind_project_remote(&offer.address);
                            let address = offer.address.clone();
                            self.check_remote(&address).await;
                            self.watch_remote(&address);
                        }
                    }
                    _ => {
                        self.foreground = Foreground::GetStarted(panel);
                        open_panel_at(self, task_service, Section::Remotes).await;
                        return false;
                    }
                },
                SetupFocus::Ready => {
                    self.commit_name(&panel);
                    let setup = self.setup();
                    if !setup_ready(&setup, &panel.name) {
                        self.say(self.setup_next_step().unwrap_or("Finish setup first"));
                    } else {
                        self.open_new_task(false);
                        return false;
                    }
                }
                SetupFocus::Settings => {
                    self.foreground = Foreground::GetStarted(panel);
                    open_panel(self, task_service).await;
                    return false;
                }
            },
            _ => {}
        }
        self.foreground = Foreground::GetStarted(panel);
        false
    }

    pub(in crate::tui) fn open_new_task(&mut self, linking: bool) {
        if !self.reload_settings() {
            return;
        }
        let question = NewTask::new(
            self.default_tracker_kind(),
            self.projects(),
            &self.active_project_id(),
            self.task_hosts(),
        );
        self.foreground = Foreground::NewTask(Box::new(if linking {
            question.linking()
        } else {
            question
        }));
    }
}
