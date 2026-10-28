//! Why a delegated issue cannot start on this machine.
//!
//! The reason belongs to the machine that refused it: the tracker coordinator
//! posts it, but it does not know which project claims a scope or which agent a
//! project runs. Keeping the reasons typed means the wording cannot drift
//! between the refusal and the message posted back.

use std::{fmt, future::Future, pin::Pin};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartRefusal {
    Paused,
    NoSetup,
    NoScope,
    NoProject { scope: String },
    NotAccepting { project: String, scope: String },
    NoAgent,
    Unavailable { reason: String },
}

impl fmt::Display for StartRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paused => formatter.write_str(
                "Delegated work is switched off on this remote: `pause_delegated_work` is set \
                 in its serve.toml. Turn it off, restart the remote, then reply here.",
            ),
            Self::NoSetup => formatter.write_str(
                "This remote has no settings from your Altitude yet, so it doesn't know your \
                 projects. In Settings → Projects, set a project's \"Start new tasks on\" to \
                 this remote, then reply here.",
            ),
            Self::NoScope => formatter.write_str(
                "Linear didn't say which team this issue belongs to, so Altitude can't tell \
                 which project should take it. Put the issue in a team, then reply here.",
            ),
            Self::NoProject { scope } => write!(
                formatter,
                "No project in your Altitude is set up for {scope}. Open Settings → Projects, \
                 choose the project that should take this work, and set its \"Task tracker \
                 team\" to that team; then reply here."
            ),
            Self::NotAccepting { project, scope } => write!(
                formatter,
                "Your project \"{project}\" is set up for {scope}, but it isn't taking \
                 delegated work. Open Settings → Projects, select \"{project}\", turn on \
                 \"Accept delegated work\", then reply here."
            ),
            Self::NoAgent => formatter.write_str(
                "Your project has no coding agent yet, so Altitude has nothing to run this task \
                 with. Open Settings → Projects, select the project, and choose one under \
                 \"Coding agent\"; then reply here.",
            ),
            Self::Unavailable { reason } => {
                write!(
                    formatter,
                    "This remote hit a problem before the task could start: {reason}"
                )
            }
        }
    }
}

/// The start method's future, kept separate because its error carries the
/// operator-facing reason rather than a generic string.
pub(crate) type StartFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, StartRefusal>> + Send + 'a>>;
