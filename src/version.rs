pub const VERSION: &str = env!("ALTITUDE_VERSION");

// The identity a remote announces: the same build as `VERSION` without the leading
// `v`, because a client's messages already spell a remote's version as `v{...}`, and
// an older peer announces the bare package version.
pub const BUILD: &str = env!("ALTITUDE_BUILD");
