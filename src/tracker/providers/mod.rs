pub(crate) mod github;
pub(crate) mod linear;

use std::{path::Path, sync::Arc};

use super::{Guide, TrackerKind, TrackerProvider};

impl TrackerKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
            Self::GitHub => "GitHub",
        }
    }

    pub fn profile(self) -> &'static str {
        match self {
            Self::Linear => "trackers/linear",
            Self::GitHub => "trackers/github",
        }
    }

    pub fn scope_noun(self) -> &'static str {
        match self {
            Self::Linear => "team",
            Self::GitHub => "repository",
        }
    }

    pub fn purpose(self) -> &'static str {
        match self {
            Self::Linear => {
                "A task tracking and product management tool. altitude reads your issues and \
                 updates their status as it works on them."
            }
            Self::GitHub => {
                "GitHub Issues as a task tracker. altitude reads your issues and moves their \
                 status:in-progress and status:in-review labels as it works on them. It signs \
                 in with the GitHub CLI's own login."
            }
        }
    }

    pub fn looks_like_issue(self, value: &str) -> bool {
        match self {
            Self::Linear => linear::looks_like_issue(value),
            Self::GitHub => github::looks_like_issue(value),
        }
    }
}

impl TrackerKind {
    pub(crate) fn guide(self) -> &'static Guide {
        match self {
            Self::Linear => &linear::setup::LINEAR,
            Self::GitHub => &github::setup::GITHUB,
        }
    }
}

pub(crate) fn provider(kind: TrackerKind, directory: &Path) -> Option<Arc<dyn TrackerProvider>> {
    match kind {
        TrackerKind::Linear => linear::Linear::from_directory(directory)
            .ok()
            .map(|tracker| Arc::new(tracker) as Arc<dyn TrackerProvider>),
        TrackerKind::GitHub => github::GitHub::from_directory(directory)
            .ok()
            .map(|tracker| Arc::new(tracker) as Arc<dyn TrackerProvider>),
    }
}
