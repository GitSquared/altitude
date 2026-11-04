#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignInCommand {
    Direct(&'static str),
    Interactive(&'static str),
}

impl SignInCommand {
    pub fn command(self) -> &'static str {
        match self {
            Self::Direct(command) | Self::Interactive(command) => command,
        }
    }

    pub fn is_interactive(self) -> bool {
        matches!(self, Self::Interactive(_))
    }
}

#[derive(Clone)]
pub struct ProfileRecipe {
    pub profile: String,
    pub display: &'static str,
    pub sign_in: &'static str,
    pub sign_in_command: SignInCommand,
    pub status_command: &'static str,
    pub label: &'static str,
    pub setup_command: &'static str,
    pub directories: Vec<PersistentDirectory>,
    pub environment: Vec<(&'static str, &'static str)>,
}

#[derive(Clone)]
pub enum GuestPath {
    Absolute(&'static str),
    HomeRelative(&'static str),
}

#[derive(Clone)]
pub struct PersistentDirectory {
    pub relative_path: &'static str,
    pub guest_path: GuestPath,
    pub defaults: Vec<DefaultFile>,
    pub managed: Vec<(&'static str, &'static str)>,
    pub instructions: &'static str,
    pub task_local_subdirectories: Vec<&'static str>,
}

impl PersistentDirectory {
    pub fn at(
        relative_path: &'static str,
        guest_path: GuestPath,
        instructions: &'static str,
    ) -> Self {
        Self {
            relative_path,
            guest_path,
            defaults: Vec::new(),
            managed: Vec::new(),
            instructions,
            task_local_subdirectories: Vec::new(),
        }
    }

    pub fn root(
        guest_path: GuestPath,
        instructions: &'static str,
        defaults: Vec<DefaultFile>,
    ) -> Self {
        Self {
            relative_path: "",
            guest_path,
            defaults,
            managed: Vec::new(),
            instructions,
            task_local_subdirectories: Vec::new(),
        }
    }

    pub fn with_task_local_subdirectories(mut self, paths: &[&'static str]) -> Self {
        self.task_local_subdirectories = paths.to_vec();
        self
    }

    pub fn with_managed_file(mut self, path: &'static str, contents: &'static str) -> Self {
        self.managed.push((path, contents));
        self
    }
}

#[derive(Clone)]
pub struct DefaultFile {
    pub path: &'static str,
    pub contents: &'static str,
}
