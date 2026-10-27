use serde::Deserialize;
use std::collections::HashMap;

use crate::{
    isolation::{GuestPath, PersistentDirectory, ProfileRecipe, SignInCommand, shell_quote},
    vcs::{
        CodeContribution, ContributionObservation, ContributionState, MergeStanding, Repository,
        VcsProvider, VcsProviderKind,
    },
};

/// What this profile installs: git and the tools that give it a GitHub
/// identity, and then the git a task finds in its `PATH` — Altitude's own, which
/// clones through this machine's shared object cache instead of pulling a
/// repository the machine already has. See `assets/git-shim.sh`.
const SETUP_COMMAND: &str = concat!(
    "apk add --no-cache git openssh-client-default github-cli gnupg\n",
    "cat > /usr/local/bin/git <<'ALTITUDE_GIT_SHIM'\n",
    include_str!("assets/git-shim.sh"),
    "ALTITUDE_GIT_SHIM\n",
    "chmod 0755 /usr/local/bin/git\n",
);

pub struct GitHub;

impl VcsProvider for GitHub {
    fn profile_recipe(&self) -> ProfileRecipe {
        ProfileRecipe {
            profile: "vcs/github",
            display: "GitHub",
            sign_in: "Follow the GitHub CLI's sign-in instructions.",
            // The one profile whose sign-in is a command of its own: it finishes,
            // and the terminal with it.
            sign_in_command: SignInCommand::Direct("exec gh auth login"),
            // The one profile whose CLI can be asked, and answers with the account and
            // the token's scopes.
            status_command: "gh auth status",
            label: "installing GitHub tools",
            setup_command: SETUP_COMMAND,
            directories: vec![
                PersistentDirectory::at(
                    "git",
                    GuestPath::Absolute("/altitude/git"),
                    include_str!("assets/git.md"),
                ),
                PersistentDirectory::at(
                    "ssh",
                    GuestPath::HomeRelative(".ssh"),
                    include_str!("assets/ssh.md"),
                ),
                PersistentDirectory::at(
                    "gh",
                    GuestPath::HomeRelative(".config/gh"),
                    include_str!("assets/github.md"),
                ),
                PersistentDirectory::at(
                    "gnupg",
                    GuestPath::HomeRelative(".gnupg"),
                    include_str!("assets/gnupg.md"),
                ),
            ],
            // A directory mount lets `git config --global` atomically replace its file.
            environment: vec![("GIT_CONFIG_GLOBAL", "/altitude/git/config")],
        }
    }

    fn repository_list_command(&self) -> String {
        // One request for the set, with the fields a picker reads. `pushedAt` is
        // asked for only to order the list: the repository an operator means is
        // almost always one they touched recently.
        "gh repo list --limit 200 --json nameWithOwner,url,isPrivate,pushedAt".to_owned()
    }

