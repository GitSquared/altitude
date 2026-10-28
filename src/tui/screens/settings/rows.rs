//! What a row is, and what it says about itself: the mark it wears, the keys it
//! offers, and the facts the rest of the panel asks it for.

use crate::{
    credentials::Credential as NamedCredential,
    isolation::EnvironmentDescription,
    project::Project,
    remote::{DiscoveredRemote, RemoteKey, TrackerConnection},
    tui::{
        app::{CredentialStatus, RemoteStatus, TrackerState},
        widgets::{chrome, palette, utils::Dot},
    },
};

use super::panel::Setting;

/// One row of the focused list.
///
/// A row is three places: a mark at the left edge where it has a status of its
/// own, its name, and a line under it where there is more to say about it than
/// the name. Only the name is always there.
pub struct Row {
    /// What the list shows: a name where there is one, an address where there is
    /// not yet.
    pub label: String,
    /// Said under the name: whether it is signed in, which one is the default,
    /// or where a remote lives. A line rather than a note at the right edge, so
    /// a long status does not have to fit beside a long name.
    pub detail: Option<String>,
    pub kind: RowKind,
}

pub enum RowKind {
    Project {
        project: Project,
    },
    /// The identity git commits under, read from the version-control profile.
    GitIdentity(GitIdentity),
    /// Something a task logs in with: this installation's key, or a login a task
    /// is given.
    Credential(Credential),
    Remote {
        address: String,
        /// What that address last answered with, if it has been met.
        remembered: Option<RemoteKey>,
        /// What it is doing now: the list says so without being asked.
        status: RemoteStatus,
        /// Whether the configuration it holds is the one this client has.
        needs_config: bool,
        /// What its last announcement said its shared trackers are, when it has
        /// answered. Absent while it has not, which is unknown rather than empty.
        trackers: Option<Vec<TrackerConnection>>,
    },
    /// A remote announcing on the network that this client has not adopted.
    Discovered(DiscoveredRemote),
    Switch {
        setting: Setting,
        on: bool,
    },
}

/// A row for a preference that is on or off.
///
/// The label, the mark, and the third column's page all come from the setting,
/// so a new preference is added in one line rather than in four places.
pub(super) fn switch_row(setting: Setting, on: bool) -> Row {
    Row {
        label: setting.label().to_owned(),
        detail: Some(if on { "on" } else { "off" }.to_owned()),
        kind: RowKind::Switch { setting, on },
    }
}

