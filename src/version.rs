//! The name this build calls itself.

/// The package version and the commit it was built from, as `--version` and the
/// interface's header print it.
///
/// The wire keeps the plain package version instead: a remote and a client built
/// from different commits still speak the same protocol.
pub const VERSION: &str = env!("ALTITUDE_VERSION");
