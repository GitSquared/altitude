//! The panel's rows, assembled from the application's own state: what the
//! settings file holds, what each remote last answered, and what the built base
//! image is made of.

use crate::{
    credentials::{Credential as NamedCredential, Service, Slot},
    tui::{
        App,
        app::{CredentialStatus, RemoteStatus},
    },
};

use super::{
    pages::{hostname, public_identity},
    panel::{Section, Setting},
    rows::{Credential, CredentialKind, ProjectDefaults, Row, RowKind, View, switch_row},
};

impl App {
    /// Everything the panel draws, assembled from the settings file and from
    /// what was read back about each remote and about the built base image.
    pub(in crate::tui) fn settings_view(&self) -> View {
        let rows = match self.panel.section {
            Section::Agents | Section::VersionControl | Section::Trackers => {
                self.identity_rows(self.panel.section)
            }
            Section::Interface => vec![
                switch_row(Setting::KeyboardHints, self.keyboard_hints()),
                switch_row(Setting::Animations, self.animations()),
            ],
            Section::Projects => self
                .projects()
                .into_iter()
                .map(|project| {
                    let detail = project
                        .repository_label()
                        .unwrap_or_else(|| "no repository".to_owned());
                    Row {
                        label: project.name.clone(),
                        detail: Some(detail),
                        kind: RowKind::Project { project },
                    }
                })
                .collect(),
            Section::Remotes => self.remote_rows(),
        };
        let credentials = self.settings_credentials();
        let tracker_states = credentials
            .iter()
            .filter(|credential| {
                credential
                    .service()
                    .is_some_and(|service| service.slot() == Slot::Tracker)
            })
            .map(|credential| {
                (
                    credential.id.clone(),
                    self.tracker_state_for(&credential.id),
                )
            })
            .collect();
        View {
            rows,
            credentials,
            defaults: self.project_defaults(),
            tracker_states,
            base: self.base_description.clone(),
        }
    }

    /// What a project with no binding of its own uses. The page shows the
    /// identity this names, never the fact that it was inherited.
    pub(in crate::tui) fn project_defaults(&self) -> ProjectDefaults {
        let settings = self.settings_file.as_ref().map(|file| file.settings());
        ProjectDefaults {
            agent: settings
                .and_then(|settings| settings.default_credential(Slot::Agent))
                .map(str::to_owned),
            vcs: settings
                .and_then(|settings| settings.default_credential(Slot::VersionControl))
                .map(str::to_owned),
            tracker: settings
                .and_then(|settings| settings.default_credential(Slot::Tracker))
                .map(str::to_owned),
            commits: self.default_commits(),
        }
    }

    /// The projects whose effective binding for this slot is the identity. A
    /// project that inherits the installation's fallback is bound to it in what
    /// it does, so it is listed here even though the project file says nothing.
    fn projects_bound_to(&self, id: &str, slot: Slot) -> Vec<String> {
        let fallback = self
            .settings_file
            .as_ref()
            .and_then(|file| file.settings().default_credential(slot));
        self.projects()
            .into_iter()
            .filter_map(|project| {
                let effective = match slot {
                    Slot::Agent => project.agent_credential.as_deref().or(fallback),
                    Slot::VersionControl => project.vcs_credential.as_deref().or(fallback),
                    Slot::Tracker => project
                        .tracker
                        .as_ref()
                        .and_then(|route| route.credential.as_deref().or(fallback)),
                };
                (effective == Some(id) && !project.name.trim().is_empty()).then_some(project.name)
            })
            .collect()
    }

    /// The identities one service section holds, with the git identity above the
    /// version-control logins it commits through.
    fn identity_rows(&self, section: Section) -> Vec<Row> {
        let mut rows = Vec::new();
        if section == Section::VersionControl {
            let mut identity = self.git_identity();
            identity.used_by = if identity.is_set() {
                self.projects_using_default_commits()
            } else {
                Vec::new()
            };
            let detail = match (&identity.name, &identity.email) {
                (Some(name), _) => name.clone(),
                (None, Some(email)) => email.clone(),
                (None, None) => "not set".to_owned(),
            };
            rows.push(Row {
                label: "Git identity".to_owned(),
                detail: Some(detail),
                kind: RowKind::GitIdentity(identity),
            });
        }
        rows.extend(
            self.settings_credentials()
                .into_iter()
                .filter_map(|credential| {
                    let service = credential.service()?;
                    (Some(service.slot()) == section.slot())
                        .then(|| self.identity_row(credential, service))
                }),
        );
        rows
    }

    /// One named login, with the service it belongs to, the place it lives, and
    /// what its own check last said. The label is the operator's; the service is
    /// the fallback that keeps two same-named identities readable.
    fn identity_row(&self, credential: NamedCredential, service: Service) -> Row {
        let id = credential.id;
        let label = credential.label;
        let (kind, status) = if service.slot() == Slot::Tracker {
            let state = self.tracker_state_for(&id);
            (
                CredentialKind::Tracker {
                    profile: id.clone(),
                    directory: self.config.profile_dir(&id).display().to_string(),
                    status: state,
                },
                state.summary().to_owned(),
            )
        } else {
            let status = self.credential_status.get(&id).cloned().unwrap_or_default();
            (
                CredentialKind::Profile {
                    profile: id.clone(),
                    directory: self.config.profile_dir(&id).display().to_string(),
                    status: status.clone(),
                },
                status.summary().to_owned(),
            )
        };
        let used_by = self.projects_bound_to(&id, service.slot());
        let account = match &kind {
            CredentialKind::Profile {
                status:
                    CredentialStatus::Checked {
                        signed_in: true,
                        output,
                    },
                ..
            } => public_identity(&id, output),
            _ => None,
        };
        let detail = account.unwrap_or(status);
        Row {
            label: label.clone(),
            detail: Some(detail),
            kind: RowKind::Credential(Credential {
                label,
                kind,
                used_by,
            }),
        }
    }

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
            used_by: Vec::new(),
        }
    }

    fn projects_using_default_commits(&self) -> Vec<String> {
        self.projects()
            .into_iter()
            .filter_map(|project| {
                project
                    .commits
                    .is_none()
                    .then_some(project.name)
                    .filter(|name| !name.trim().is_empty())
            })
            .collect()
    }

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
