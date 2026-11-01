pub const LOCAL_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compatibility {
    Compatible,
    RequiresUpdate,
}

impl Compatibility {
    pub fn with_local_version(remote_version: &str) -> Self {
        Self::compare_versions(LOCAL_VERSION, remote_version)
    }

    fn compare_versions(local_version: &str, remote_version: &str) -> Self {
        match (
            Version::parse(local_version),
            Version::parse(remote_version),
        ) {
            (Some(local), Some(remote)) if local.compatible_with(remote) => Self::Compatible,
            _ => Self::RequiresUpdate,
        }
    }

    pub fn is_compatible(self) -> bool {
        matches!(self, Self::Compatible)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim().strip_prefix('v').unwrap_or(text.trim());
        // Prerelease and build suffixes do not change the wire protocol.
        let release_version = text.split(['-', '+']).next()?;
        let mut components = release_version.split('.');
        let major = components.next()?.parse().ok()?;
        let minor = components.next().unwrap_or("0").parse().ok()?;
        let patch = components.next().unwrap_or("0").parse().ok()?;
        if components.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }

    fn compatible_with(self, other: Self) -> bool {
        if self.major != other.major {
            return false;
        }
        if self.major > 0 {
            return true;
        }
        // Match Cargo caret compatibility: before 1.0, minor changes break it;
        // at 0.0, patch changes do too.
        self.minor == other.minor && (self.minor > 0 || self.patch == other.patch)
    }
}
