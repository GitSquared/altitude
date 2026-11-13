use super::*;

impl Settings {
    pub(super) fn normalize(&mut self, existing: &[Credential]) {
        if self.credentials.is_empty() {
            self.credentials = existing.to_vec();
        }
        let known = |id: &str| {
            self.credentials
                .iter()
                .any(|credential| credential.id == id)
        };
        if self
            .defaults
            .agent_credential
            .as_deref()
            .is_none_or(|id| !known(id))
        {
            // The operator chose a harness, so the login is that harness's: the
            // one it has, whether the operator named it or not. Two of them are
            // two to choose between, and the settings screen asks.
            self.defaults.agent_credential = self.defaults.agent.and_then(|agent| {
                Self::sole_credential(&self.credentials, |id| agent_of(Some(id)) == Some(agent))
            });
        }
        if self
            .defaults
            .vcs_credential
            .as_deref()
            .is_none_or(|id| !known(id))
        {
            self.defaults.vcs_credential =
                Some(VcsProviderKind::sole().profile().to_owned()).filter(|id| known(id));
        }
        if self
            .defaults
            .tracker_credential
            .as_deref()
            .is_none_or(|id| !known(id))
        {
            self.defaults.tracker_credential =
                Some(TrackerKind::sole().profile().to_owned()).filter(|id| known(id));
        }
        for project in &mut self.projects {
            if project
                .agent_credential
                .as_deref()
                .is_none_or(|id| !known(id))
            {
                project.agent_credential = project
                    .agent
                    .and_then(|agent| {
                        Self::sole_credential(&self.credentials, |id| {
                            agent_of(Some(id)) == Some(agent)
                        })
                    })
                    .or_else(|| self.defaults.agent_credential.clone());
            }
            if project
                .vcs_credential
                .as_deref()
                .is_none_or(|id| !known(id))
            {
                project.vcs_credential = self.defaults.vcs_credential.clone();
            }
            if let Some(binding) = &mut project.tracker
                && binding.credential.as_deref().is_none_or(|id| !known(id))
            {
                binding.credential = self.defaults.tracker_credential.clone();
            }
        }
        // A project answers what it runs as soon as it is read, so no screen has
        // to work out an answer the file could have said. The identity decides
        // it: a login for Pi is a project that runs Pi.
        let installation = self.defaults.agent;
        for project in &mut self.projects {
            project.agent = project
                .agent
                .or(agent_of(project.agent_credential.as_deref()));
            project.agent = project.agent.or(installation);
        }
        // One login for a slot is that slot's default: there is nothing to choose
        // between, and a default nobody set is what makes a connected tracker look
        // like a feature the build does not have.
        for slot in [Slot::Agent, Slot::VersionControl, Slot::Tracker] {
            if self.default_credential(slot).is_some() {
                continue;
            }
            let only = Self::sole_credential(&self.credentials, |id| {
                Service::from_id(id).is_some_and(|service| service.slot() == slot)
            });
            if let Some(id) = only {
                let _ = self.choose_default_credential(&id);
            }
        }
    }

    // The id of the one login that answers, when there is exactly one: a login a
    // default can be worked out from, rather than one of several to choose.
    fn sole_credential(credentials: &[Credential], keep: impl Fn(&str) -> bool) -> Option<String> {
        let mut matching = credentials
            .iter()
            .map(|credential| credential.id.as_str())
            .filter(|id| keep(id));
        let only = matching.next()?;
        matching.next().is_none().then(|| only.to_owned())
    }

    pub fn credentials(&self) -> &[Credential] {
        &self.credentials
    }

    pub fn credential(&self, id: &str) -> Option<&Credential> {
        self.credentials
            .iter()
            .find(|credential| credential.id == id)
    }

    pub fn default_credential(&self, slot: Slot) -> Option<&str> {
        match slot {
            Slot::Agent => self.defaults.agent_credential.as_deref(),
            Slot::VersionControl => self.defaults.vcs_credential.as_deref(),
            Slot::Tracker => self.defaults.tracker_credential.as_deref(),
        }
    }

    pub fn add_credential(&mut self, credential: Credential) -> Result<(), SettingsError> {
        if !credential.is_valid() {
            return Err(SettingsError::Invalid(format!(
                "credential {:?} is not a usable named identity",
                credential.id
            )));
        }
        if self.credential(&credential.id).is_some() {
            return Err(SettingsError::Invalid(format!(
                "credential {:?} already exists",
                credential.id
            )));
        }
        self.credentials.push(credential);
        Ok(())
    }

    pub fn rename_credential(&mut self, id: &str, label: &str) -> Result<(), SettingsError> {
        let label = label.trim();
        if label.is_empty() || label.len() > 64 || label.chars().any(char::is_control) {
            return Err(SettingsError::Invalid(
                "a name needs one short line".to_owned(),
            ));
        }
        if self
            .credentials
            .iter()
            .any(|credential| credential.id != id && credential.label.eq_ignore_ascii_case(label))
        {
            return Err(SettingsError::Invalid(format!("{label:?} is already used")));
        }
        let Some(credential) = self
            .credentials
            .iter_mut()
            .find(|credential| credential.id == id)
        else {
            return Err(SettingsError::Invalid(format!("no credential {id:?}")));
        };
        credential.label = label.to_owned();
        credential.label_auto = false;
        Ok(())
    }