    fn repositories(&self, output: &str) -> Option<Vec<Repository>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Row {
            name_with_owner: String,
            url: String,
            is_private: bool,
            #[serde(default)]
            pushed_at: String,
        }
        let mut rows: Vec<Row> = serde_json::from_str(output).ok()?;
        // ISO-8601 timestamps order as text, and the provider's own order is not
        // promised.
        rows.sort_by(|a, b| b.pushed_at.cmp(&a.pushed_at));
        Some(
            rows.into_iter()
                .map(|row| Repository {
                    name: row.name_with_owner,
                    url: row.url,
                    is_private: row.is_private,
                })
                .collect(),
        )
    }

    fn contribution_from_url(&self, value: &str) -> Option<CodeContribution> {
        let url = url::Url::parse(value).ok()?;
        let host = url.host_str()?;
        if url.scheme() != "https" || host != "github.com" {
            return None;
        }
        let parts: Vec<_> = url
            .path_segments()?
            .filter(|part| !part.is_empty())
            .collect();
        // GitHub appends view segments to the same resource (`/files`,
        // `/commits`, `/checks`), and a reported URL may carry them. They do not
        // identify a different contribution, so accept and ignore the tail.
        let [owner, repository, "pull", number, ..] = parts.as_slice() else {
            return None;
        };
        // Normalize the number rather than echoing it: `/pull/007` and `/pull/7`
        // are one contribution, so they must produce one identity.
        let number = number.parse::<u64>().ok()?;
        if number == 0 {
            return None;
        }
        let repository = format!("{owner}/{repository}").to_ascii_lowercase();
        let canonical_url = format!("https://{host}/{repository}/pull/{number}");
        Some(CodeContribution {
            id: canonical_url.clone(),
            provider: VcsProviderKind::GitHub,
            host: host.to_owned(),
            repository,
            reference: number.to_string(),
            url: canonical_url,
        })
    }

    fn observation_command(&self, contributions: &[CodeContribution]) -> String {
        // One request for the set rather than one per contribution: `gh pr view`
        // takes a single pull request, so a host watching many reviews would
        // otherwise spend one process and one request on each of them. `resource`
        // resolves a pull request from the URL that already identifies it here, so
        // the query carries the same identity the task stores.
        //
        // `gh` resolves the token from the capsule's persistent gh config, so
        // watching costs the operator no setup beyond authenticating the CLI.
        let mut query = String::from("query {\n");
        for (index, contribution) in contributions.iter().enumerate() {
            query.push_str(&format!(
                "  c{index}: resource(url: {}) {{\n{CONTRIBUTION_FIELDS}\n  }}\n",
                graphql_string(&contribution.url)
            ));
        }
        query.push('}');
        format!("gh api graphql -f query={}", shell_quote(&query))
    }

    fn contribution_observations(
        &self,
        contributions: &[CodeContribution],
        output: &str,
    ) -> Option<Vec<ContributionObservation>> {
        let response: ContributionQuery = serde_json::from_str(output).ok()?;
        let resources = response.data?;
        let mut observations = Vec::with_capacity(contributions.len());
        for (index, contribution) in contributions.iter().enumerate() {
            // A contribution the provider cannot resolve answers null, which is
            // not a state to decide anything from.
            let view = resources.get(&format!("c{index}"))?.as_ref()?;
            observations.push(observation_of(contribution, view));
        }
        Some(observations)
    }
}

/// The fields every contribution is read from.
///
/// A pull request is asked about through `resource`, so the fragment carries what
/// the host decides on and nothing else.
const CONTRIBUTION_FIELDS: &str = r#"    ... on PullRequest {
      headRefOid
      createdAt
      reviewDecision
      mergeable
      mergeStateStatus
      isDraft
      state
      statusCheckRollup {
        contexts(first: 100) {
          nodes {
            ... on CheckRun { conclusion }
            ... on StatusContext { state }
          }
        }
      }
    }"#;

/// One GraphQL string literal.
///
/// The URL comes from one an agent supplied, so it is quoted rather than trusted
/// to hold nothing that would end the literal.
fn graphql_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The answer to one contribution query, keyed by the alias its contribution got.
#[derive(Deserialize)]
struct ContributionQuery {
    data: Option<HashMap<String, Option<PullRequestView>>>,
}

/// What one pull request looks like, as the host reads it.
fn observation_of(
    contribution: &CodeContribution,
    view: &PullRequestView,
) -> ContributionObservation {
    let mut checks_passing = 0;
    let mut checks_failing = 0;
    let mut checks_pending = 0;
    for check in view.status_check_rollup.as_deref().unwrap_or_default() {
        match check.outcome() {
            CheckOutcome::Passing => checks_passing += 1,
            CheckOutcome::Failing => checks_failing += 1,
            CheckOutcome::Pending => checks_pending += 1,
        }
    }
    let standing = merge_standing(view);
    ContributionObservation {
        contribution_id: contribution.id.clone(),
        summary: format!(
            "{} · {}",
            check_summary(checks_failing, checks_pending, checks_passing),
            standing.label(),
        ),
        state: ContributionState {
            head_sha: view.head_ref_oid.clone(),
            checks_passing,
            checks_failing,
            checks_pending,
            standing,
            checks_settle_by_ms: view
                .created_at
                .as_deref()
                .and_then(created_at_ms)
                .map(|created| created + CHECKS_APPEAR_WITHIN_MS)
                .unwrap_or_default(),
        },
    }
}

/// The subset of `gh pr view --json` this provider reads.
#[derive(Deserialize)]
struct PullRequestView {
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    /// When the pull request was opened, which is what its checks are timed from.
    #[serde(rename = "createdAt", default)]
    created_at: Option<String>,
    #[serde(rename = "reviewDecision", default)]
    review_decision: Option<String>,
    /// Absent or null when a pull request has no checks at all.
    #[serde(rename = "statusCheckRollup", default)]
    status_check_rollup: Option<Vec<CheckRollupEntry>>,
    #[serde(default)]
    mergeable: Option<String>,
    #[serde(rename = "mergeStateStatus", default)]
    merge_state_status: Option<String>,
    #[serde(rename = "isDraft", default)]
    is_draft: Option<bool>,
    #[serde(default)]
    state: Option<String>,
}

