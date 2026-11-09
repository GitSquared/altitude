pub(crate) mod linear;

use std::{path::Path, sync::Arc};

use super::{TrackerKind, TrackerProvider};

impl TrackerKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
        }
    }

    pub fn profile(self) -> &'static str {
        match self {
            Self::Linear => "trackers/linear",
        }
    }

    pub fn scope_noun(self) -> &'static str {
        match self {
            Self::Linear => "team",
        }
    }

    pub fn purpose(self) -> &'static str {
        "A task tracking and product management tool. Altitude reads your issues and updates their status as it works on them."
    }

    pub fn looks_like_issue(self, value: &str) -> bool {
        match self {
            Self::Linear => linear::looks_like_issue(value),
        }
    }
}

pub(crate) fn provider(kind: TrackerKind, directory: &Path) -> Option<Arc<dyn TrackerProvider>> {
    match kind {
        TrackerKind::Linear => linear::Linear::from_directory(directory)
            .ok()
            .map(|tracker| Arc::new(tracker) as Arc<dyn TrackerProvider>),
    }
}
