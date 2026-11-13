use super::*;

impl Settings {
    pub(super) fn validate(&self) -> Result<(), SettingsError> {
        let mut credential_ids = HashSet::new();
        for credential in &self.credentials {
            if !credential.is_valid() {
                return Err(SettingsError::Invalid(format!(
                    "credential {:?} is not a usable named identity",
                    credential.id
                )));
            }
            if !credential_ids.insert(credential.id.as_str()) {
                return Err(SettingsError::Invalid(format!(
                    "duplicate credential id: {}",
                    credential.id
                )));
            }
        }
        for (slot, id) in [
            (Slot::Agent, self.defaults.agent_credential.as_deref()),
            (
                Slot::VersionControl,
                self.defaults.vcs_credential.as_deref(),
            ),
            (Slot::Tracker, self.defaults.tracker_credential.as_deref()),
        ] {
            if let Some(id) = id
                && (!credential_ids.contains(id)
                    || Service::from_id(id).is_none_or(|service| service.slot() != slot))
            {
                return Err(SettingsError::Invalid(format!(
                    "default credential {id:?} does not name a {slot:?} identity"
                )));
            }
        }
        for project in &self.projects {
            for (slot, id) in [
                (Slot::Agent, project.agent_credential.as_deref()),
                (Slot::VersionControl, project.vcs_credential.as_deref()),
                (
                    Slot::Tracker,
                    project
                        .tracker
                        .as_ref()
                        .and_then(|binding| binding.credential.as_deref()),
                ),
            ] {
                if let Some(id) = id
                    && (!credential_ids.contains(id)
                        || Service::from_id(id).is_none_or(|service| service.slot() != slot))
                {
                    return Err(SettingsError::Invalid(format!(
                        "project {:?} binds {id:?}, which is not a {slot:?} identity",
                        project.name
                    )));
                }
            }
        }
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for project in &self.projects {
            project
                .validate()
                .map_err(|error| SettingsError::Invalid(error.to_string()))?;
            // A project id is a uuid, because storage is keyed by it and because
            // altitude builds a few sandboxes under ids of its own.
            if project.id.parse::<uuid::Uuid>().is_err() {
                return Err(SettingsError::Invalid(format!(
                    "project id {:?} must be a uuid",
                    project.id
                )));
            }
            if !ids.insert(project.id.clone()) {
                return Err(SettingsError::Invalid(format!(
                    "duplicate project id: {}",
                    project.id
                )));
            }
            if !names.insert(project.key()) {
                return Err(SettingsError::Invalid(format!(
                    "duplicate project name: {}",
                    project.name
                )));
            }
        }
        // Two projects claiming one scope is a question nobody can answer when the
        // issue arrives, so it is refused while the file is being written. A binding
        // with no scope claims its whole account, and only one project may.
        let mut claims: Vec<(String, &str)> = Vec::new();
        for project in &self.projects {
            let Some(binding) = &project.tracker else {
                continue;
            };
            let key = binding.claim_key();
            let what = binding.scope.as_deref().unwrap_or("the whole account");
            if let Some((_, other)) = claims.iter().find(|(claimed, _)| *claimed == key) {
                return Err(SettingsError::Invalid(format!(
                    "{other} and {} both claim {what}",
                    project.name
                )));
            }
            claims.push((key, &project.name));
        }
        let mut addresses = HashSet::new();
        for address in &self.remotes {
            validate_remote_address(address)?;
            if !addresses.insert(address.to_ascii_lowercase()) {
                return Err(SettingsError::Invalid(format!(
                    "duplicate remote address: {address}"
                )));
            }
        }
        Ok(())
    }
}
