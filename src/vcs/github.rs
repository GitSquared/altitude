use serde::Deserialize;
use std::collections::HashMap;

use crate::{
    isolation::{GuestPath, PersistentDirectory, ProfileRecipe, SignInCommand, shell_quote},
    vcs::{
        CodeContribution, ContributionObservation, ContributionState, MergeStanding, Repository,
        VcsProvider, VcsProviderKind,
    },
};

const SETUP_COMMAND: &str = concat!(
    "apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends git openssh-client gh gnupg\n",
    // The login this profile mounts is the GitHub CLI's, so git is pointed at the
    // same one. Without it an https clone of a private repository stops at a
    // username prompt, and the sandbox's own setup has nobody to answer it.
    "git config --system credential.https://github.com.helper '!gh auth git-credential'\n",
    "cat > /usr/local/bin/git <<'ALTITUDE_GIT_SHIM'\n",
    include_str!("assets/git-shim.sh"),
    "ALTITUDE_GIT_SHIM\n",
    "chmod 0755 /usr/local/bin/git\n",
);

pub struct GitHub;

impl VcsProvider for GitHub {
    fn profile_recipe(&self) -> ProfileRecipe {
        ProfileRecipe {
            profile: "vcs/github".to_owned(),
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
            environment: vec![
                // A directory mount lets `git config --global` atomically replace its file.
                ("GIT_CONFIG_GLOBAL", "/altitude/git/config"),
                // A sandbox has a terminal, so a credential git does not have is a
                // prompt waiting for an operator who is not there. Failing instead
                // is the same answer, in the place the operator can read it.
                ("GIT_TERMINAL_PROMPT", "0"),
            ],
        }
    }

    fn repository_list_command(&self) -> String {
        // One request for the set, with the fields a picker reads. `pushedAt` is
        // asked for only to order the list: the repository an operator means is
        // almost always one they touched recently. NO_COLOR keeps gh from
        // colouring the JSON, which it does whenever the command has a terminal
        // (these commands run under one) and which would make it unreadable here.
        "NO_COLOR=1 gh repo list --limit 200 --json nameWithOwner,url,isPrivate,pushedAt".to_owned()
    }

    fn repositories(&self, output: &str) -> Option<Vec<Repository>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Row {
            name_with_owner: String,
            url: String,
            is_private: bool,
            // An empty repository has never been pushed to, so it has no date.
            #[serde(default)]
            pushed_at: Option<String>,
        }
        let mut rows: Vec<Row> = match serde_json::from_str(output) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "could not read the repository list");
                return None;
            }
        };
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
        // The name is compared case-insensitively, so the identity is lowercase.
        // The URL is not: GitHub resolves a `resource(url:)` by its exact owner, so
        // a link rewritten to lowercase comes back as nothing at all — which reads
        // as a pull request that does not exist rather than as a case mismatch.
        let naming = format!("{owner}/{repository}").to_ascii_lowercase();
        let canonical_url = format!("https://{host}/{owner}/{repository}/pull/{number}");
        Some(CodeContribution {
            id: canonical_url.to_ascii_lowercase(),
            provider: VcsProviderKind::GitHub,
            host: host.to_owned(),
            repository: naming,
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
        // NO_COLOR for the same reason as the repository list: a terminal makes
        // gh colour its JSON, and the reply is read as JSON.
        format!("NO_COLOR=1 gh api graphql -f query={}", shell_quote(&query))
    }

    fn contribution_observations(
        &self,
        contributions: &[CodeContribution],
        output: &str,
    ) -> Option<Vec<ContributionObservation>> {
        let response: ContributionQuery = serde_json::from_str(output).ok()?;
        let resources = response.data?;
        let mut observations = Vec::with_capacity(contributions.len());
        let mut unresolved = Vec::new();
        for (index, contribution) in contributions.iter().enumerate() {
            // A contribution the provider cannot resolve answers null, which is
            // not a state to decide anything from. It is also not a reason to
            // throw away the ones it did resolve: one dead link used to fail every
            // review its owner was watching.
            match resources.get(&format!("c{index}")) {
                Some(Some(view)) => observations.push(observation_of(contribution, view)),
                _ => unresolved.push(contribution.url.clone()),
            }
        }
        if observations.is_empty() && !unresolved.is_empty() {
            // Nothing was answered, so this is not a look at all.
            return None;
        }
        if !unresolved.is_empty() {
            tracing::warn!(
                unresolved = unresolved.join(", "),
                "the provider resolved none of these pull requests"
            );
        }
        Some(observations)
    }
}

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
          pageInfo { hasNextPage }
          nodes {
            ... on CheckRun { conclusion }
            ... on StatusContext { state }
          }
        }
      }
      commits(last: 1) {
        nodes { commit { committedDate } }
      }
    }"#;

