//! The panel's rows, assembled from the application's own state: what the
//! settings file holds, what each remote last answered, and what the built
//! environments are made of.

use crate::{
    agent::AgentKind,
    isolation::ProfileSummary,
    tui::{
        App,
        app::{CredentialStatus, RemoteStatus},
        popups::new_task::AGENTS,
    },
};

use super::{
    pages::{agent_profile, hostname},
    panel::{Section, Setting},
    rows::{Credential, CredentialKind, Row, RowKind, View, switch_row},
};

impl App {
    /// Everything the panel draws, assembled from the settings file and from
    /// what was read back about each remote and about the base environment.
    pub(in crate::tui) fn settings_view(&self) -> View {
        let default_environment = self.default_environment_name().to_owned();
        let rows = match self.panel.section {
            Section::Interface => vec![
                switch_row(Setting::KeyboardHints, self.keyboard_hints()),
                switch_row(Setting::Animations, self.animations()),
            ],
            Section::Agents => AGENTS
                .into_iter()
                .map(|kind| {
                    let credential = self.agent_credential(kind);
                    let mut detail = credential.summary().to_owned();
                    if self.agent() == Some(kind) {
                        detail.push_str(" · default");
                    }
                    Row {
                        label: kind.to_string(),
                        detail: Some(detail),
                        kind: RowKind::Agent { kind, credential },
                    }
                })
                .collect(),
            Section::VersionControl => {
                let identity = self.git_identity();
                let mut rows = vec![Row {
                    label: "Git identity".to_owned(),
                    detail: Some(if identity.is_set() { "set" } else { "not set" }.to_owned()),
                    kind: RowKind::GitIdentity(identity),
                }];
                rows.extend(
                    self.profiles
                        .iter()
                        .filter(|profile| profile.profile.starts_with("vcs/"))
                        .map(|profile| {
                            let credential = self.profile_credential(profile);
                            let detail = credential.summary().to_owned();
                            Row {
                                label: profile.display.to_owned(),
                                detail: Some(detail),
                                kind: RowKind::Credential(credential),
                            }
                        }),
                );
                rows
            }
            Section::Trackers => vec![Row {
                label: "Linear".to_owned(),
                detail: Some(self.tracker_state.summary().to_owned()),
                kind: RowKind::Tracker {
                    state: self.tracker_state,
                },
            }],
            Section::Environments => self
                .environments()
                .into_iter()
                .map(|environment| {
                    let built_in = environment.is_base();
                    let is_default = environment.name.eq_ignore_ascii_case(&default_environment);
                    Row {
                        label: environment.name.clone(),
                        // The built-in one says what it is rather than what it is
                        // used for: there is nothing to choose about it.
                        detail: Some(
                            if built_in {
                                "built in"
                            } else if is_default {
                                "default"
                            } else {
                                "custom"
                            }
                            .to_owned(),
                        ),
                        kind: if built_in {
                            RowKind::Base
                        } else {
                            RowKind::Environment {
                                name: environment.name,
                                setup: environment.setup_command,
                                is_default,
                            }
                        },
                    }
                })
                .collect(),
            Section::Remotes => self.remote_rows(),
        };
        View {
            rows,
            base: self.base_description.clone(),
            tracker_profile_dir: self.tracker_profile_dir(),
        }
    }

    /// This installation's hostname, key, and where its state lives.
    fn installation_credential(&self) -> Credential {
        let identity = self.config.identity();
        let hostname = hostname();
        Credential {
            label: hostname.clone(),
            kind: CredentialKind::Installation {
                hostname,
                label: identity
                    .as_ref()
                    .map(|identity| identity.label.clone())
                    .unwrap_or_else(|error| format!("unreadable: {error}")),
                key: identity
                    .as_ref()
                    .map(|identity| identity.id.clone())
                    .unwrap_or_else(|error| format!("unreadable: {error}")),
                data_dir: self.config.data_dir().display().to_string(),
            },
        }
    }

