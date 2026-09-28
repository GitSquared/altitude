use crate::{
    isolation::{EnvironmentRecipe, GuestPath, PersistentDirectory},
    vcs::{CodeContribution, VcsProvider, VcsProviderKind},
};

pub struct GitHub;

impl GitHub {
    pub fn environment_recipe() -> EnvironmentRecipe {
        EnvironmentRecipe {
            profile: "vcs/github",
            setup_command: "apk add --no-cache git openssh-client-default github-cli gnupg",
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
        let [owner, repository, "pull", number] = parts.as_slice() else {
            return None;
        };
        if number.parse::<u64>().is_err() {
            return None;
        }
        let repository = format!("{owner}/{repository}");
        let canonical_url = format!("https://{host}/{repository}/pull/{number}");
        Some(CodeContribution {
            id: canonical_url.clone(),
            provider: VcsProviderKind::GitHub,
            host: host.to_owned(),
            repository,
            reference: (*number).to_owned(),
            url: canonical_url,
        })
    }
}
