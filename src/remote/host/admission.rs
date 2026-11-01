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
    pub is_agent_thread: bool,
    pub is_agent_document_comment: bool,
    pub is_tracked_signal: bool,
    pub is_task_issue: bool,
    pub has_other_humans: bool,
}

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

    // The operator's own words on the task's issue are words for the task. Nobody
    // else's are, which is why this asks about the actor rather than the place.
    if facts.actor == Actor::Operator && facts.is_task_issue {
        return Admission::Wake;
    }

    Admission::Context
}
