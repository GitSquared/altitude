//! A credential is one named instance of one service: "Work Linear", "Claude
//! Work", "GitHub".
//!
//! The service is what knows how to sign in and check; the credential is what an
//! operator names, binds to a project, and sees in a list. The id encodes the
//! service because it also names the profile directory, travels in a task, and
//! has to be understood by a machine that has never seen this settings file.

use crate::{agent::AgentKind, tracker::TrackerKind, vcs::VcsProviderKind};
use serde::{Deserialize, Serialize};

/// Which slot of a project a credential can fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Agent,
    VersionControl,
    Tracker,
}

/// The external services a credential can belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Agent(AgentKind),
    VersionControl(VcsProviderKind),
    Tracker(TrackerKind),
}

impl Service {
    /// Every service this build can connect.
    pub const ALL: [Self; 4] = [
        Self::Agent(AgentKind::Claude),
        Self::Agent(AgentKind::Pi),
        Self::VersionControl(VcsProviderKind::GitHub),
        Self::Tracker(TrackerKind::Linear),
    ];

    /// The service a profile id names, with any instance suffix ignored.
    pub fn from_id(id: &str) -> Option<Self> {
        let mut parts = id.split('/');
        match (parts.next()?, parts.next()?) {
            ("agents", "claude") => Some(Self::Agent(AgentKind::Claude)),
            ("agents", "pi") => Some(Self::Agent(AgentKind::Pi)),
            ("vcs", "github") => Some(Self::VersionControl(VcsProviderKind::GitHub)),
            ("trackers", "linear") => Some(Self::Tracker(TrackerKind::Linear)),
            _ => None,
        }
    }

    pub fn slot(self) -> Slot {
        match self {
            Self::Agent(_) => Slot::Agent,
            Self::VersionControl(_) => Slot::VersionControl,
            Self::Tracker(_) => Slot::Tracker,
        }
    }

    /// What this service is called, independent of the credential.
    pub fn label(self) -> &'static str {
        match self {
            Self::Agent(AgentKind::Claude) => "Claude Code",
            Self::Agent(AgentKind::Pi) => "Pi",
            Self::VersionControl(VcsProviderKind::GitHub) => "GitHub",
            Self::Tracker(TrackerKind::Linear) => "Linear",
        }
    }

    /// Whether a task mounts this credential's directory, as opposed to the host
    /// using it as a session.
    pub fn is_mounted(self) -> bool {
        !matches!(self, Self::Tracker(_))
    }

    /// The id every installation starts with for this service.
    pub fn default_id(self) -> &'static str {
        match self {
            Self::Agent(AgentKind::Claude) => "agents/claude",
            Self::Agent(AgentKind::Pi) => "agents/pi",
            Self::VersionControl(VcsProviderKind::GitHub) => "vcs/github",
            Self::Tracker(TrackerKind::Linear) => "trackers/linear",
        }
    }
}

/// One named credential.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Credential {
    /// Stable, and the profile directory a task mounts.
    pub id: String,
    /// What an operator reads and binds by.
    pub label: String,
    /// Whether the label is still the one the service or the fallback chose.
    /// A rename turns this off, so a later account hint does not overwrite it.
    #[serde(default = "default_true")]
    pub label_auto: bool,
}

fn default_true() -> bool {
    true
}

impl Credential {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            label_auto: true,
        }
    }

    pub fn service(&self) -> Option<Service> {
        Service::from_id(&self.id)
    }

    /// Whether the id is usable as a profile directory and a route.
    pub fn is_valid(&self) -> bool {
        let id = self.id.trim();
        !id.is_empty()
            && id == self.id
            && id.len() <= 96
            && id.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            })
            && Service::from_id(id).is_some()
            && !self.label.trim().is_empty()
            && self.label == self.label.trim()
            && self.label.len() <= 64
            && !self.label.chars().any(char::is_control)
    }
}

/// Build the id for a new credential of `service`, using a slug of its label and
/// a short suffix when that is taken.
pub fn new_id(service: Service, label: &str, taken: impl Fn(&str) -> bool) -> String {
    let slug: String = label
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug = if slug.is_empty() { "identity" } else { &slug };
    let base = match service {
        Service::Agent(AgentKind::Claude) => "agents/claude",
        Service::Agent(AgentKind::Pi) => "agents/pi",
        Service::VersionControl(VcsProviderKind::GitHub) => "vcs/github",
        Service::Tracker(TrackerKind::Linear) => "trackers/linear",
    };
    let candidate = format!("{base}/{slug}");
    if !taken(&candidate) {
        return candidate;
    }
    for n in 2.. {
        let candidate = format!("{base}/{slug}-{n}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}
