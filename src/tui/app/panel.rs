use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::mpsc;

use crate::{isolation::IsolationEngine, task::TaskService};

use super::{
    App, CredentialShellEvent, Foreground, RemoteStatus, TrackerConnectReply, is_dismiss,
    is_remove, mem,
};
use crate::remote::{Compatibility, LOCAL_VERSION};
use crate::tui::popups::confirm::PendingConfirm;
use crate::tui::popups::setup::{Setup, SetupKind};
use crate::tui::screens;
use crate::tui::screens::settings::CredentialKind;
use crate::tui::screens::settings::{Depth, Form, Panel, RowKind, Section, Target};

impl App {
    pub(in crate::tui) async fn panel_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
        tracker_connects: &mpsc::UnboundedSender<TrackerConnectReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if self.panel.depth == Depth::Rail && key.code == KeyCode::Enter {
            self.panel.depth = Depth::List;
            return;
        }
        let previous_section = self.panel.section;
        match key.code {
            KeyCode::PageDown => {
                self.panel.detail_scroll = self.panel.detail_scroll.saturating_add(5)
            }
            KeyCode::PageUp => {
                self.panel.detail_scroll = self.panel.detail_scroll.saturating_sub(5)
            }
            KeyCode::Home => self.panel.detail_scroll = 0,
            _ if is_dismiss(key) || key.code == KeyCode::Char('s') => {
                // Return to what opened Settings. The first screen is a place of
                // its own until the operator starts work from it, so leaving
                // Settings from the card goes back to the card rather than to
                // the task list behind it.
                if self.panel_from_setup {
                    self.panel_from_setup = false;
                    self.open_get_started();
                } else if self.setup_incomplete() {
                    self.open_get_started();
                } else {
                    self.foreground = Foreground::Tasks;
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.panel.depth = match self.panel.depth {
                    Depth::Rail => Depth::List,
                    Depth::List => Depth::Rail,
                };
            }
            KeyCode::Right | KeyCode::Char('l') => self.panel.depth = Depth::List,
            KeyCode::Left | KeyCode::Char('h') => self.panel.depth = Depth::Rail,
            KeyCode::Down | KeyCode::Char('j') => self.panel.move_selection(true),
            KeyCode::Up | KeyCode::Char('k') => self.panel.move_selection(false),
            _ if is_remove(key)
                && self.panel.section.slot().is_some()
                && self
                    .selected_row()
                    .is_some_and(|row| matches!(row.kind, RowKind::Credential(_))) =>
            {
                self.ask_to_forget_credentials()
            }
            _ if is_remove(key) && self.selected_has_credentials() => {
                self.ask_to_forget_credentials()
            }
            KeyCode::Char(' ')
                if self.panel.section.slot().is_some()
                    && self
                        .selected_row()
                        .is_some_and(|row| matches!(row.kind, RowKind::Credential(_))) =>
            {
                self.choose_default_identity()
            }
            KeyCode::Char('r')
                if self.panel.section.slot().is_some()
                    && self
                        .selected_row()
                        .is_some_and(|row| matches!(row.kind, RowKind::Credential(_))) =>
            {
                self.open_rename_identity()
            }
            KeyCode::Char('r')
                if self
                    .selected_row()
                    .is_some_and(|row| matches!(row.kind, RowKind::GitIdentity(_))) =>
            {
                self.open_rename_commit_identity()
            }
            KeyCode::Char('e') if self.panel.section.grows() => self.edit_selected(),
            KeyCode::Char('t')
                if self
                    .selected_row()
                    .is_some_and(|row| row.is_configured_remote()) =>
            {
                if let Some(RowKind::Remote { address, .. }) =
                    self.selected_row().map(|row| row.kind)
                {
                    self.foreground = Foreground::Setup(Setup::tracker_list(address));
                }
            }
            KeyCode::Char('n') if self.panel.section.grows() => match self.panel.section {
                Section::Agents | Section::VersionControl | Section::Trackers => {
                    self.open_identity_setup(self.panel.section)
                }
                Section::Remotes => self.panel.form = Some(Form::for_remote(Target::NewRemote, "")),
                Section::Projects => {
                    self.panel.form = Some(Form::new_project(
                        &self.scope_label(),
                        &self.remotes(),
                        self.project_defaults(),
                        self.saved_identities(),
                        self.described_credentials(),
                    ))
                }
                Section::Interface => {}
            },
            _ if is_remove(key) && self.selected_row().is_some_and(|row| row.is_removable()) => {
                self.ask_to_delete()
            }
            KeyCode::Enter => {
                self.activate_selected(task_service, events, tracker_connects)
                    .await
            }
            _ => {}
        }
        if self.panel.section != previous_section {
            if self.panel.section == Section::VersionControl {
                self.refresh_git_identity().await;
            }
            self.credentials_visible_since = None;
        }
    }