impl Row {
    /// The mark in front of this row, where what it says is asked rather than
    /// known: a remote's address, a profile's login, a git identity, or whether
    /// a preference is on.
    pub(super) fn dot(&self) -> Option<Dot> {
        match &self.kind {
            RowKind::Remote {
                status: RemoteStatus::Reached,
                needs_config: true,
                ..
            } => Some(mark(false, true)),
            RowKind::Remote { status, .. } => Some(remote_mark(status)),
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

    /// Whether this row is a remote this client has configured.
    pub fn is_configured_remote(&self) -> bool {
        matches!(self.kind, RowKind::Remote { .. })
    }

    /// Whether this row can be taken out of its list.
    ///
    /// A discovered remote cannot: it is not this client's to remove until it is
    /// adopted. The last project cannot either, which the panel decides because
    /// that is a fact about the list rather than about the row.
    pub fn is_removable(&self) -> bool {
        matches!(self.kind, RowKind::Project { .. } | RowKind::Remote { .. })
    }

    /// Whether delete here removes a saved sign-in rather than the row itself.
    pub fn forgets_credentials(&self) -> bool {
        matches!(
            &self.kind,
            RowKind::Credential(Credential {
                kind: CredentialKind::Profile { .. } | CredentialKind::Tracker { .. },
                ..
            })
        )
    }

    /// The profile whose login this row stands for, when it stands for one.
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

    /// The footer hints this row offers, beyond moving and changing columns.
    ///
    /// They live with the row rather than with the section, so the keys that
    /// work and the keys that are shown are described in one place.
    pub fn hints(&self, connecting_tracker: bool) -> Vec<chrome::Hint> {
        match &self.kind {
            RowKind::Project { .. } => vec![chrome::Hint::new("enter", "edit")],
            RowKind::GitIdentity(_) => vec![chrome::Hint::new("enter", "sign in")],
            RowKind::Credential(credential) => match &credential.kind {
                CredentialKind::Installation { .. } => {
                    vec![chrome::Hint::new("enter", "remote guide")]
                }
                CredentialKind::Profile { .. } if credential.is_pending() => {
                    vec![chrome::Hint::new("e", "rename")]
                }
                CredentialKind::Profile { .. } => vec![
                    chrome::Hint::new("enter", "sign in"),
                    chrome::Hint::new("e", "rename"),
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
                    chrome::Hint::new("e", "rename"),
                ],
            },
            RowKind::Remote { status, .. } => vec![
                chrome::Hint::new(
                    "enter",
                    if *status == RemoteStatus::Changed {
                        "accept identity"
                    } else {
                        "connect"
                    },
                ),
                chrome::Hint::new("t", "task trackers"),
                chrome::Hint::new("e", "edit"),
            ],
            RowKind::Discovered(_) => vec![chrome::Hint::new("enter", "use remote")],
            RowKind::Switch { .. } => vec![chrome::Hint::new("enter", "toggle")],
        }
    }
}

/// Everything the panel draws, assembled by the caller from settings, the
/// remembered keys, and what the built base image is made of.
pub struct View {
    pub rows: Vec<Row>,
    /// The named identities the pages can label a project's bindings with.
    pub credentials: Vec<NamedCredential>,
    /// What the base image is made of, once it has been built here.
    pub base: Option<EnvironmentDescription>,
}

/// One credential, as the list shows it.
pub struct Credential {
    pub label: String,
    pub kind: CredentialKind,
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

    /// The mark in front of a credential's row. A profile's says what its own
    /// check last said. This installation is signed in to nothing, so its mark
    /// says which machine it is instead: a filled square, which reads at a
    /// glance where a small house does not.
    pub(super) fn dot(&self) -> Option<Dot> {
        match &self.kind {
            CredentialKind::Profile { status, .. } => Some(credential_mark(status)),
            CredentialKind::Tracker { status, .. } => Some(tracker_mark(*status)),
            CredentialKind::Installation { .. } => Some(Dot::still('■', palette::muted())),
        }
    }

    /// Whether this row is signed in, saved, or still being asked about.
    ///
    /// The install is not signed in to anything; it is the machine itself, so it
    /// says that rather than a status it does not have.
    pub(super) fn summary(&self) -> &'static str {
        match &self.kind {
            CredentialKind::Profile { status, .. } => status.summary(),
            CredentialKind::Tracker { status, .. } => status.summary(),
            CredentialKind::Installation { .. } => "this machine",
        }
    }
}

pub enum CredentialKind {
    /// This installation: the machine it runs on, the key that names it, and
    /// where its state lives.
    Installation {
        hostname: String,
        label: String,
        key: String,
        data_dir: String,
    },
    /// A host-side tracker identity: a live session, not a mount.
    Tracker {
        profile: String,
        directory: String,
        status: TrackerState,
    },
    /// A profile a task mounts. Presence and place only: never the contents.
    Profile {
        profile: String,
        directory: String,
        /// What that profile's own check said about it, if it has been asked.
        status: CredentialStatus,
    },
}

/// The identity and signing a task's git commits use.
///
/// Read from the version-control profile's git config, which every task using
/// that profile shares. An absent field is one git has no value for, which is
/// what a commit under a missing name or an unsigned commit looks like.
#[derive(Clone, Debug, Default)]
pub struct GitIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
    pub signing_key: Option<String>,
    pub signs_commits: bool,
}

impl GitIdentity {
    pub(in crate::tui) fn is_set(&self) -> bool {
        self.name.is_some() && self.email.is_some()
    }
}

/// The status vocabulary of this panel: one glyph for an answer that came back,
/// one for an answer that did not, and one that travels while the answer is
/// still out.
///
/// The colour is the terminal's own rather than green or red, so no status here
/// claims a verdict the row has not earned: a filled mark is white, and an empty
/// one is dim.
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

fn remote_mark(status: &RemoteStatus) -> Dot {
    match status {
        RemoteStatus::Asking | RemoteStatus::Syncing => mark(false, true),
        RemoteStatus::Reached => mark(true, false),
        RemoteStatus::Unknown | RemoteStatus::Changed | RemoteStatus::Silent(_) => {
            mark(false, false)
        }
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
