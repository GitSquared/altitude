//! What a version control provider has to say, in terms the host decides in.
//!
//! A provider normalizes its own vocabulary into the types here: what it reports
//! about a contribution, and what stands between that contribution and its merge.
//! The host's readiness rule is written once against those, so a second provider
//! needs a normalization rather than a second rule.

mod github;
pub(crate) mod listing;
pub mod tools;

pub use github::GitHub;

use crate::isolation::ProfileRecipe;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VcsProviderKind {
    GitHub,
}

impl VcsProviderKind {
    /// Every kind this build knows, in the order a URL is offered to them.
    const ALL: &[Self] = &[Self::GitHub];

    /// The provider a repository field lists from while a build knows one.
    ///
    /// A project will name its version-control connection when a machine can hold
    /// more than one. Until then every repository is this one's, and saying so
    /// here keeps the assumption out of a caller indexing a list.
    pub fn sole() -> Self {
        Self::ALL[0]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
        }
    }

    /// The profile this provider's login lives in.
    ///
    /// The recipe a provider contributes repeats the name for the build, which is
    /// the one place it has to exist besides here.
    pub fn profile(self) -> &'static str {
        match self {
            Self::GitHub => "vcs/github",
        }
    }
}

/// A repository a provider's login can reach, as a picker lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    /// `owner/name`, which is how the provider names it.
    pub name: String,
    /// What a clone takes.
    pub url: String,
    pub is_private: bool,
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
pub trait VcsProvider: Send + Sync {
    fn contribution_from_url(&self, url: &str) -> Option<CodeContribution>;

    /// One shell command that reports the state of every contribution given.
    ///
    /// One command for the set rather than one per contribution: a host watching
    /// many reviews asks its provider once per look rather than once per pull
    /// request. It runs inside the task capsule, where the provider CLI and its
    /// credentials already live, so watching needs no configuration anywhere else.
    fn observation_command(&self, contributions: &[CodeContribution]) -> String;

    /// Read that command's stdout into one observation per contribution.
    ///
    /// `None` when any contribution cannot be read: a partial picture must not
    /// decide that a pull request is ready, and a provider that is unreachable
    /// must not be mistaken for a finished check. Pure, so a provider's
    /// normalization can be reasoned about without a capsule or a network.
    fn contribution_observations(
        &self,
        contributions: &[CodeContribution],
        output: &str,
    ) -> Option<Vec<ContributionObservation>>;

    /// What a sandbox has to install before this provider's commands can run.
    fn profile_recipe(&self) -> ProfileRecipe;

    /// What the provider's own CLI runs to list the repositories its login can
    /// reach.
    ///
    /// A command rather than a request of Altitude's own: the login belongs to
    /// the CLI, and the thing that can answer for it is the sandbox the CLI is
    /// installed in.
    fn repository_list_command(&self) -> String;

    /// Read that command's output into the repositories it listed, newest first.
    ///
    /// `None` when the answer cannot be read, so a malformed listing is a picker
    /// that says so rather than a list of half-read rows.
    fn repositories(&self, output: &str) -> Option<Vec<Repository>>;
}

/// What stands between a contribution and its merge.
///
/// Every provider answers this in its own words and maps them onto these, so the
/// rule that reads them is written once. Each variant names one situation rather
/// than one provider's spelling of it: the same standing has to mean the same
/// thing to the host whichever provider reported it.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeStanding {
    /// A person merged it.
    Merged,
    /// A person closed it without merging.
    Closed,
    /// A draft, not offered for review yet.
    Draft,
    /// Nobody has reviewed it, and a review is what is outstanding.
    AwaitingReview,
    /// A reviewer asked for changes.
    ChangesRequested,
    /// A reviewer approved it, and nothing else stands in the way.
    Approved,
    /// Nothing stands in the way, and no review is expected.
    Clear,
    /// Blocked by something the provider does not name.
    Blocked,
    /// The branch is out of date with its base.
    Behind,
    /// Checks have not settled.
    Unstable,
    /// The branch cannot be merged as it stands.
    Conflicting,
    /// The provider cannot say yet.
    Unknown,
}

impl MergeStanding {
    /// How this reads to the operator.
    ///
    /// The wording belongs to the standing rather than to a provider, so the same
    /// situation reads the same way whichever provider reported it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Draft => "draft",
            Self::AwaitingReview => "merge blocked · review required",
            Self::ChangesRequested => "merge blocked · review changes requested",
            Self::Approved => "mergeable · review approved",
            Self::Clear => "mergeable",
            Self::Blocked => "merge blocked",
            Self::Behind => "base branch update required",
            Self::Unstable => "merge checks not passing",
            Self::Conflicting => "merge conflict",
            Self::Unknown => "merge state unknown",
        }
    }
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
    pub standing: MergeStanding,
    /// When the provider's checks for this contribution should have appeared.
    ///
    /// A provider starts the checks a contribution requires in the seconds after
    /// it is created, and until they exist they are not in what the provider
    /// reports: an unstarted check and an absent check look the same. Until this
    /// moment, no checks reported is a contribution whose checks have not appeared
    /// yet rather than one that has none.
    pub checks_settle_by_ms: i64,
}

/// What a contribution's observed state means for the host watching it.
///
/// Provider-neutral: it is the interpretation the control loop acts on, not a
/// provider's own vocabulary, so watching a contribution asks the same question
/// of every provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContributionReadiness {
    /// Checks have not finished. There is nothing to decide yet.
    Pending,
    /// Checks finished and nothing needs the agent. The operator decides next.
    Ready,
    /// Something about the contribution needs the agent before a human review.
    NeedsAction,
}

