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

    /// Shell command that reports a contribution's current state.
    ///
    /// It is run inside the task capsule, where the provider CLI and its
    /// credentials already live, so watching a contribution needs no
    /// configuration on the remote side.
    fn observation_command(&self, contribution: &CodeContribution) -> String;

    /// Read that command's stdout into an observation.
    ///
    /// Pure, so a provider's normalization can be reasoned about without a
    /// capsule or a network.
    fn contribution_observation(
        &self,
        contribution: &CodeContribution,
        output: &str,
    ) -> Option<ContributionObservation>;
}

/// The parts of a contribution's state that decide whether there is something
/// new to record.
///
/// Compared with the latest stored observation to suppress unchanged polls.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContributionState {
    /// Commit the state applies to. Feedback on an older commit stays
    /// distinguishable from feedback on the current head of the branch.
    pub head_sha: String,
    pub checks_passing: u32,
    pub checks_failing: u32,
    pub checks_pending: u32,
    /// Normalized review verdict: `approved`, `changes_requested`,
    /// `review_required`, or `none`.
    pub review: String,
}

/// What a provider reported about one contribution.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContributionObservation {
    /// [`CodeContribution::id`] this is about.
    pub contribution_id: String,
    pub state: ContributionState,
    /// One operator-facing line, e.g. `ci 1 failing, 2 passing · review changes
    /// requested`.
    pub summary: String,
}