    pub fn reidentify(&mut self, old: &str, new: &str, label: &str) -> Result<(), SettingsError> {
        if old == new {
            self.suggest_credential_name(old, label);
            return Ok(());
        }
        let candidate = Credential::new(new, label);
        if !candidate.is_valid() {
            return Err(SettingsError::Invalid(format!(
                "{new:?} is not a usable identity id"
            )));
        }
        if self.credential(new).is_some() {
            return Err(SettingsError::Invalid(format!(
                "identity {new:?} already exists"
            )));
        }
        let Some(credential) = self.credentials.iter_mut().find(|c| c.id == old) else {
            return Err(SettingsError::Invalid(format!("no credential {old:?}")));
        };
        credential.id = new.to_owned();
        credential.label = label.to_owned();
        for default in [
            &mut self.defaults.agent_credential,
            &mut self.defaults.vcs_credential,
            &mut self.defaults.tracker_credential,
        ] {
            if default.as_deref() == Some(old) {
                *default = Some(new.to_owned());
            }
        }
        for project in &mut self.projects {
            if project.agent_credential.as_deref() == Some(old) {
                project.agent_credential = Some(new.to_owned());
            }
            if project.vcs_credential.as_deref() == Some(old) {
                project.vcs_credential = Some(new.to_owned());
            }
            if let Some(binding) = &mut project.tracker
                && binding.credential.as_deref() == Some(old)
            {
                binding.credential = Some(new.to_owned());
            }
        }
        Ok(())
    }

    pub fn suggest_credential_name(&mut self, id: &str, label: &str) {
        let label = label.trim();
        if label.is_empty() || label.len() > 64 || label.chars().any(char::is_control) {
            return;
        }
        if self
            .credentials
            .iter()
            .any(|credential| credential.id != id && credential.label.eq_ignore_ascii_case(label))
        {
            return;
        }
        if let Some(credential) = self
            .credentials
            .iter_mut()
            .find(|credential| credential.id == id)
            && credential.label_auto
        {
            credential.label = label.to_owned();
        }
    }

    pub fn choose_default_credential(&mut self, id: &str) -> Result<(), SettingsError> {
        let Some(credential) = self.credential(id) else {
            return Err(SettingsError::Invalid(format!("no credential {id:?}")));
        };
        match credential.service().map(|service| service.slot()) {
            Some(Slot::Agent) => self.defaults.agent_credential = Some(id.to_owned()),
            Some(Slot::VersionControl) => self.defaults.vcs_credential = Some(id.to_owned()),
            Some(Slot::Tracker) => self.defaults.tracker_credential = Some(id.to_owned()),
            None => {
                return Err(SettingsError::Invalid(format!(
                    "{id:?} is not a known service"
                )));
            }
        }
        Ok(())
    }

    pub fn projects_binding(&self, id: &str) -> Vec<&str> {
        self.projects
            .iter()
            .filter(|project| {
                project.agent_credential.as_deref() == Some(id)
                    || project.vcs_credential.as_deref() == Some(id)
                    || project
                        .tracker
                        .as_ref()
                        .and_then(|binding| binding.credential.as_deref())
                        == Some(id)
            })
            .map(|project| project.name.as_str())
            .collect()
    }

    pub fn binding_refusal(&self, id: &str) -> Option<String> {
        let bound = self.projects_binding(id);
        if bound.is_empty() {
            return None;
        }
        let one = bound.len() == 1;
        Some(format!(
            "{} still {} this identity; change {} first",
            named(&bound),
            if one { "binds" } else { "bind" },
            if one {
                "the project's login"
            } else {
                "their logins"
            },
        ))
    }

    pub fn remove_credential(&mut self, id: &str) -> Result<(), SettingsError> {
        if let Some(refusal) = self.binding_refusal(id) {
            return Err(SettingsError::Invalid(refusal));
        }
        // A tracker is optional, so its default may go with its last identity.
        // An agent and a version-control login are what a task runs with, so
        // those keep the default until another identity takes it over.
        if self.defaults.tracker_credential.as_deref() == Some(id) {
            self.defaults.tracker_credential = None;
        } else if [
            self.defaults.agent_credential.as_deref(),
            self.defaults.vcs_credential.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|default| default == id)
        {
            return Err(SettingsError::Invalid(
                "this identity is a default; make another one the default first".to_owned(),
            ));
        }
        self.credentials.retain(|credential| credential.id != id);
        Ok(())
    }

    pub fn used_credential_ids(&self) -> Vec<String> {
        let mut used = Vec::new();
        for project in &self.projects {
            let agent = project
                .agent_credential
                .clone()
                .or_else(|| project.agent.map(|kind| kind.profile().to_owned()));
            let vcs = project
                .vcs_credential
                .clone()
                .or_else(|| project.vcs.map(|kind| kind.profile().to_owned()));
            let tracker = project
                .tracker
                .as_ref()
                .and_then(|binding| binding.credential.clone());
            used.extend(agent.into_iter().chain(vcs).chain(tracker));
        }
        used.sort_unstable();
        used.dedup();
        used
    }
}