/// How long GitHub may take to start a contribution's checks.
///
/// A workflow begins in the seconds after the pull request event, and until its
/// check runs exist nothing in the pull request reports them. Waiting this long
/// costs a review nothing in the ordinary case — an agent opens a pull request,
/// keeps working, and asks for review minutes later — and in the unusual case it
/// is the difference between seeing a failing check and never seeing it at all.
const CHECKS_APPEAR_WITHIN_MS: i64 = 60_000;

/// A provider timestamp as the host measures it.
fn created_at_ms(created_at: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(created_at)
        .ok()
        .map(|created| created.timestamp_millis())
}

/// What GitHub says stands between a pull request and its merge.
///
/// GitHub reports this in two fields that each say part of it: `mergeStateStatus`
/// names the state, and `reviewDecision` says whether a review is the reason. Only
/// the pair together can say that a review is what is outstanding, which is the one
/// standing the operator is the next step for.
fn merge_standing(view: &PullRequestView) -> MergeStanding {
    match view.state.as_deref() {
        Some("MERGED") => return MergeStanding::Merged,
        Some("CLOSED") => return MergeStanding::Closed,
        _ => {}
    }
    if view.is_draft == Some(true) {
        return MergeStanding::Draft;
    }
    let mergeable = view.mergeable.as_deref();
    let review = view.review_decision.as_deref();
    if mergeable == Some("CONFLICTING") || view.merge_state_status.as_deref() == Some("DIRTY") {
        return MergeStanding::Conflicting;
    }
    if review == Some("CHANGES_REQUESTED") {
        return MergeStanding::ChangesRequested;
    }
    match view.merge_state_status.as_deref() {
        // BLOCKED is GitHub saying something is unsatisfied without saying what. A
        // required review is the one block that is the operator's, and it can only
        // be named when the branch is otherwise mergeable. A block this provider
        // cannot attribute is the agent's rather than guessed away.
        Some("BLOCKED") if review == Some("REVIEW_REQUIRED") && mergeable == Some("MERGEABLE") => {
            MergeStanding::AwaitingReview
        }
        Some("BLOCKED") => MergeStanding::Blocked,
        Some("BEHIND") => MergeStanding::Behind,
        Some("UNSTABLE") => MergeStanding::Unstable,
        // GitHub calls a merge unblocked CLEAN whether or not anyone reviewed it,
        // so the verdict is what tells the two apart.
        Some("CLEAN" | "HAS_HOOKS") if mergeable == Some("MERGEABLE") => match review {
            Some("APPROVED") => MergeStanding::Approved,
            _ => MergeStanding::Clear,
        },
        _ => MergeStanding::Unknown,
    }
}

/// A rollup entry is either a `CheckRun`, which carries a conclusion once it has
/// finished, or a legacy `StatusContext`, which carries a single state; both
/// normalize to the same three counts.
#[derive(Deserialize)]
struct CheckRollupEntry {
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

enum CheckOutcome {
    Passing,
    Failing,
    Pending,
}

impl CheckRollupEntry {
    fn outcome(&self) -> CheckOutcome {
        match self.conclusion.as_deref() {
            Some("SUCCESS" | "NEUTRAL" | "SKIPPED") => CheckOutcome::Passing,
            Some("FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE") => {
                CheckOutcome::Failing
            }
            // An unrecognized conclusion is surfaced rather than counted as a
            // pass, so a new GitHub verdict cannot hide behind a green build.
            Some("") => CheckOutcome::Pending,
            Some(_) => CheckOutcome::Failing,
            // A check run with no conclusion has not finished; a status context
            // carries its state here instead.
            None => match self.state.as_deref() {
                Some("SUCCESS") => CheckOutcome::Passing,
                Some("FAILURE" | "ERROR") => CheckOutcome::Failing,
                _ => CheckOutcome::Pending,
            },
        }
    }
}

fn check_summary(failing: u32, pending: u32, passing: u32) -> String {
    if failing + pending + passing == 0 {
        return "no checks reported".to_owned();
    }
    let mut parts = Vec::new();
    if failing > 0 {
        parts.push(format!("{failing} failing"));
    }
    if pending > 0 {
        parts.push(format!("{pending} pending"));
    }
    if passing > 0 {
        parts.push(format!("{passing} passing"));
    }
    format!("ci {}", parts.join(", "))
}