fn graphql_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[derive(Deserialize)]
struct ContributionQuery {
    data: Option<HashMap<String, Option<PullRequestView>>>,
}

fn observation_of(
    contribution: &CodeContribution,
    view: &PullRequestView,
) -> ContributionObservation {
    let mut checks_passing = 0;
    let mut checks_failing = 0;
    let mut checks_pending = 0;
    let rollup = view.status_check_rollup.as_ref();
    for check in rollup
        .and_then(|rollup| rollup.contexts.as_ref())
        .map(|contexts| contexts.nodes.as_slice())
        .unwrap_or_default()
    {
        match check.outcome() {
            CheckOutcome::Passing => checks_passing += 1,
            CheckOutcome::Failing => checks_failing += 1,
            CheckOutcome::Pending => checks_pending += 1,
        }
    }
    // A rollup that has more than the queried page cannot be called settled:
    // the check that would fail may be on the next page.
    if rollup
        .and_then(|rollup| rollup.contexts.as_ref())
        .and_then(|contexts| contexts.page_info.as_ref())
        .is_some_and(|page| page.has_next_page)
    {
        checks_pending += 1;
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
                .head_committed_at()
                .or_else(|| view.created_at.as_deref().and_then(created_at_ms))
                .map(|committed| committed + CHECKS_APPEAR_WITHIN_MS)
                .unwrap_or_default(),
        },
    }
}

#[derive(Deserialize)]
struct PullRequestView {
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "createdAt", default)]
    created_at: Option<String>,
    #[serde(rename = "reviewDecision", default)]
    review_decision: Option<String>,
    #[serde(rename = "statusCheckRollup", default)]
    status_check_rollup: Option<CheckRollup>,
    #[serde(default)]
    commits: Option<CommitConnection>,
    #[serde(default)]
    mergeable: Option<String>,
    #[serde(rename = "mergeStateStatus", default)]
    merge_state_status: Option<String>,
    #[serde(rename = "isDraft", default)]
    is_draft: Option<bool>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Deserialize)]
struct CheckRollup {
    #[serde(default)]
    contexts: Option<CheckContexts>,
}

#[derive(Deserialize)]
struct CheckContexts {
    #[serde(default)]
    nodes: Vec<CheckRollupEntry>,
    #[serde(rename = "pageInfo", default)]
    page_info: Option<PageInfo>,
}

#[derive(Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage", default)]
    has_next_page: bool,
}

#[derive(Deserialize)]
struct CommitConnection {
    #[serde(default)]
    nodes: Vec<CommitNode>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: CommitDate,
}

#[derive(Deserialize)]
struct CommitDate {
    #[serde(rename = "committedDate", default)]
    committed_date: Option<String>,
}

impl PullRequestView {
    fn head_committed_at(&self) -> Option<i64> {
        self.commits
            .as_ref()?
            .nodes
            .first()?
            .commit
            .committed_date
            .as_deref()
            .and_then(created_at_ms)
    }
}

const CHECKS_APPEAR_WITHIN_MS: i64 = 60_000;

fn created_at_ms(created_at: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(created_at)
        .ok()
        .map(|created| created.timestamp_millis())
}

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