impl ContributionState {
    /// Whether this state is still moving, ready for a human, or asking the agent
    /// to act.
    ///
    /// `Ready` needs a positive reason: checks are not running or failing, the
    /// contribution is not a draft, and its standing is one that says the review is
    /// what remains. `Unknown` is a wait, not a yes, because a provider computes
    /// mergeability asynchronously and a repository rule may block what it does not
    /// name.
    ///
    /// `now_ms` is the host's clock. It is passed in rather than read here because
    /// a contribution settles with time and a recorded state does not.
    pub fn readiness(&self, now_ms: i64) -> ContributionReadiness {
        match self.standing {
            // A contribution a person merged or closed is not the agent's to fix:
            // the operator ended it deliberately, and only they know why. It goes
            // to them rather than back to the agent, whatever the checks say.
            MergeStanding::Merged | MergeStanding::Closed => ContributionReadiness::Ready,
            // What the agent has to fix is the agent's now, whatever the checks
            // are still doing: it can start while they finish.
            MergeStanding::Draft
            | MergeStanding::ChangesRequested
            | MergeStanding::Blocked
            | MergeStanding::Behind
            | MergeStanding::Unstable
            | MergeStanding::Conflicting => ContributionReadiness::NeedsAction,
            // A failing check is the agent's too, whichever standing names it.
            _ if self.checks_failing > 0 => ContributionReadiness::NeedsAction,
            // Checks that are running, or that the provider has not had time to
            // start, are not checks that passed.
            _ if self.checks_pending > 0 || now_ms < self.checks_settle_by_ms => {
                ContributionReadiness::Pending
            }
            // Nothing is outstanding but a review, so the operator is the next step.
            MergeStanding::Approved | MergeStanding::Clear | MergeStanding::AwaitingReview => {
                ContributionReadiness::Ready
            }
            // A provider that cannot say is a wait, not a yes: it computes
            // mergeability asynchronously and a repository rule may block what it
            // does not name.
            MergeStanding::Unknown => ContributionReadiness::Pending,
        }
    }
}

/// What every linked contribution of one task says together.
///
/// The worst state wins: one contribution still running keeps the task waiting,
/// and one that needs the agent sends it back whatever the others say. Deciding
/// this is about the contributions rather than about the host that asked, which
/// is why it lives here beside the vocabulary it reads.
pub struct ContributionOutlook {
    pub readiness: ContributionReadiness,
    /// The contributions that need the agent, in the operator's words. Empty
    /// unless the readiness is [`ContributionReadiness::NeedsAction`].
    pub needing_action: Vec<String>,
}

impl ContributionOutlook {
    pub fn of(observations: &[ContributionObservation], now_ms: i64) -> Self {
        let mut readiness = ContributionReadiness::Ready;
        let mut needing_action = Vec::new();
        for observation in observations {
            match observation.state.readiness(now_ms) {
                ContributionReadiness::Pending => {
                    if readiness == ContributionReadiness::Ready {
                        readiness = ContributionReadiness::Pending;
                    }
                }
                ContributionReadiness::NeedsAction => {
                    readiness = ContributionReadiness::NeedsAction;
                    needing_action.push(observation.summary.clone());
                }
                ContributionReadiness::Ready => {}
            }
        }
        Self {
            readiness,
            needing_action,
        }
    }
}

/// What a provider reported about one contribution.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContributionObservation {
    /// [`CodeContribution::id`] this is about.
    pub contribution_id: String,
    pub state: ContributionState,
    /// One operator-facing line, e.g. `ci 1 failing, 2 passing · merge blocked ·
    /// review changes requested`.
    pub summary: String,
}

/// The contribution a URL names, if any provider claims it.
pub fn contribution_from_url(url: &str) -> Option<CodeContribution> {
    VcsProviderKind::ALL
        .iter()
        .find_map(|kind| answering(*kind).contribution_from_url(url))
}

/// Some of a task's contributions, and the provider that answers for them.
///
/// Grouped by provider, because a contribution is asked about in the vocabulary of
/// the provider it came from: a set spanning providers is one question each, and
/// one provider's command cannot report another's contribution.
pub struct ProviderBatch {
    kind: VcsProviderKind,
    contributions: Vec<CodeContribution>,
}

impl ProviderBatch {
    /// The batches every contribution given falls into.
    pub fn of(contributions: &[CodeContribution]) -> Vec<Self> {
        VcsProviderKind::ALL
            .iter()
            .filter_map(|kind| {
                let theirs: Vec<CodeContribution> = contributions
                    .iter()
                    .filter(|contribution| contribution.provider == *kind)
                    .cloned()
                    .collect();
                (!theirs.is_empty()).then_some(Self {
                    kind: *kind,
                    contributions: theirs,
                })
            })
            .collect()
    }

    /// The shell command that reports every contribution in this batch.
    pub fn command(&self) -> String {
        answering(self.kind).observation_command(&self.contributions)
    }

    /// What that command reported, one observation per contribution in the batch.
    ///
    /// `None` when any of them could not be read.
    pub fn observations(&self, output: &str) -> Option<Vec<ContributionObservation>> {
        answering(self.kind).contribution_observations(&self.contributions, output)
    }
}

/// What a sandbox has to install to read every provider this build knows.
///
/// A provider is read through its own command, so the sandbox the read runs in has
/// to have it: this is what a capsule and a watcher both need before either can
/// watch anything.
pub fn profile_recipes() -> Vec<ProfileRecipe> {
    VcsProviderKind::ALL
        .iter()
        .map(|kind| answering(*kind).profile_recipe())
        .collect()
}

/// The provider that answers for one kind of contribution.
fn answering(kind: VcsProviderKind) -> &'static dyn VcsProvider {
    match kind {
        VcsProviderKind::GitHub => &GitHub,
    }
}

/// The host's clock, in the unit a contribution's own timestamps are compared in.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default()
}
