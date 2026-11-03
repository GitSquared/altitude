use std::time::Instant;

use tokio::sync::mpsc;

use crate::{
    credentials::{Credential, Service, Slot, describe},
    isolation::IsolationEngine,
    remote::Announcement,
    task::TaskService,
    tui::popups,
    tui::screens,
    vcs::VcsProviderKind,
};

use popups::confirm::PendingConfirm;
use screens::settings::{Depth, RowKind};

use super::*;

impl App {
    pub(super) fn unasked_profiles(&self) -> Vec<String> {
        let Some(slot) = self.panel.section.slot() else {
            return Vec::new();
        };
        let retry_due = self
            .credential_retry_at
            .is_none_or(|at| Instant::now() >= at);
        self.profiles
            .iter()
            .map(|summary| summary.profile.clone())
            .filter(|profile| {
                Service::from_id(profile).is_some_and(|service| service.slot() == slot)
            })
            .filter(|profile| {
                if self.credential_checking.contains(profile) {
                    return false;
                }
                match self.credential_status.get(profile) {
                    None => true,
                    Some(CredentialStatus::Unanswered) => retry_due,
                    Some(CredentialStatus::Checking) => false,
                    Some(_) => self
                        .credential_checked_at
                        .get(profile)
                        .is_none_or(|at| at.elapsed() >= CHECK_FRESH_FOR),
                }
            })
            .collect()
    }

    pub(super) fn ask_profiles<E>(
        &mut self,
        profiles: Vec<String>,
        task_service: &TaskService<E>,
        reports: &mpsc::UnboundedSender<CredentialCheckReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if profiles.is_empty() {
            return;
        }
        for profile in &profiles {
            self.credential_checking.insert(profile.clone());
            // A stale answer stays on screen while its replacement is out; only a
            // profile with nothing to show says it is checking.
            if self
                .credential_status
                .get(profile)
                .is_none_or(|status| matches!(status, CredentialStatus::Unanswered))
            {
                self.credential_status
                    .insert(profile.clone(), CredentialStatus::Checking);
            }
        }
        self.credential_retry_at = None;
        let service = task_service.clone();
        let reports = reports.clone();
        let generation = self.credential_check_generation;
        tokio::spawn(async move {
            let answers = match service.credentials().answers(profiles.clone()).await {
                Ok(answers) => answers
                    .into_iter()
                    .map(|(profile, answer)| (profile, credential_status(answer)))
                    .collect(),
                Err(error) => {
                    tracing::debug!(%error, "credential checks unavailable");
                    profiles
                        .into_iter()
                        .map(|profile| (profile, CredentialStatus::Unanswered))
                        .collect()
                }
            };
            let _ = reports.send(CredentialCheckReply {
                generation,
                answers,
            });
        });
    }

    pub(super) async fn remember(&mut self, address: &str, said: &Announcement) {
        if let Err(error) = self
            .remote_keys
            .remember(address, &said.id, &said.label, &said.version)
            .await
        {
            tracing::error!(%error, address, "could not remember a remote");
        }
    }

    pub(super) fn selected_has_credentials(&self) -> bool {
        self.panel.depth != Depth::Rail
            && self
                .selected_row()
                .is_some_and(|row| row.forgets_credentials())
    }

    pub(super) fn choose_default_identity(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let Some(id) = row.profile().map(str::to_owned) else {
            return;
        };
        let Some(file) = self.settings_file.as_mut() else {
            self.problem("Settings are unavailable; fix the file and reopen");
            return;
        };
        let mut proposed = file.settings().clone();
        match proposed.choose_default_credential(&id) {
            Ok(()) => match file.save(proposed) {
                Ok(()) => self.say("Default identity updated"),
                Err(error) => self.problem(error.to_string()),
            },
            Err(error) => self.problem(error.to_string()),
        }
    }