    pub(in crate::tui) async fn activate_selected<E>(
        &mut self,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
        tracker_connects: &mpsc::UnboundedSender<TrackerConnectReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        match self.selected_row().map(|row| row.kind) {
            // A machine that answered with another identity is asking to be told
            // which machine it is: connecting again would get the same answer.
            Some(RowKind::Remote {
                address, status, ..
            }) => {
                if status == RemoteStatus::Changed {
                    self.foreground = Foreground::Confirm(PendingConfirm::replacing_key(
                        address.clone(),
                        format!(
                            "The machine at {} is not the one this client knew: it answered \
                             with a different identity, as one that was reinstalled or replaced \
                             would. Accept it?",
                            self.remote_name(&address)
                        ),
                    ));
                    return;
                }
                self.check_remote(&address).await
            }
            Some(RowKind::Discovered(remote)) => {
                if remote.compatibility == Compatibility::RequiresUpdate {
                    self.say(format!(
                        "{} runs altitude v{}, and this machine runs v{}. Update both to the \
                         same version to use it",
                        remote.label, remote.version, LOCAL_VERSION
                    ));
                    return;
                }
                if let Some(file) = self.settings_file.as_mut() {
                    let mut proposed = file.settings().clone();
                    proposed.add_remote(&remote.address);
                    match file.save(proposed) {
                        Ok(()) => {
                            self.remote_status.remove(&remote.address);
                            self.check_remote(&remote.address).await;
                        }
                        Err(error) => self.problem(error.to_string()),
                    }
                }
            }
            Some(RowKind::HostLogin(login)) => {
                self.foreground = Foreground::Confirm(PendingConfirm::importing_host_login(login));
            }
            Some(RowKind::GitIdentity(identity)) => match identity.saved {
                Some(commits) => self.open_saved_commit_identity(commits),
                None => self.open_git_identity(),
            },
            Some(RowKind::Credential(credential)) => match credential.kind {
                CredentialKind::Profile { profile, .. } => {
                    self.begin_sign_in(&credential.label, &profile, task_service, events);
                }
                CredentialKind::Tracker { profile, .. } => {
                    self.connect_tracker_for(&profile, task_service, tracker_connects);
                }
                CredentialKind::Installation { .. } => {
                    self.foreground = Foreground::Setup(Setup::new(
                        SetupKind::Remote,
                        screens::settings::remote_setup_lines(),
                    ));
                }
            },
            _ => self.edit_selected(),
        }
    }
}

pub(in crate::tui) async fn open_panel<E>(app: &mut App, task_service: &TaskService<E>)
where
    E: IsolationEngine + Clone + 'static,
{
    let behind = mem::replace(&mut app.foreground, Foreground::Panel);
    if !matches!(behind, Foreground::Panel) {
        app.panel_from_setup = matches!(behind, Foreground::GetStarted(_));
    }
    app.panel = Panel::new();
    if !app.reload_settings() {
        return;
    }
    app.base_description = task_service.base_description();
    app.reload_profiles(task_service);
    app.refresh_tracker_state(task_service);
    match app.remote_keys.list().await {
        Ok(keys) => app.remembered_keys = keys,
        Err(error) => app.problem(format!("could not read remembered remotes: {error}")),
    }
}

pub(in crate::tui) async fn open_panel_at<E>(
    app: &mut App,
    task_service: &TaskService<E>,
    section: Section,
) where
    E: IsolationEngine + Clone + 'static,
{
    open_panel(app, task_service).await;
    app.panel.section = section;
    app.panel.list.reset();
}
