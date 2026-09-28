mod github;

pub use github::GitHub;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VcsProviderKind {
    GitHub,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodeContribution {
    /// Provider-scoped stable identity. The provider adapter owns its format.
    pub id: String,
    pub provider: VcsProviderKind,
    pub host: String,
    pub repository: String,
    pub reference: String,
    pub url: String,
}

/// Provider-specific discovery and normalization; task history stores only the
/// provider-neutral contribution returned here.
pub trait VcsProvider {
    fn contribution_from_url(&self, url: &str) -> Option<CodeContribution>;
}