    pub(super) fn ask_to_forget_credentials(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let RowKind::Credential(credential) = row.kind else {
            return;
        };
        let Some(id) = credential.profile().map(str::to_owned) else {
            return;
        };
        if let Some(refusal) = self
            .settings_file
            .as_ref()
            .and_then(|file| file.settings().binding_refusal(&id))
        {
            self.problem(refusal);
            return;
        }
        let tracker = Service::from_id(&id).is_some_and(|service| service.slot() == Slot::Tracker);
        let question = format!(
            "Delete the {} identity? Its login and profile are forgotten here. {}",
            credential.label,
            if tracker {
                "Projects must stop using it first."
            } else {
                "Projects and the default must stop using it first."
            }
        );
        self.foreground = Foreground::Confirm(PendingConfirm::forgetting_credentials(id, question));
    }

    pub(in crate::tui) async fn forget_credentials<E>(
        &mut self,
        id: String,
        service: &TaskService<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(file) = self.settings_file.as_mut() else {
            self.problem("Settings are unavailable; fix the file and reopen");
            return;
        };
        let mut proposed = file.settings().clone();
        if let Err(error) = proposed.remove_credential(&id) {
            self.problem(error.to_string());
            return;
        }
        let service_of = Service::from_id(&id);
        let removed = if service_of.is_some_and(|service| service.is_mounted()) {
            service
                .credentials()
                .forget(&id)
                .map_err(|error| error.to_string())
        } else {
            match service.tracker_connectable(&id) {
                Some(tracker) => tracker
                    .clear_credentials()
                    .await
                    .map_err(|error| error.to_string()),
                None => Err("this tracker identity is unavailable".to_owned()),
            }
        };
        self.foreground = Foreground::Panel;
        let result = match removed {
            Ok(()) => {
                if let Err(error) = self.config.remove_profile(&id) {
                    tracing::warn!(%error, profile = %id, "could not remove a deleted identity's profile");
                }
                self.credential_status.remove(&id);
                self.credential_checked_at.remove(&id);
                self.credential_checking.remove(&id);
                self.tracker_states.remove(&id);
                self.profiles.retain(|profile| profile.profile != id);
                file.save(proposed).map_err(|error| error.to_string())
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                self.credential_check_generation = self.credential_check_generation.wrapping_add(1);
                self.credentials_visible_since = None;
                self.say("Identity deleted");
            }
            Err(error) => self.problem(format!("Could not delete the identity: {error}")),
        }
    }

    pub(super) fn signed_out(&self, profile: &'static str) -> bool {
        matches!(
            self.credential_status.get(profile),
            Some(CredentialStatus::Checked {
                signed_in: false,
                ..
            })
        )
    }

    pub(super) fn signed_out_vcs(&self) -> bool {
        self.signed_out(self.active_vcs().profile())
    }

    pub(super) fn setup_profiles(&self) -> Vec<String> {
        let mut profiles = Vec::new();
        if let Some(agent) = self.setup_agent() {
            profiles.push(agent.profile().to_owned());
        }
        profiles.push(self.active_vcs().profile().to_owned());
        profiles
    }

    pub(super) fn leave_credentials(&mut self) {
        self.recheck_signed_in_profile();
        self.foreground = self.shell_behind.uncover().unwrap_or(Foreground::Panel);
    }

    pub(super) fn credential_tag(&self, credential: &Credential) -> String {
        let Some(service) = credential.service() else {
            return credential.label.clone();
        };
        let account = self.profile_account(&credential.id);
        let group = account
            .as_ref()
            .and_then(|account| account.workspace.as_deref());
        // A name the operator gave wins: it is the one they will recognise. A
        // name nobody gave is only the tool's display name, which the tag already
        // says, so it is left out — but an account that named itself is the
        // person rather than the tool, and that is worth printing.
        let identity = if credential.label_auto {
            account
                .as_ref()
                .map(|account| account.display_name.clone())
                .unwrap_or_default()
        } else {
            credential.label.clone()
        };
        describe(service, group, &identity)
    }

