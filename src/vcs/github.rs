use crate::isolation::{EnvironmentRecipe, GuestPath, PersistentDirectory};

pub struct GitHub;

impl GitHub {
    pub fn environment_recipe() -> EnvironmentRecipe {
        EnvironmentRecipe {
            profile: "vcs/github",
            setup_command: "apk add --no-cache git openssh-client-default github-cli gnupg",
            directories: vec![
                PersistentDirectory::new("git", GuestPath::Absolute("/altitude/git")),
                PersistentDirectory::new("ssh", GuestPath::HomeRelative(".ssh")),
                PersistentDirectory::new("gh", GuestPath::HomeRelative(".config/gh")),
                PersistentDirectory::new("gnupg", GuestPath::HomeRelative(".gnupg")),
            ],
            // A directory mount lets `git config --global` atomically replace its file.
            environment: vec![("GIT_CONFIG_GLOBAL", "/altitude/git/config")],
        }
    }
}
