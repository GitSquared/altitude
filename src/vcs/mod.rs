pub(crate) mod listing;
mod providers;
pub mod tools;

use crate::isolation::ProfileRecipe;
use providers::answering;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VcsProviderKind {
    GitHub,
}

impl VcsProviderKind {
    pub const ALL: &[Self] = &[Self::GitHub];

    pub fn sole() -> Self {
        Self::ALL[0]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    pub name: String,
    pub url: String,
    pub is_private: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodeContribution {
    pub id: String,
    pub provider: VcsProviderKind,
    pub host: String,
    pub repository: String,
    pub reference: String,
    pub url: String,
}

pub trait VcsProvider: Send + Sync {
    fn contribution_from_url(&self, url: &str) -> Option<CodeContribution>;

    fn observation_command(&self, contributions: &[CodeContribution]) -> String;

    fn contribution_observations(
        &self,
        contributions: &[CodeContribution],
        output: &str,
    ) -> Option<Vec<ContributionObservation>>;

    fn profile_recipe(&self) -> ProfileRecipe;

    fn repository_list_command(&self) -> String;

    fn repositories(&self, output: &str) -> Option<Vec<Repository>>;

    fn short_repository(&self, repository: &str) -> Option<String>;

    // The lines a signed-in CLI writes that name the account, so the interface can
    // call a profile by the person it belongs to rather than by the host's name.
    fn account_markers(&self) -> &'static [&'static str];

    fn purpose(&self) -> &'static str;

    fn login_files(&self) -> &'static [&'static str];

    fn signing_key_instructions(&self) -> (&'static str, &'static str);
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeStanding {
    Merged,
    Closed,
    Draft,
    AwaitingReview,
    ChangesRequested,
    Approved,
    Clear,
    Blocked,
    Behind,
    Unstable,
    Conflicting,
    Unknown,
}

impl MergeStanding {
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContributionState {
    pub head_sha: String,
    pub checks_passing: u32,
    pub checks_failing: u32,
    pub checks_pending: u32,
    pub standing: MergeStanding,
    pub checks_settle_by_ms: i64,
    // The jobs that are not passing, by name: a count tells the agent something is
    // wrong, a name tells it where to look.
    #[serde(default)]
    pub failing_checks: Vec<String>,
    #[serde(default)]
    pub pending_checks: Vec<String>,
    // A review or a comment is news even when it changes no verdict: the agent
    // cannot answer feedback it was never told about. What was said is readable
    // where it was said, so only which one landed and who wrote it is kept.
    #[serde(default)]
    pub latest_review: Option<LatestReview>,
    #[serde(default)]
    pub comment_count: u32,
    #[serde(default)]
    pub latest_comment: Option<LatestComment>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LatestComment {
    pub id: String,
    pub author: Option<String>,
    pub is_bot: bool,
    pub at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LatestReview {
    pub verdict: String,
    pub author: Option<String>,
    // Whether a robot wrote it. A robot's review is addressed to the agent; a
    // person's is the operator's to read and answer, so the two are not the same
    // kind of news and only one of them is work.
    pub is_bot: bool,
}

impl LatestReview {
    pub fn needs_a_reply(&self) -> bool {
        self.is_bot && matches!(self.verdict.as_str(), "commented" | "changes requested")
    }

    pub fn label(&self) -> String {
        let who = match (&self.author, self.is_bot) {
            (Some(author), true) => format!(" by {author}, a bot"),
            (Some(author), false) => format!(" by {author}"),
            (None, true) => " by a bot".to_owned(),
            (None, false) => String::new(),
        };
        format!("{}{who}", self.verdict)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContributionReadiness {
    Pending,
    Ready,
    NeedsAction,
}

impl ContributionState {
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
            // A robot's review or comment is feedback the agent is supposed to act
            // on, and it asks for nothing from the operator.
            _ if self
                .latest_review
                .as_ref()
                .is_some_and(LatestReview::needs_a_reply)
                || self
                    .latest_comment
                    .as_ref()
                    .is_some_and(|comment| comment.is_bot) =>
            {
                ContributionReadiness::NeedsAction
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

pub struct ContributionOutlook {
    pub readiness: ContributionReadiness,
    // The ones the agent is told about, in the order they were linked.
    pub needing_action: Vec<ContributionObservation>,
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
                    needing_action.push(observation.clone());
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContributionObservation {
    pub contribution_id: String,
    pub state: ContributionState,
    pub summary: String,
}

pub fn contribution_from_url(url: &str) -> Option<CodeContribution> {
    VcsProviderKind::ALL
        .iter()
        .find_map(|kind| answering(*kind).contribution_from_url(url))
}

pub struct ProviderBatch {
    kind: VcsProviderKind,
    contributions: Vec<CodeContribution>,
}

impl ProviderBatch {
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

    pub fn command(&self) -> String {
        answering(self.kind).observation_command(&self.contributions)
    }

    pub fn observations(&self, output: &str) -> Option<Vec<ContributionObservation>> {
        answering(self.kind).contribution_observations(&self.contributions, output)
    }
}

pub fn profile_recipes() -> Vec<ProfileRecipe> {
    VcsProviderKind::ALL
        .iter()
        .map(|kind| answering(*kind).profile_recipe())
        .collect()
}

pub(crate) fn profile_recipe(kind: VcsProviderKind) -> ProfileRecipe {
    answering(kind).profile_recipe()
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default()
}