    pub(in crate::tui) fn described_credentials(&self) -> Vec<Credential> {
        let mut described: Vec<Credential> = self
            .settings_credentials()
            .into_iter()
            .map(|mut credential| {
                credential.label = self.credential_tag(&credential);
                credential
            })
            .collect();
        // Two logins that read the same tag are told apart by their ids: the tag
        // has nothing left to say about them, and the id is the one thing in the
        // file that differs. A named login never gets here — its name is its
        // identity, so naming it is what makes its tag its own.
        let tags: Vec<String> = described.iter().map(|one| one.label.clone()).collect();
        let mut told_apart: Vec<Option<String>> = vec![None; described.len()];
        for (index, tag) in tags.iter().enumerate() {
            if tags.iter().filter(|other| *other == tag).count() > 1 {
                told_apart[index] = Some(described[index].id.clone());
            }
        }
        for (credential, told_apart) in described.iter_mut().zip(told_apart) {
            if let Some(told_apart) = told_apart {
                credential.label = told_apart;
            }
        }
        described
    }

    pub(in crate::tui) fn settings_credentials(&self) -> Vec<Credential> {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().credentials().to_vec())
            .unwrap_or_default()
    }

    pub(in crate::tui) fn reload_profiles<E>(&mut self, task_service: &TaskService<E>)
    where
        E: IsolationEngine + Clone + 'static,
    {
        self.profiles = self
            .settings_credentials()
            .iter()
            .filter_map(|credential| task_service.credentials().summary(&credential.id))
            .collect();
    }

    pub(in crate::tui) fn ask_credential_checks<E>(
        &mut self,
        task_service: &TaskService<E>,
        reports: &mpsc::UnboundedSender<CredentialCheckReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let profiles = self.unasked_profiles();
        self.ask_profiles(profiles, task_service, reports);
    }

    pub(in crate::tui) fn credential_checked(&mut self, reply: CredentialCheckReply) {
        if reply.generation != self.credential_check_generation {
            return;
        }
        let mut nothing_answered = false;
        let mut suggested = None;
        for (profile, status) in reply.answers {
            self.credential_checking.remove(&profile);
            self.credential_checked_at
                .insert(profile.clone(), Instant::now());
            nothing_answered |= matches!(status, CredentialStatus::Unanswered);
            if let CredentialStatus::Checked {
                signed_in: true,
                output,
            } = &status
                && Service::from_id(&profile)
                    == Some(Service::VersionControl(VcsProviderKind::GitHub))
                && let Some(login) = github_login(output)
            {
                suggested = Some((profile.clone(), login));
            }
            self.credential_status.insert(profile, status);
        }
        if let Some((id, login)) = suggested {
            self.adopt_identity_name(&id, &login);
        }
        if nothing_answered {
            self.credential_retry_at = Some(Instant::now() + CHECK_RETRY_AFTER);
        }
    }

    pub(in crate::tui) fn sync_credentials(&mut self) -> bool {
        let showing =
            matches!(self.foreground, Foreground::Panel) && self.panel.section.slot().is_some();
        match (showing, self.credentials_visible_since) {
            (true, None) => {
                self.credentials_visible_since = Some(Instant::now());
                self.credential_retry_at = None;
                true
            }
            (false, Some(_)) => {
                self.credentials_visible_since = None;
                true
            }
            _ => false,
        }
    }

    pub(in crate::tui) fn is_asking_credentials(&self) -> bool {
        !self.credential_checking.is_empty()
            || self
                .credential_status
                .values()
                .any(CredentialStatus::is_in_progress)
    }

    pub(in crate::tui) fn wants_credential_check(&self) -> bool {
        let Some(since) = self.credentials_visible_since else {
            return false;
        };
        since.elapsed() >= ASK_AFTER
            && !self.is_asking_credentials()
            && !self.unasked_profiles().is_empty()
    }
}
