//! What a row is, and what it says about itself: the mark it wears, the keys it
//! offers, and the facts the rest of the panel asks it for.

use std::path::PathBuf;

use crate::{
    agent::AgentKind,
    isolation::EnvironmentDescription,
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
/// own, its name, and a note at the right edge where there is more to say about
/// it than the name. Only the name is always there.
pub struct Row {
    /// What the list shows: a name where there is one, an address where there is
    /// not yet.
    pub label: String,
    /// Said at the right edge, so a column of notes lines up rather than each
    /// one sitting wherever its name happens to end.
    pub note: Option<&'static str>,
    pub kind: RowKind,
}

pub enum RowKind {
    /// The built-in environment: described, never edited.
    Base,
    Environment {
        /// The name as configured, which is what an edit replaces.
        name: String,
        setup: String,
        is_default: bool,
    },
    /// A coding agent, which may be the one new tasks use, and the login a task
    /// drives it with.
    Agent {
        kind: AgentKind,
        credential: Credential,
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
    /// A tracker Altitude can bind an issue to. Today that is Linear, and the
    /// row says whether its live session is connected.
    Tracker {
        state: TrackerState,
    },
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
        note: None,
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
            RowKind::Agent { credential, .. } => credential.dot(),
            RowKind::Credential(credential) => credential.dot(),
            RowKind::GitIdentity(identity) => Some(mark(identity.is_set(), false)),
            RowKind::Tracker { state } => Some(tracker_mark(*state)),
            // A preference is the same kind of fact as a login or a remote: on,
            // or not. It reads with the same mark rather than a word of its own.
            RowKind::Switch { on, .. } => Some(mark(*on, false)),
            _ => None,
        }
    }

    /// Whether this row opens a form.
    ///
    /// The built-in base does not: there is no form behind it and nothing to
    /// configure, so selecting it is already as far as it goes, and its
    /// description is shown without a second step. A credential and a git
    /// identity are acted on with enter rather than edited here.
    pub fn opens_form(&self) -> bool {
        matches!(
            self.kind,
            RowKind::Environment { .. } | RowKind::Remote { .. }
        )
    }

    /// Whether this row is a coding agent.
    pub fn is_agent(&self) -> bool {
        matches!(self.kind, RowKind::Agent { .. })
    }

    /// Whether this row is a remote this client has configured.
    pub fn is_configured_remote(&self) -> bool {
        matches!(self.kind, RowKind::Remote { .. })
    }

    /// Whether this row can be taken out of its list.
    ///
    /// The base environment and a discovered remote cannot: one is built in,
    /// and the other is not this client's to remove until it is adopted.
    pub fn is_removable(&self) -> bool {
        matches!(
            self.kind,
            RowKind::Environment { .. } | RowKind::Remote { .. }
        )
    }

    /// Whether delete here removes a saved sign-in rather than the row itself.
    pub fn forgets_credentials(&self) -> bool {
        matches!(self.kind, RowKind::Agent { .. } | RowKind::Tracker { .. })
            || matches!(
                &self.kind,
                RowKind::Credential(Credential {
                    kind: CredentialKind::Profile { .. },
                    ..
                })
            )
    }

    /// The profile whose login this row stands for, when it stands for one.
    pub fn profile(&self) -> Option<&'static str> {
        match &self.kind {
            RowKind::Agent { credential, .. } | RowKind::Credential(credential) => {
                match &credential.kind {
                    CredentialKind::Profile { profile, .. } => Some(profile),
                    CredentialKind::Installation { .. } => None,
                }
            }
            _ => None,
        }
    }

    /// The footer hints this row offers, beyond moving and changing columns.
    ///
    /// They live with the row rather than with the section, so the keys that
    /// work and the keys that are shown are described in one place.
    pub fn hints(&self, connecting_tracker: bool) -> Vec<chrome::Hint> {
        match &self.kind {
            RowKind::Base | RowKind::Environment { .. } => vec![chrome::Hint::new("enter", "edit")],
            RowKind::Agent { credential, .. } => {
                let mut hints = Vec::new();
                if !credential.is_pending() {
                    hints.push(chrome::Hint::new("enter", "sign in"));
                }
                hints.push(chrome::Hint::new("space", "set as default"));
                hints
            }
            RowKind::GitIdentity(_) => vec![chrome::Hint::new("enter", "open setup shell")],
            RowKind::Credential(credential) => match &credential.kind {
                CredentialKind::Installation { .. } => {
                    vec![chrome::Hint::new("enter", "remote guide")]
                }
                CredentialKind::Profile { .. } if credential.is_pending() => Vec::new(),
                CredentialKind::Profile { .. } => {
                    vec![chrome::Hint::new("enter", "open setup shell")]
                }
            },
            RowKind::Remote { .. } => vec![
                chrome::Hint::new("enter", "connect"),
                chrome::Hint::new("t", "task trackers"),
                chrome::Hint::new("b", "pair Linear"),
                chrome::Hint::new("e", "edit"),
            ],
            RowKind::Discovered(_) => vec![chrome::Hint::new("enter", "use remote")],
            RowKind::Tracker { .. } => vec![chrome::Hint::new(
                "enter",
                if connecting_tracker {
                    "cancel sign-in"
                } else {
                    "sign in"
                },
            )],
            RowKind::Switch { .. } => vec![chrome::Hint::new("enter", "toggle")],
        }
    }
}

/// Everything the panel draws, assembled by the caller from settings, the
/// remembered keys, and the built environments.
pub struct View {
    pub rows: Vec<Row>,
    /// What the base environment is made of, once it has been built here.
    pub base: Option<EnvironmentDescription>,
    /// Where Linear keeps its sign-in profile, for the Trackers page.
    pub tracker_profile_dir: PathBuf,
}

/// One credential, as the list shows it.
pub struct Credential {
    pub label: String,
    pub kind: CredentialKind,
}

impl Credential {
    pub(in crate::tui) fn is_pending(&self) -> bool {
        matches!(
            &self.kind,
            CredentialKind::Profile {
                status: CredentialStatus::Unknown | CredentialStatus::Checking,
                ..
            }
        )
    }

    /// What a profile's own check last said, as a mark in front of its row. The
    /// installation's key is not signed in to, so it has nothing to say.
    pub(super) fn dot(&self) -> Option<Dot> {
        match &self.kind {
            CredentialKind::Profile { status, .. } => Some(credential_mark(status)),
            CredentialKind::Installation { .. } => None,
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
    /// A profile a task mounts. Presence and place only: never the contents.
    Profile {
        profile: &'static str,
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
    pub(super) fn is_set(&self) -> bool {
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
        // Everything that is not a confirmed sign-in reads as not signed in, so
        // a failed check and a check that has not run look the same here.
        CredentialStatus::Unknown
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
