use serde::Deserialize;

use crate::{
    isolation::{EnvironmentRecipe, GuestPath, PersistentDirectory, shell_quote},
    vcs::{
        CodeContribution, ContributionObservation, ContributionState, VcsProvider, VcsProviderKind,
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

impl GitHub {
    pub fn environment_recipe() -> EnvironmentRecipe {
        EnvironmentRecipe {
            profile: "vcs/github",
            display: "GitHub",
            sign_in: "Run gh auth login and follow its sign-in instructions.",
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
}

impl VcsProvider for GitHub {
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

    fn observation_command(&self, contribution: &CodeContribution) -> String {
        // `gh` resolves the token from the capsule's persistent gh config, so
        // watching costs the operator no setup beyond authenticating the CLI.
        format!(
            "gh pr view {} --repo {} --json headRefOid,reviewDecision,statusCheckRollup",
            shell_quote(&contribution.reference),
            shell_quote(&contribution.repository),
        )
    }

    fn contribution_observation(
        &self,
        contribution: &CodeContribution,
        output: &str,
    ) -> Option<ContributionObservation> {
        let view: PullRequestView = serde_json::from_str(output).ok()?;
        let mut checks_passing = 0;
        let mut checks_failing = 0;
        let mut checks_pending = 0;
        for check in view.status_check_rollup.unwrap_or_default() {
            match check.outcome() {
                CheckOutcome::Passing => checks_passing += 1,
                CheckOutcome::Failing => checks_failing += 1,
                CheckOutcome::Pending => checks_pending += 1,
            }
        }
        let (review, review_label) = review_decision(view.review_decision.as_deref());
        Some(ContributionObservation {
            contribution_id: contribution.id.clone(),
            summary: format!(
                "{} · {review_label}",
                check_summary(checks_failing, checks_pending, checks_passing)
            ),
            state: ContributionState {
                head_sha: view.head_ref_oid,
                checks_passing,
                checks_failing,
                checks_pending,
                review: review.to_owned(),
            },
        })
    }
}

/// The subset of `gh pr view --json` this provider reads.
#[derive(Deserialize)]
struct PullRequestView {
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "reviewDecision", default)]
    review_decision: Option<String>,
    /// Absent or null when a pull request has no checks at all.
    #[serde(rename = "statusCheckRollup", default)]
    status_check_rollup: Option<Vec<CheckRollupEntry>>,
}

/// A rollup entry is either a `CheckRun` (status plus conclusion) or a legacy
/// `StatusContext` (a single state); both normalize to the same three counts.
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

/// Normalize `reviewDecision` into a stored token and an operator-facing label.
fn review_decision(value: Option<&str>) -> (&'static str, &'static str) {
    match value {
        Some("APPROVED") => ("approved", "review approved"),
        Some("CHANGES_REQUESTED") => ("changes_requested", "review changes requested"),
        Some("REVIEW_REQUIRED") => ("review_required", "review required"),
        _ => ("none", "no review yet"),
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
