mod github;

pub use github::GitHub;

use super::{VcsProvider, VcsProviderKind};

impl VcsProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
        }
    }

    pub fn profile(self) -> &'static str {
        match self {
            Self::GitHub => "vcs/github",
        }
    }

    pub fn purpose(self) -> &'static str {
        answering(self).purpose()
    }

    pub fn short_repository(self, repository: &str) -> Option<String> {
        answering(self).short_repository(repository)
    }

    pub fn account_markers(self) -> &'static [&'static str] {
        answering(self).account_markers()
    }

    pub fn login_files(self) -> &'static [&'static str] {
        answering(self).login_files()
    }

    pub fn signing_key_instructions(self) -> (&'static str, &'static str) {
        answering(self).signing_key_instructions()
    }
}

pub(super) fn answering(kind: VcsProviderKind) -> &'static dyn VcsProvider {
    match kind {
        VcsProviderKind::GitHub => &GitHub,
    }
}
