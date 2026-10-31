//! Whether two Altitude builds can work together.
//!
//! The link speaks one protocol, and the announcement carries the plain package
//! version rather than the build's name (see [`crate::version`]). Two machines can
//! still be running releases far enough apart that what one promises the other has
//! never heard of, and the point of this module is to say so before a task is sent
//! somewhere it cannot finish, instead of failing halfway through an operation.
//!
//! Semantic versioning is the rule, and it is applied the way Cargo reads a caret
//! requirement: a `1.x` build works with any other `1.x`, and below `1.0` a minor
//! bump is the breaking change, so only the same `0.y` line works together. A
//! version this machine cannot read is treated as incompatible, because nothing
//! about it can be promised.

/// The package version this build announces, and the local half of every
/// comparison.
pub const LOCAL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How the version a remote announced stands next to this build.
///
/// This is deliberately one bit: two builds either work together or one of them
/// has to be updated. Which one is a judgement about who is newer, and it does not
/// change what the operator does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compatibility {
    /// The same compatible release line.
    Compatible,
    /// A different release line, or a version that could not be read.
    RequiresUpdate,
}

impl Compatibility {
    /// How the version a remote announced stands next to this build.
    pub fn of(remote: &str) -> Self {
        Self::between(LOCAL_VERSION, remote)
    }

    fn between(local: &str, remote: &str) -> Self {
        match (Version::parse(local), Version::parse(remote)) {
            (Some(local), Some(remote)) if local.compatible_with(remote) => Self::Compatible,
            _ => Self::RequiresUpdate,
        }
    }

    pub fn is_compatible(self) -> bool {
        matches!(self, Self::Compatible)
    }
}

/// A package version as `major.minor.patch`, with any pre-release or build suffix
/// ignored: a release candidate and its release are the same protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim().strip_prefix('v').unwrap_or(text.trim());
        // `1.2.3-rc.1+build` is still `1.2.3` for the question asked here.
        let core = text.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        // A fourth component is not a package version this can reason about.
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }

    /// Whether a build on one of these versions can work with a build on the
    /// other, in both directions.
    fn compatible_with(self, other: Self) -> bool {
        if self.major != other.major {
            return false;
        }
        // `1.x` is the stable promise: any other `1.x` works together.
        if self.major > 0 {
            return true;
        }
        // Below `1.0` a minor bump is the breaking change, so the `0.y` line has
        // to match. `0.0.z` is a special case in caret rules: only the exact
        // patch is compatible.
        self.minor == other.minor && (self.minor > 0 || self.patch == other.patch)
    }
}
