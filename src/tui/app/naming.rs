use crate::credentials::{Credential, Service, new_id};

use super::*;

impl App {
    pub(super) fn ensure_credential(&mut self, id: &str, label: &str) -> bool {
        let Some(service) = Service::from_id(id) else {
            return false;
        };
        if self
            .settings_file
            .as_ref()
            .is_some_and(|file| file.settings().credential(id).is_some())
        {
            return false;
        }
        let label = if label.trim().is_empty() {
            service.label().to_owned()
        } else {
            label.to_owned()
        };
        let Some(file) = self.settings_file.as_mut() else {
            return false;
        };
        let mut proposed = file.settings().clone();
        if proposed.add_credential(Credential::new(id, label)).is_err() {
            return false;
        }
        if let Err(error) = file.save(proposed) {
            tracing::warn!(%error, "could not record the identity a sign-in started");
            return false;
        }
        true
    }

    pub(super) fn adopt_identity_name(&mut self, id: &str, label: &str) -> String {
        let Some(service) = Service::from_id(id) else {
            return id.to_owned();
        };
        let Some(credential) = self
            .settings_file
            .as_ref()
            .and_then(|file| file.settings().credential(id))
            .cloned()
        else {
            return id.to_owned();
        };
        if !credential.label_auto {
            return id.to_owned();
        }
        let target = new_id(service, label, |candidate| {
            candidate != id
                && (self
                    .settings_file
                    .as_ref()
                    .is_some_and(|file| file.settings().credential(candidate).is_some())
                    || self.config.profile_dir(candidate).is_dir())
        });
        if target == id {
            self.suggest_credential_name(id, label);
            return id.to_owned();
        }
        if self.tasks.iter().any(|task| {
            task.project().is_some_and(|project| {
                project.agent_credential.as_deref() == Some(id)
                    || project.vcs_credential.as_deref() == Some(id)
                    || project
                        .tracker
                        .as_ref()
                        .and_then(|binding| binding.credential.as_deref())
                        == Some(id)
            })
        }) {
            self.suggest_credential_name(id, label);
            return id.to_owned();
        }
        if let Err(error) = self.config.move_profile(id, &target) {
            tracing::warn!(%error, from = %id, to = %target, "could not move an identity's profile");
            self.suggest_credential_name(id, label);
            return id.to_owned();
        }
        let Some(file) = self.settings_file.as_mut() else {
            return id.to_owned();
        };
        let mut proposed = file.settings().clone();
        if let Err(error) = proposed.reidentify(id, &target, label) {
            self.problem(error.to_string());
            let _ = self.config.move_profile(&target, id);
            return id.to_owned();
        }
        if let Err(error) = file.save(proposed) {
            self.problem(error.to_string());
            let _ = self.config.move_profile(&target, id);
            return id.to_owned();
        }
        if let Some(status) = self.credential_status.remove(id) {
            self.credential_status.insert(target.clone(), status);
        }
        if let Some(at) = self.credential_checked_at.remove(id) {
            self.credential_checked_at.insert(target.clone(), at);
        }
        if self.credential_checking.remove(id) {
            self.credential_checking.insert(target.clone());
        }
        if let Some(state) = self.tracker_states.remove(id) {
            self.tracker_states.insert(target.clone(), state);
        }
        for profile in &mut self.profiles {
            if profile.profile == id {
                profile.profile = target.clone();
            }
        }
        if self.tracker_connecting.as_deref() == Some(id) {
            self.tracker_connecting = Some(target.clone());
        }
        target
    }

    pub(super) fn suggest_credential_name(&mut self, id: &str, label: &str) {
        let Some(file) = self.settings_file.as_mut() else {
            return;
        };
        let mut proposed = file.settings().clone();
        proposed.suggest_credential_name(id, label);
        if proposed.credentials() != file.settings().credentials()
            && let Err(error) = file.save(proposed)
        {
            tracing::warn!(%error, "could not keep a suggested identity name");
        }
    }
}
