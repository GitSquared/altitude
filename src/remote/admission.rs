//! Whether a comment that arrived from a tracker is for this task's agent.
//!
//! The rules are deliberately conservative. A comment that is not clearly
//! addressed to the agent stays context: a shared issue is a human space, and a
//! wrong wake is more expensive than a missed one because the operator can
//! always mention the agent explicitly.
//!
//! This module is pure. The coordinator gathers the facts and performs the
//! decision; keeping the decision here makes the policy reviewable in one place.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    Operator,
    OtherHuman,
    Agent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Ignore,
    Context,
    Wake,
    Redirect,
}

impl Admission {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Ignore => "not addressed to this task",
            Self::Context => "shared context, no wake",
            Self::Wake => "addressed to the agent",
            Self::Redirect => "another human, redirect to the operator",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct InputFacts {
    pub actor: Actor,
    pub is_mention: bool,
    /// The root of this thread was written by the app, so a reply is a
    /// conversation with the agent rather than a comment on an issue.
    pub is_agent_thread: bool,
    pub is_agent_document_comment: bool,
    /// A signal root this task published and tracks. Reserved for the signal
    /// binding ledger; false until signal comments exist.
    pub is_tracked_signal: bool,
    /// Any human other than the operator has spoken in this thread or document.
    /// Unknown is treated as true so an uncertain answer cannot cause a wake.
    pub has_other_humans: bool,
}

/// Decide before any reaction or wake.
///
/// Order matters:
/// 1. An explicit mention is the operator's strongest signal, and a non-operator
///    mention is the one case that deserves a reply the agent did not otherwise
///    earn.
/// 2. A tracked signal root is the operator↔agent channel.
/// 3. A reply in a one-to-one agent thread, or a top-level comment on a
///    one-to-one agent document, is a conversation with the agent.
/// 4. Everything else is context.
pub fn decide(facts: &InputFacts) -> Admission {
    if facts.actor == Actor::Agent {
        return Admission::Ignore;
    }

    if facts.is_mention {
        return match facts.actor {
            Actor::Operator => Admission::Wake,
            Actor::OtherHuman => Admission::Redirect,
            Actor::Agent => Admission::Ignore,
        };
    }

    if facts.is_tracked_signal && facts.actor == Actor::Operator {
        return Admission::Wake;
    }

    if facts.actor == Actor::Operator
        && !facts.has_other_humans
        && (facts.is_agent_thread || facts.is_agent_document_comment)
    {
        return Admission::Wake;
    }

    Admission::Context
}
