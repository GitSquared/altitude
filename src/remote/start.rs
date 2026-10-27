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
                "Delegated work is paused on this machine. Turn off `pause_delegated_work` in \
                 its serve.toml and restart it.",
            ),
            Self::NoSetup => formatter.write_str(
                "This machine has no setup for you yet. In Altitude, open Settings → Remotes, \
                 connect this machine, and sync your setup before delegating work.",
            ),
            Self::NoScope => formatter.write_str(
                "The tracker did not say which team, project, or repository this issue belongs \
                 to, so no project can claim it.",
            ),
            Self::NoProject { scope } => write!(
                formatter,
                "No project on this machine claims {scope}. On the machine that runs this \
                 work, open Settings → Projects and add the scope to a project's tracker \
                 route, then sync."
            ),
            Self::NotAccepting { project, scope } => write!(
                formatter,
                "Project \"{project}\" claims {scope} but is not accepting delegated work. On \
                 the machine that runs this work, open Settings → Projects and turn on \
                 \"Accept delegated work\" for \"{project}\", then sync."
            ),
            Self::NoAgent => formatter.write_str(
                "No agent is chosen for this work. On the machine that runs it, choose one for \
                 the project or for Altitude, then sync.",
            ),
            Self::Unavailable { reason } => {
                write!(formatter, "This machine could not start the work: {reason}")
            }
        }
    }
}

/// The start method's future, kept separate because its error carries the
/// operator-facing reason rather than a generic string.
pub(crate) type StartFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, StartRefusal>> + Send + 'a>>;
