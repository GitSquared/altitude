use crossterm::event::KeyEvent;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use crate::{
    credentials::{Credential, Service, Slot, new_id},
    isolation::IsolationEngine,
    project::ProjectCommits,
    task::TaskService,
    tui::screens::settings::Section,
};

use super::{
    App, CommitIdentityEdit, CredentialShellCancel, CredentialShellEvent, CredentialShellState,
    Foreground, GitIdentityDecision, GitIdentitySetup, IdentityChoice, IdentitySetup, Shell,
    ShellId, TrackerConnectReply, TrackerState, credential_shell_id, mem,
    popups::identity::SetupDecision,
    popups::identity::{RenameDecision, RenameIdentity},
    popups::sign_in::{Decision as SignInDecision, SignIn},
    screens::settings::RowKind,
};

impl App {
    pub(in crate::tui) fn open_identity_setup(&mut self, section: Section) {
        let mut choices: Vec<IdentityChoice> = section
            .services()
            .into_iter()
            .map(IdentityChoice::Service)
            .collect();
        if section == Section::VersionControl {
            choices.push(IdentityChoice::GitIdentity);
        }
        if !choices.is_empty() {
            self.foreground = Foreground::IdentitySetup(Box::new(IdentitySetup::new(choices)));
        }
    }

    pub(in crate::tui) async fn identity_setup_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
        tracker_connects: &mpsc::UnboundedSender<TrackerConnectReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Foreground::IdentitySetup(mut setup) = mem::take(&mut self.foreground) else {
            return;
        };
        match setup.key(key) {
            SetupDecision::Still => self.foreground = Foreground::IdentitySetup(setup),
            SetupDecision::Closed => self.foreground = Foreground::Panel,
            SetupDecision::Chosen(IdentityChoice::Service(service)) => {
                self.create_identity(service, task_service, events, tracker_connects)
            }
            SetupDecision::Chosen(IdentityChoice::GitIdentity) => {
                self.foreground = Foreground::Panel;
                self.open_new_commit_identity();
            }
        }
    }

    pub(in crate::tui) fn create_identity<E>(
        &mut self,
        service: Service,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
        tracker_connects: &mpsc::UnboundedSender<TrackerConnectReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let existing = self.settings_credentials();
        let count = existing
            .iter()
            .filter(|credential| credential.service() == Some(service))
            .count();
        let label = if count == 0 {
            service.label().to_owned()
        } else {
            format!("{} {}", service.label(), count + 1)
        };
        let id = new_id(service, &label, |candidate| {
            existing.iter().any(|credential| credential.id == candidate)
                || self.config.profile_dir(candidate).is_dir()
        });
        let credential = Credential::new(id.clone(), label.clone());
        let Some(file) = self.settings_file.as_mut() else {
            self.problem("Settings are unavailable; fix the file and reopen");
            return;
        };
        let mut proposed = file.settings().clone();
        if let Err(error) = proposed.add_credential(credential) {
            self.problem(error.to_string());
            return;
        }
        if let Err(error) = file.save(proposed) {
            self.problem(error.to_string());
            return;
        }
        self.panel.section = Section::for_service(service);
        self.foreground = Foreground::Panel;
        self.tracker_states
            .insert(id.clone(), TrackerState::NotConnected);
        if service.is_mounted()
            && let Some(summary) = task_service.credentials().summary(&id)
        {
            self.profiles.push(summary);
        }
        match service.slot() {
            Slot::Tracker => self.connect_tracker_for(&id, task_service, tracker_connects),
            _ => self.begin_sign_in(&label, &id, task_service, events),
        }
    }

    pub(in crate::tui) fn open_git_identity(&mut self) {
        let identity = self.git_identity();
        self.commit_edit = Some(CommitIdentityEdit::Installation);
        self.git_identity_over_card = false;
        self.foreground = Foreground::GitIdentity(Box::new(GitIdentitySetup::new(
            identity.name.as_deref().unwrap_or_default(),
            identity.email.as_deref().unwrap_or_default(),
            identity.signs_commits,
        )));
    }

    pub(in crate::tui) fn open_onboarding_identity(&mut self) {
        let identity = self.git_identity();
        self.commit_edit = Some(CommitIdentityEdit::Installation);
        self.git_identity_over_card = true;
        self.foreground = Foreground::GitIdentity(Box::new(GitIdentitySetup::new(
            identity.name.as_deref().unwrap_or_default(),
            identity.email.as_deref().unwrap_or_default(),
            identity.signs_commits,
        )));
    }

    pub(in crate::tui) fn open_new_commit_identity(&mut self) {
        self.commit_edit = Some(CommitIdentityEdit::New);
        self.git_identity_over_card = false;
        self.foreground = Foreground::GitIdentity(Box::new(GitIdentitySetup::new("", "", false)));
    }

    pub(in crate::tui) fn open_saved_commit_identity(&mut self, commits: ProjectCommits) {
        self.commit_edit = Some(CommitIdentityEdit::Saved(commits.clone()));
        self.git_identity_over_card = false;
        self.foreground = Foreground::GitIdentity(Box::new(GitIdentitySetup::new(
            &commits.name,
            &commits.email,
            commits.signing_key.is_some(),
        )));
    }

    pub(in crate::tui) fn close_git_identity(&mut self) {
        if mem::take(&mut self.git_identity_over_card) {
            self.open_get_started();
        } else {
            self.foreground = Foreground::Panel;
        }
    }

    pub(in crate::tui) fn git_identity_over_card(&self) -> bool {
        self.git_identity_over_card
    }

    pub(in crate::tui) async fn git_identity_key(&mut self, key: KeyEvent) {
        let Foreground::GitIdentity(mut identity) = mem::take(&mut self.foreground) else {
            return;
        };
        match identity.key(key) {
            GitIdentityDecision::Still => {
                self.foreground = Foreground::GitIdentity(identity);
            }
            GitIdentityDecision::Closed => {
                self.commit_edit = None;
                self.close_git_identity();
            }
            GitIdentityDecision::Save { name, email, signs } => {
                if name.is_empty() || email.is_empty() {
                    self.say("A commit needs a name and an email");
                    self.foreground = Foreground::GitIdentity(Box::new(GitIdentitySetup::new(
                        &name, &email, signs,
                    )));
                    return;
                }
                let edit = self.commit_edit.clone();
                let was_installation = matches!(edit, Some(CommitIdentityEdit::Installation));
                let label = match &edit {
                    Some(CommitIdentityEdit::Saved(commits)) if !commits.label.is_empty() => {
                        commits.label.clone()
                    }
                    _ => name.clone(),
                };
                if self
                    .save_commit_identity(&label, &name, &email, signs, was_installation)
                    .await
                {
                    self.commit_edit = None;
                    self.close_git_identity();
                    self.say("Git identity saved");
                } else {
                    self.foreground = Foreground::GitIdentity(Box::new(GitIdentitySetup::new(
                        &name, &email, signs,
                    )));
                }
            }
        }
    }

    pub(in crate::tui) fn open_rename_identity(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let RowKind::Credential(credential) = row.kind else {
            self.say("Only a named identity can be renamed");
            return;
        };
        let Some(id) = credential.profile().map(str::to_owned) else {
            return;
        };
        // What is being renamed is the name, not the tag the row prints: the
        // prompt starts from the name the operator gave, or from the account's.
        let name = self
            .settings_file
            .as_ref()
            .and_then(|file| file.settings().credential(&id))
            .map(|credential| credential.label.clone())
            .unwrap_or_default();
        self.foreground = Foreground::RenameIdentity(Box::new(RenameIdentity::new(id, &name)));
    }

    pub(in crate::tui) fn open_rename_commit_identity(&mut self) {
        let Some(RowKind::GitIdentity(identity)) = self.selected_row().map(|row| row.kind) else {
            return;
        };
        let commits = identity.saved.unwrap_or_else(|| ProjectCommits {
            label: identity.label.clone(),
            name: identity.name.clone().unwrap_or_default(),
            email: identity.email.clone().unwrap_or_default(),
            signing_key: identity.signing_key.clone(),
        });
        if commits.name.is_empty() || commits.email.is_empty() {
            self.say("Set a commit name and email before renaming this identity");
            return;
        }
        let label = if commits.label.is_empty() {
            commits.name.clone()
        } else {
            commits.label.clone()
        };
        self.foreground =
            Foreground::RenameIdentity(Box::new(RenameIdentity::for_commit(commits, &label)));
    }

    pub(in crate::tui) fn rename_identity_key(&mut self, key: KeyEvent) {
        let Foreground::RenameIdentity(mut rename) = mem::take(&mut self.foreground) else {
            return;
        };
        match rename.key(key) {
            RenameDecision::Still => self.foreground = Foreground::RenameIdentity(rename),
            RenameDecision::Closed => self.foreground = Foreground::Panel,
            RenameDecision::Chosen(label) => {
                let Some(file) = self.settings_file.as_mut() else {
                    self.problem("Settings are unavailable; fix the file and reopen");
                    return;
                };
                let mut proposed = file.settings().clone();
                let result = if let Some(commits) = rename.commits.clone() {
                    let mut renamed = commits.clone();
                    renamed.label = label;
                    proposed.replace_identity(&commits, renamed);
                    Ok(())
                } else {
                    proposed.rename_credential(&rename.id, &label)
                };
                match result {
                    Ok(()) => match file.save(proposed) {
                        Ok(()) => self.foreground = Foreground::Panel,
                        Err(error) => self.problem(error.to_string()),
                    },
                    Err(error) => self.problem(error.to_string()),
                }
            }
        }
    }

    pub(in crate::tui) fn begin_sign_in<E>(
        &mut self,
        display: &str,
        profile: &str,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        // A sign-in from the first screen names the built-in profile directly;
        // that is an identity being created, so record it before the shell opens.
        if self.ensure_credential(profile, display)
            && Service::from_id(profile).is_some_and(|service| service.is_mounted())
            && let Some(summary) = task_service.credentials().summary(profile)
            && !self
                .profiles
                .iter()
                .any(|known| known.profile == summary.profile)
        {
            self.profiles.push(summary);
        }
        match self
            .profiles
            .iter()
            .find(|summary| summary.profile == profile)
        {
            Some(summary) if summary.sign_in_command.is_interactive() => {
                self.foreground = Foreground::SignIn(Box::new(SignIn::new(
                    summary.display,
                    profile.to_owned(),
                    summary.sign_in,
                )));
                self.alert = None;
            }
            _ => self.open_credential_shell(display, profile.to_owned(), task_service, events),
        }
    }

    pub(in crate::tui) async fn sign_in_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Foreground::SignIn(sign_in) = mem::take(&mut self.foreground) else {
            return;
        };
        match sign_in.handle_key(key) {
            None => self.foreground = Foreground::SignIn(sign_in),
            Some(SignInDecision::Cancel) => self.foreground = Foreground::Panel,
            Some(SignInDecision::Open) => {
                self.foreground = Foreground::Panel;
                self.open_credential_shell(&sign_in.display, sign_in.profile, task_service, events);
            }
        }
    }

    pub(in crate::tui) fn open_credential_shell<E>(
        &mut self,
        display: &str,
        profile: String,
        task_service: &TaskService<E>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if !matches!(self.credential_shell, CredentialShellState::Idle) {
            self.say("A shell is already open");
            return;
        }
        self.signing_in = Some(profile.clone());
        if self.ensure_credential(&profile, display)
            && let Some(summary) = task_service.credentials().summary(&profile)
            && !self
                .profiles
                .iter()
                .any(|known| known.profile == summary.profile)
        {
            self.profiles.push(summary);
        }
        let cancel = CredentialShellCancel::new();
        let id = credential_shell_id();
        // A built environment is a wait of a moment, so it gets no wait to look at:
        // the box appears only when there is an install to watch.
        let progress = (!task_service.credentials().build_ready(&profile))
            .then(|| "Preparing your sign-in…".to_owned());
        self.credential_shell = CredentialShellState::Opening {
            id: id.clone(),
            display: display.to_owned(),
            progress,
            cancel: cancel.clone(),
        };
        self.alert = None;
        let service = task_service.clone();
        let events = events.clone();
        tokio::spawn(async move {
            match service
                .credentials()
                .open_shell(id.clone(), &profile, cancel, &events)
                .await
            {
                Ok(()) => {}
                Err(error) => {
                    let _ = events.send(CredentialShellEvent::Unavailable {
                        id,
                        reason: error.to_string(),
                    });
                }
            }
        });
    }

    pub(in crate::tui) async fn credential_shell_event<E>(
        &mut self,
        event: CredentialShellEvent,
        terminal: &DefaultTerminal,
        task_service: &TaskService<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let expected = match &self.credential_shell {
            CredentialShellState::Opening { id, .. } => Some(id.as_str()),
            CredentialShellState::Open(shell) => Some(shell.id()),
            CredentialShellState::Idle => None,
        };
        if expected != Some(event.id()) {
            if let CredentialShellEvent::Opened(shell) = event {
                let service = task_service.clone();
                tokio::spawn(async move {
                    let _ = service.credentials().close_shell(shell.id()).await;
                });
            }
            return;
        }
        match event {
            CredentialShellEvent::Preparing { label, .. } => {
                if let CredentialShellState::Opening { progress, .. } = &mut self.credential_shell {
                    *progress = Some(label);
                }
            }
            CredentialShellEvent::Opened(shell) => {
                let CredentialShellState::Opening { display, .. } =
                    mem::replace(&mut self.credential_shell, CredentialShellState::Idle)
                else {
                    // Nothing is waiting for it any more, so it has no reason to run.
                    if let Err(error) = task_service.credentials().close_shell(shell.id()).await {
                        tracing::warn!(%error, "could not remove an unwanted credential shell");
                    }
                    return;
                };
                let id = ShellId::Credentials(shell.id().to_owned());
                // What was in front stays, so closing the terminal returns to it
                // rather than to whatever the shell happened to cover.
                self.shell_behind.cover(mem::replace(
                    &mut self.foreground,
                    Foreground::Shell(id.clone()),
                ));
                self.alert = None;
                let Some((rows, cols)) = self.shell_size(terminal) else {
                    let service = task_service.clone();
                    tokio::spawn(async move {
                        let _ = service.credentials().close_shell(shell.id()).await;
                    });
                    self.leave_credentials();
                    return;
                };
                self.shells.insert(
                    id.clone(),
                    Shell::new(rows, cols, Vec::new(), format!("credentials — {display}")),
                );
                shell.resize(rows, cols);
                self.credential_shell = CredentialShellState::Open(shell);
                self.foreground = Foreground::Shell(id);
                self.alert = None;
            }
            CredentialShellEvent::Output { id, bytes } => {
                let replies = self.forward_shell_output(&ShellId::Credentials(id), bytes);
                for reply in replies {
                    if let CredentialShellState::Open(shell) = &self.credential_shell {
                        shell.input(reply);
                    }
                }
            }
            CredentialShellEvent::Exited { code, .. } => {
                // Leaving the terminal is not an error: a cancelled sign-in and a
                // finished one look the same from here, and the row's next check says
                // what came of it.
                tracing::debug!(code, "a credential shell exited");
                self.close_credential_shell(task_service).await;
            }
            CredentialShellEvent::Failed { error, .. } => {
                self.problem(format!("The shell ended: {error}"));
                self.close_credential_shell(task_service).await;
            }
            CredentialShellEvent::Unavailable { reason, .. } => {
                self.credential_shell = CredentialShellState::Idle;
                // A sign-in that never started is not an answer about the login
                // that was there before: ask again rather than leave the row
                // saying what it said before the attempt.
                self.recheck_signed_in_profile();
                self.problem(format!("No shell: {reason}"));
            }
        }
    }

    pub(in crate::tui) fn recheck_signed_in_profile(&mut self) {
        let Some(profile) = self.signing_in.take() else {
            return;
        };
        self.credential_status.remove(&profile);
        self.credential_checked_at.remove(&profile);
        self.credential_checking.remove(&profile);
    }

    pub(in crate::tui) async fn close_credential_shell<E>(&mut self, task_service: &TaskService<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        let state = mem::take(&mut self.credential_shell);
        let CredentialShellState::Open(shell) = state else {
            if let CredentialShellState::Opening { cancel, .. } = state {
                cancel.cancel();
            }
            self.credential_shell = CredentialShellState::Idle;
            // The same answer as closing an open shell: cancelling before the
            // terminal appears still means the row has to be checked again, or
            // it goes on reporting the login the attempt was meant to replace.
            self.recheck_signed_in_profile();
            return;
        };
        let id = ShellId::Credentials(shell.id().to_owned());
        self.shells.remove(&id);
        if self.open_shell() == Some(&id) {
            self.recheck_signed_in_profile();
            // Back to the screen that asked, with the row that asked still on it.
            self.foreground = self.shell_behind.uncover().unwrap_or(Foreground::Panel);
        }
        let service = task_service.clone();
        tokio::spawn(async move {
            if let Err(error) = service.credentials().close_shell(shell.id()).await {
                tracing::warn!(%error, "could not remove a credential shell's sandbox");
            }
        });
        self.credentials_visible_since = None;
        self.refresh_git_identity().await;
    }
}
