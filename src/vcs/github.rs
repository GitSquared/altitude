use crate::isolation::{EnvironmentRecipe, GuestPath, PersistentDirectory};

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
