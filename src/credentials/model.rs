use crate::{agent::AgentKind, tracker::TrackerKind, vcs::VcsProviderKind};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Agent,
    VersionControl,
    Tracker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Agent(AgentKind),
    VersionControl(VcsProviderKind),
    Tracker(TrackerKind),
}

impl Service {
    pub const ALL: [Self; 4] = [
        Self::Agent(AgentKind::Claude),
        Self::Agent(AgentKind::Pi),
        Self::VersionControl(VcsProviderKind::GitHub),
        Self::Tracker(TrackerKind::Linear),
    ];

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|service| service.owns(id))
    }

    // A credential id is the service's profile, either bare or with the operator's
    // own name under it: a settings file from before identities were named keeps
    // one login per service, and its id is the profile alone.
    pub fn owns(self, id: &str) -> bool {
        id == self.id()
            || id
                .strip_prefix(self.id())
                .is_some_and(|rest| rest.starts_with('/'))
    }

    // The profile a credential of this service is kept in, which is the id a
    // default credential of it takes and the whole of what tells two apart.
    pub fn id(self) -> &'static str {
        match self {
            Self::Agent(kind) => kind.profile(),
            Self::VersionControl(kind) => kind.profile(),
            Self::Tracker(kind) => kind.profile(),
        }
    }

    pub fn slot(self) -> Slot {
        match self {
            Self::Agent(_) => Slot::Agent,
            Self::VersionControl(_) => Slot::VersionControl,
            Self::Tracker(_) => Slot::Tracker,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Agent(kind) => kind.label(),
            Self::VersionControl(kind) => kind.label(),
            Self::Tracker(kind) => kind.label(),
        }
    }

    pub fn is_mounted(self) -> bool {
        !matches!(self, Self::Tracker(_))
    }

    // What the service is called once a label has to name one of its credentials:
    // the shortest word its id already ends in.
    pub fn slug(self) -> &'static str {
        self.id().rsplit('/').next().unwrap_or_default()
    }
}

pub fn describe(service: Service, group: Option<&str>, identity: &str) -> String {
    let name = service.slug();
    let identity = identity.trim();
    let identity = if identity.eq_ignore_ascii_case(name) {
        ""
    } else {
        identity
    };
    let group = group.map(str::trim).filter(|group| !group.is_empty());
    match (group, identity.is_empty()) {
        (Some(group), false) => format!("{name}:{group}/{identity}"),
        (Some(group), true) => format!("{name}:{group}"),
        (None, false) => format!("{name}:{identity}"),
        (None, true) => name.to_owned(),
    }
}

pub fn agent_of(credential: Option<&str>) -> Option<AgentKind> {
    match credential.and_then(Service::from_id) {
        Some(Service::Agent(kind)) => Some(kind),
        _ => None,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Credential {
    pub id: String,
    pub label: String,
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
    let base = service.id();
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
