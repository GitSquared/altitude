use std::collections::HashMap;

use crate::{
    credentials::Credential as NamedCredential,
    isolation::EnvironmentDescription,
    project::{Project, ProjectCommits},
    remote::{DiscoveredRemote, RemoteKey, TrackerConnection, TrackerReadiness},
    tui::{
        app::{CredentialStatus, RemoteStatus, Sync, TrackerState},
        widgets::{chrome, palette, utils::Dot},
    },
};

use super::panel::Setting;

pub struct Row {
    pub label: String,
    pub detail: Option<String>,
    pub kind: RowKind,
}

pub struct TrackerBinding {
    pub workspace: Option<String>,
    pub identities: Vec<String>,
    pub teams: Vec<(String, String)>,
}

pub(super) fn tracker_is_set_up(trackers: Option<&[TrackerConnection]>) -> bool {
    trackers.is_some_and(|connections| {
        connections
            .iter()
            .any(|connection| connection.state == TrackerReadiness::Ready)
    })
}

pub enum RowKind {
    Project {
        project: Project,
    },
    GitIdentity(GitIdentity),
    Credential(Credential),
    Remote {
        address: String,
        remembered: Option<RemoteKey>,
        status: RemoteStatus,
        sync: Sync,
        trackers: Option<Vec<TrackerConnection>>,
        binding: Option<TrackerBinding>,
    },
    Discovered(DiscoveredRemote),
    Switch {
        setting: Setting,
        on: bool,
    },
}

pub(super) fn switch_row(setting: Setting, on: bool) -> Row {
    Row {
        label: setting.label().to_owned(),
        detail: Some(if on { "on" } else { "off" }.to_owned()),
        kind: RowKind::Switch { setting, on },
    }
}

impl Row {
    pub(super) fn dot(&self) -> Option<Dot> {
        match &self.kind {
            RowKind::Remote { status, sync, .. } => Some(remote_mark(status, *sync)),
            // The same question mark the first screen gives a machine found on
            // the network: it is an offer, not something this client holds.
            RowKind::Discovered(_) => Some(Dot::still('?', palette::muted())),
            RowKind::Credential(credential) => credential.dot(),
            RowKind::GitIdentity(identity) => Some(mark(identity.is_set(), false)),
            // A preference is the same kind of fact as a login or a remote: on,
            // or not. It reads with the same mark rather than a word of its own.
            RowKind::Switch { on, .. } => Some(mark(*on, false)),
            _ => None,
        }
    }

    pub fn is_configured_remote(&self) -> bool {
        matches!(self.kind, RowKind::Remote { .. })
    }

    pub fn is_removable(&self) -> bool {
        matches!(
            self.kind,
            RowKind::Project { .. }
                | RowKind::Remote { .. }
                | RowKind::GitIdentity(GitIdentity { saved: Some(_), .. })
        )
    }

    pub fn forgets_credentials(&self) -> bool {
        matches!(
            &self.kind,
            RowKind::Credential(Credential {
                kind: CredentialKind::Profile { .. } | CredentialKind::Tracker { .. },
                ..
            })
        )
    }

    pub fn profile(&self) -> Option<&str> {
        match &self.kind {
            RowKind::Credential(credential) => match &credential.kind {
                CredentialKind::Profile { profile, .. }
                | CredentialKind::Tracker { profile, .. } => Some(profile.as_str()),
                CredentialKind::Installation { .. } => None,
            },
            _ => None,
        }
    }

    pub fn hints(&self, connecting_tracker: bool) -> Vec<chrome::Hint> {
        match &self.kind {
            RowKind::Project { .. } => vec![chrome::Hint::new("enter", "edit")],
            RowKind::GitIdentity(identity) => {
                let mut hints = vec![
                    chrome::Hint::new("enter", "edit"),
                    chrome::Hint::new("r", "rename"),
                ];
                if identity.saved.is_some() {
                    hints.push(chrome::Hint::new("d", "delete"));
                }
                hints
            }
            RowKind::Credential(credential) => match &credential.kind {
                CredentialKind::Installation { .. } => {
                    vec![chrome::Hint::new("enter", "remote guide")]
                }
                CredentialKind::Profile { .. } if credential.is_pending() => {
                    vec![chrome::Hint::new("r", "rename")]
                }
                CredentialKind::Profile { .. } => vec![
                    chrome::Hint::new("enter", "sign in"),
                    chrome::Hint::new("r", "rename"),
                ],
                CredentialKind::Tracker { .. } => vec![
                    chrome::Hint::new(
                        "enter",
                        if connecting_tracker {
                            "cancel sign-in"
                        } else {
                            "sign in"
                        },
                    ),
                    chrome::Hint::new("r", "rename"),
                ],
            },
            RowKind::Remote {
                status, trackers, ..
            } => {
                let enter_action = match status {
                    RemoteStatus::Changed => "accept identity",
                    RemoteStatus::Incompatible { .. } => "check again",
                    _ => "connect",
                };
                let mut hints = vec![
                    chrome::Hint::new("enter", enter_action),
                    chrome::Hint::new("e", "edit"),
                ];
                if !tracker_is_set_up(trackers.as_deref())
                    && !matches!(status, RemoteStatus::Incompatible { .. })
                {
                    hints.insert(1, chrome::Hint::new("t", "set up guide"));
                }
                hints
            }
            RowKind::Discovered(remote) => {
                if remote.compatibility.is_compatible() {
                    vec![chrome::Hint::new("enter", "use remote")]
                } else {
                    Vec::new()
                }
            }
            RowKind::Switch { .. } => vec![chrome::Hint::new("enter", "toggle")],
        }
    }
}

