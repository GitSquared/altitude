//! Why a delegated issue cannot start on this machine.
//!
//! The reason belongs to the machine that refused it: the tracker coordinator
//! posts it, but it does not know which project claims a scope or which agent a
//! project runs. Keeping the reasons typed means the wording cannot drift
//! between the refusal and the message posted back.

use std::{fmt, future::Future, pin::Pin};

/// A start refusal the tracker coordinator can report back verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartRefusal {
    /// This machine has the delegated-work brake on.
    Paused,
    /// The peer has not pushed the settings this machine runs work with.
    NoSetup,
    /// The tracker did not name the issue's scope.
    NoScope,
    /// No project on this machine claims the issue's scope.
    NoProject { scope: String },
    /// A project claims the scope but will not start unattended work.
    NotAccepting { project: String, scope: String },
    /// Neither the project nor the machine has chosen an agent.
    NoAgent,
    /// Something on this machine failed before the work could start.
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
                 projects. Sync this remote from Settings → Remotes, then reply here.",
            ),
            Self::NoScope => formatter.write_str(
                "Linear didn't say which team this issue belongs to, so Altitude can't tell \
                 which project should take it. Put the issue in a team, then reply here.",
            ),
            Self::NoProject { scope } => write!(
                formatter,
                "No project in your Altitude is set up for {scope}. Open Settings → Projects, \
                 choose the project that should take this work, add {scope} to its task \
                 tracker, then sync."
            ),
            Self::NotAccepting { project, scope } => write!(
                formatter,
                "Your project \"{project}\" is set up for {scope}, but it isn't taking \
                 delegated work. Open Settings → Projects, select \"{project}\", turn on \
                 \"Accept delegated work\", then sync."
            ),
            Self::NoAgent => formatter.write_str(
                "Your project has no agent yet, so Altitude has nothing to run this task with. \
                 Open Settings → Projects and give the project an agent, or choose one agent \
                 for everything in Altitude, then sync.",
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
