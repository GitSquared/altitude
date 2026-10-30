//! The panel's rows, assembled from the application's own state: what the
//! settings file holds, what each remote last answered, and what the built base
//! image is made of.

use crate::{
    credentials::{Credential as NamedCredential, Service, Slot},
    project::ProjectCommits,
    tui::{
        App,
        app::{CredentialStatus, RemoteStatus},
    },
};

use super::{
    pages::{hostname, public_identity},
    panel::{Section, Setting},
    rows::{
        Credential, CredentialKind, GitIdentity, ProjectDefaults, Row, RowKind, TrackerBinding,
        View, switch_row,
    },
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
                        .repository_display()
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
        // Named as the pages print them: one form for a login everywhere, rather
        // than a page that has to describe what the list already decided.
        let credentials = self.described_credentials();
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
            let default = self.default_commits();
            let saved = self.saved_identities();
            if let Some(commits) = default.clone()
                && !saved
                    .iter()
                    .any(|known| known.email.eq_ignore_ascii_case(&commits.email))
            {
                rows.push(self.commit_row(&commits, false, default.as_ref()));
            }
            for commits in &saved {
                rows.push(self.commit_row(commits, true, default.as_ref()));
            }
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

    fn commit_row(
        &self,
        commits: &ProjectCommits,
        saved: bool,
        default: Option<&ProjectCommits>,
    ) -> Row {
        let label = if commits.label.is_empty() {
            commits.name.clone()
        } else {
            commits.label.clone()
        };
        let is_default =
            default.is_some_and(|known| known.email.eq_ignore_ascii_case(&commits.email));
        let used_by = self
            .projects()
            .into_iter()
            .filter_map(|project| {
                let uses = match project.commits.as_ref() {
                    Some(bound) => bound.email.eq_ignore_ascii_case(&commits.email),
                    None => is_default,
                };
                (uses && !project.name.trim().is_empty()).then_some(project.name)
            })
            .collect();
        Row {
            label,
            detail: Some(format!("git: {}", commits.email)),
            kind: RowKind::GitIdentity(GitIdentity {
                label: commits.label.clone(),
                name: Some(commits.name.clone()),
                email: Some(commits.email.clone()),
                signing_key: commits.signing_key.clone(),
                signs_commits: commits.signing_key.is_some(),
                used_by,
                saved: saved.then(|| commits.clone()),
            }),
        }
    }

    /// One named login, with the service it belongs to, the place it lives, and
    /// what its own check last said. The label is the operator's; the service is
    /// the fallback that keeps two same-named identities readable.
    fn identity_row(&self, credential: NamedCredential, service: Service) -> Row {
        let label = self.credential_label(&credential);
        let id = credential.id;
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
        let detail = format!("{}: {}", service.label(), account.unwrap_or(status));
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
            // What this machine gives the remote's tracker work: the login that
            // travels with the projects running there, and the team each project
            // claims. The remote holds the app; these are the other half of the
            // answer, and they are this machine's to know.
            let binding = self.tracker_binding(&address);
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
                    binding,
                },
            }
        }));
        rows.extend(self.discovered_rows());
        rows
    }

    /// The tracker work this machine has bound to one remote.
    ///
    /// Read from the projects that run there: the login each one's route names,
    /// and the team it claims. A remote with the app and no project of ours is
    /// bound to nothing, which is a different answer from a remote without the
    /// app.
    fn tracker_binding(&self, address: &str) -> Option<TrackerBinding> {
        let projects: Vec<_> = self
            .projects()
            .into_iter()
            .filter(|project| project.host.as_deref() == Some(address))
            .collect();
        let default = self
            .settings_file
            .as_ref()
            .and_then(|file| file.settings().default_credential(Slot::Tracker))
            .map(str::to_owned);
        let mut identities: Vec<String> = Vec::new();
        let mut workspace = None;
        let mut teams = Vec::new();
        for project in &projects {
            let Some(route) = &project.tracker else {
                continue;
            };
            if let Some(credential) = route.credential.clone().or_else(|| default.clone()) {
                // The account says whose login it is and which workspace it is
                // in; the label says what the operator called it. Both are read
                // here rather than asked over the network.
                let label = self
                    .settings_file
                    .as_ref()
                    .and_then(|file| file.settings().credential(&credential))
                    .map(|credential| credential.label.clone())
                    .unwrap_or_else(|| credential.clone());
                let account = self.profile_account(&credential);
                let identity = account
                    .as_ref()
                    .map(|account| account.display_name.clone())
                    .unwrap_or_else(|| label.clone());
                if !identities.contains(&identity) {
                    identities.push(identity);
                }
                // A name the login adopted carries its own workspace —
                // `gaby@acme` — so it answers the question an account recorded
                // before workspaces were fetched cannot.
                if workspace.is_none() {
                    workspace = account.and_then(|account| account.workspace).or_else(|| {
                        label
                            .split_once('@')
                            .map(|(_, workspace)| workspace.to_owned())
                    });
                }
            }
            teams.push((
                route
                    .scope
                    .clone()
                    .unwrap_or_else(|| "whole account".to_owned()),
                project.name.clone(),
            ));
        }
        (!identities.is_empty() || !teams.is_empty()).then_some(TrackerBinding {
            workspace,
            identities,
            teams,
        })
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