    /// One profile's login, with the place it lives and what its own check last
    /// said. Names and places only: a credential is never read, and never shown.
    fn profile_credential(&self, profile: &ProfileSummary) -> Credential {
        Credential {
            label: profile.display.to_owned(),
            kind: CredentialKind::Profile {
                profile: profile.profile,
                directory: self
                    .config
                    .profile_dir(profile.profile)
                    .display()
                    .to_string(),
                status: self
                    .credential_status
                    .get(profile.profile)
                    .cloned()
                    .unwrap_or_default(),
            },
        }
    }

    /// The login of the profile a coding agent runs under.
    fn agent_credential(&self, kind: AgentKind) -> Credential {
        let profile = agent_profile(kind);
        match self
            .profiles
            .iter()
            .find(|summary| summary.profile == profile)
        {
            Some(summary) => self.profile_credential(summary),
            None => Credential {
                label: kind.to_string(),
                kind: CredentialKind::Profile {
                    profile,
                    directory: String::new(),
                    status: CredentialStatus::default(),
                },
            },
        }
    }

    /// The addresses the settings file configures.
    pub(in crate::tui) fn configured_remotes(&self) -> Vec<String> {
        self.settings_file
            .as_ref()
            .map(|file| file.settings().remotes().to_vec())
            .unwrap_or_default()
    }

    /// The remotes list: detected remotes go last, so adopting one never
    /// reorders the remotes the operator configured.
    fn remote_rows(&self) -> Vec<Row> {
        let installation = self.installation_credential();
        // This installation comes first: it is the identity a remote knows this
        // machine by.
        let detail = installation.summary().to_owned();
        let mut rows = vec![Row {
            label: installation.label.clone(),
            detail: Some(detail),
            kind: RowKind::Credential(installation),
        }];
        rows.extend(self.configured_remotes().into_iter().map(|address| {
            let remembered = self
                .remembered_keys
                .iter()
                .find(|key| key.address.eq_ignore_ascii_case(&address))
                .cloned();
            let status = self
                .remote_status
                .get(&address)
                .cloned()
                .unwrap_or(RemoteStatus::Unknown);
            let trackers = self.remote_trackers.get(&address).cloned();
            Row {
                // A name is what the remote called itself, so an address
                // that has never answered shows itself instead.
                label: match &remembered {
                    Some(key) => key.label.clone(),
                    None => address.clone(),
                },
                detail: Some(address.clone()),
                kind: RowKind::Remote {
                    needs_config: self.remote_needs_config(&address),
                    address,
                    remembered,
                    status,
                    trackers,
                },
            }
        }));
        rows.extend(self.discovered_rows());
        rows
    }

    /// Discovered remotes, ordered by the name they announce so the list does
    /// not reshuffle as announcements arrive.
    fn discovered_rows(&self) -> Vec<Row> {
        let configured = self.configured_remotes();
        let mut discovered = self
            .discovered_remotes
            .values()
            .filter(|remote| {
                !configured
                    .iter()
                    .any(|address| address.eq_ignore_ascii_case(&remote.address))
                    && !self.remembered_keys.iter().any(|key| {
                        key.key == remote.id.to_string() && configured.contains(&key.address)
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        discovered.sort_by(|left, right| {
            left.label
                .cmp(&right.label)
                .then_with(|| left.address.cmp(&right.address))
        });
        discovered
            .into_iter()
            .map(|remote| Row {
                label: if remote.label.trim().is_empty() {
                    remote.address.clone()
                } else {
                    remote.label.clone()
                },
                detail: Some(remote.address.clone()),
                kind: RowKind::Discovered(remote),
            })
            .collect()
    }

    /// How many rows the focused list holds, which is what moving needs.
    ///
    /// Read from the view the list is drawn from, so a section that gains a row
    /// cannot be counted wrong by a second list that has to be kept in step.
    pub(in crate::tui) fn row_count(&self) -> usize {
        self.settings_view().rows.len()
    }

    pub(in crate::tui) fn selected_row(&self) -> Option<Row> {
        self.settings_view()
            .rows
            .into_iter()
            .nth(self.panel.selected())
    }
}