pub struct View {
    pub rows: Vec<Row>,
    pub credentials: Vec<NamedCredential>,
    pub defaults: ProjectDefaults,
    pub tracker_states: HashMap<String, TrackerState>,
    pub base: Option<EnvironmentDescription>,
}

#[derive(Default)]
pub struct ProjectDefaults {
    pub agent: Option<String>,
    pub vcs: Option<String>,
    pub tracker: Option<String>,
    pub commits: Option<ProjectCommits>,
}

pub struct Credential {
    pub label: String,
    pub kind: CredentialKind,
    pub used_by: Vec<String>,
}

impl Credential {
    pub(in crate::tui) fn profile(&self) -> Option<&str> {
        match &self.kind {
            CredentialKind::Profile { profile, .. } | CredentialKind::Tracker { profile, .. } => {
                Some(profile.as_str())
            }
            CredentialKind::Installation { .. } => None,
        }
    }

    pub(in crate::tui) fn is_pending(&self) -> bool {
        matches!(
            &self.kind,
            CredentialKind::Profile {
                status: CredentialStatus::Unknown | CredentialStatus::Checking,
                ..
            } | CredentialKind::Tracker {
                status: TrackerState::NotConnected,
                ..
            }
        )
    }

    pub(super) fn dot(&self) -> Option<Dot> {
        match &self.kind {
            CredentialKind::Profile { status, .. } => Some(credential_mark(status)),
            CredentialKind::Tracker { status, .. } => Some(tracker_mark(*status)),
            CredentialKind::Installation { .. } => Some(Dot::still('■', palette::muted())),
        }
    }

    pub(super) fn summary(&self) -> &'static str {
        match &self.kind {
            CredentialKind::Profile { status, .. } => status.summary(),
            CredentialKind::Tracker { status, .. } => status.summary(),
            CredentialKind::Installation { .. } => "this machine",
        }
    }
}

pub enum CredentialKind {
    Installation {
        hostname: String,
        label: String,
        key: String,
        data_dir: String,
    },
    Tracker {
        profile: String,
        directory: String,
        status: TrackerState,
    },
    Profile {
        profile: String,
        directory: String,
        status: CredentialStatus,
    },
}

#[derive(Clone, Debug, Default)]
pub struct GitIdentity {
    pub label: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub signing_key: Option<String>,
    pub signs_commits: bool,
    pub used_by: Vec<String>,
    pub saved: Option<ProjectCommits>,
}

impl GitIdentity {
    pub(in crate::tui) fn is_set(&self) -> bool {
        self.name.is_some() && self.email.is_some()
    }
}

pub(super) fn mark(filled: bool, asking: bool) -> Dot {
    let (glyph, style) = match (asking, filled) {
        (true, _) => ('◌', palette::muted()),
        (false, true) => ('●', palette::primary()),
        (false, false) => ('○', palette::muted()),
    };
    if asking {
        Dot::travelling(glyph, style)
    } else {
        Dot::still(glyph, style)
    }
}

fn remote_mark(status: &RemoteStatus, sync: Sync) -> Dot {
    // What a remote is owed is the row's word, since a machine this one has
    // nothing to send is as still as it looks.
    match (status, sync) {
        (RemoteStatus::Reached, Sync::Configuration) => return mark(false, true),
        (RemoteStatus::Reached, Sync::NoProjects | Sync::UntilItsTaskEnds) => {
            return mark(false, false);
        }
        _ => {}
    }
    match status {
        RemoteStatus::Asking | RemoteStatus::Preparing(_) | RemoteStatus::Syncing => {
            mark(false, true)
        }
        RemoteStatus::Reached => mark(true, false),
        RemoteStatus::Unknown
        | RemoteStatus::Changed
        | RemoteStatus::Incompatible { .. }
        | RemoteStatus::Silent(_) => mark(false, false),
    }
}

fn credential_mark(status: &CredentialStatus) -> Dot {
    match status {
        CredentialStatus::Checking => mark(false, true),
        CredentialStatus::Checked {
            signed_in: true, ..
        } => mark(true, false),
        // A check that has not run and one that could not run are not answers, so
        // they stay hollow rather than claiming a sign-out.
        CredentialStatus::Saved => mark(true, false),
        CredentialStatus::Unknown
        | CredentialStatus::Unanswered
        | CredentialStatus::Checked {
            signed_in: false, ..
        } => mark(false, false),
    }
}

fn tracker_mark(state: TrackerState) -> Dot {
    match state {
        TrackerState::Connecting => mark(false, true),
        TrackerState::Connected => mark(true, false),
        TrackerState::NotConnected => mark(false, false),
    }
}
