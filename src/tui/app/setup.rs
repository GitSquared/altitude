use tokio::sync::mpsc;

use crate::{credentials::host, isolation::IsolationEngine, task::TaskService, tui::screens};

use screens::get_started::{RemoteOffer, SetupState};

use super::*;

impl App {
    pub(in crate::tui) fn setup(&self) -> SetupState {
        let agent = self.setup_agent();
        let vcs = self.active_vcs();
        let active = self.active_project_id();
        let configured = self.configured_remotes();
        let offer = self
            .discovered_remotes
            .values()
            .filter(|remote| !configured.contains(&remote.address))
            .filter(|remote| remote.compatibility.is_compatible())
            .min_by(|left, right| left.label.cmp(&right.label))
            .map(|remote| RemoteOffer {
                label: remote.label.clone(),
                address: remote.address.clone(),
            });
        SetupState {
            agent,
            agent_status: agent
                .and_then(|agent| self.credential_status.get(agent.profile()))
                .cloned()
                .unwrap_or_default(),
            vcs,
            vcs_status: self
                .credential_status
                .get(vcs.profile())
                .cloned()
                .unwrap_or_default(),
            repository: self.active_repository(),
            vcs_listing: self.repositories_listing && self.active_repository().is_none(),
            host: self
                .projects()
                .into_iter()
                .find(|project| project.id == active)
                .and_then(|project| project.host),
            offer,
            tracker: self.tracker_state,
            commits: self.git_identity.clone(),
            has_key: self.signing_key_path().is_some(),
            host_logins: host::logins(&self.config, &self.settings_credentials()),
        }
    }

    pub(in crate::tui) fn wants_setup_check(&self) -> bool {
        matches!(self.foreground, Foreground::GetStarted(_))
            && self
                .setup_profiles()
                .iter()
                .any(|profile| !self.credential_status.contains_key(profile.as_str()))
    }

    pub(in crate::tui) fn ask_setup_check<E>(
        &mut self,
        task_service: &TaskService<E>,
        reports: &mpsc::UnboundedSender<CredentialCheckReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let profiles: Vec<String> = self
            .setup_profiles()
            .into_iter()
            .filter(|profile| !self.credential_status.contains_key(profile.as_str()))
            .collect();
        self.ask_profiles(profiles, task_service, reports);
    }
}
