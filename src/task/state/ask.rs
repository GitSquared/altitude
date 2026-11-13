use super::*;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
// What a thread wants from a person: an answer the work cannot go on without, or work
// that is ready for them to review.
pub enum TaskAskKind {
    BlockingQuestion,
    ReadyForReview,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskAskQuestion {
    pub id: String,
    pub task_id: String,
    pub kind: TaskAskKind,
    pub title: String,
    pub body: String,
    pub raised_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<ProjectProposal>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectProposal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caches: Option<Vec<String>>,
}

impl ProjectProposal {
    pub fn is_empty(&self) -> bool {
        self.setup_command.is_none() && self.caches.is_none()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskAsk {
    pub id: String,
    pub task_id: String,
    pub kind: TaskAskKind,
    pub title: String,
    pub opened_at_ms: i64,
    pub last_raised_at_ms: i64,
    pub messages: Vec<TaskAskMessage>,
    pub resolved: bool,
    pub resolved_by: Option<TaskAskAuthor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolving_message_id: Option<String>,
    pub resolved_at_ms: Option<i64>,
    #[serde(default)]
    pub transitions: Vec<AskTransition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<ProjectProposal>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct AskTransition {
    pub at_ms: i64,
    pub resolved: bool,
    pub by: TaskAskAuthor,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskAskMessage {
    #[serde(default)]
    pub id: String,
    pub author: TaskAskAuthor,
    pub body: String,
    pub at_ms: i64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskAskAuthor {
    #[default]
    Agent,
    Operator,
}

impl TaskAsk {
    pub fn is_resolved(&self) -> bool {
        self.resolved
    }

    pub fn is_unresolved(&self) -> bool {
        !self.resolved
    }

    pub fn latest_agent_message(&self) -> Option<&TaskAskMessage> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.author == TaskAskAuthor::Agent)
    }

    // An open thread wants a person until someone speaks in it after it was asked. What
    // a thread says after that is conversation, and an agent that keeps talking in one the
    // operator is already answering has not handed the work back by doing so: asking again
    // is raising it again, which moves the question to the end of the thread.
    pub fn waits_on_operator(&self) -> bool {
        self.is_unresolved()
            && self
                .messages
                .iter()
                .all(|message| message.at_ms <= self.last_raised_at_ms)
    }
}

// What a task is in the middle of asking, without the thread: enough for a row, a
// summary and a pause, and no second opinion about whether it is waiting.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeadingAsk {
    pub kind: TaskAskKind,
    pub title: String,
    #[serde(default)]
    pub wants_a_person: bool,
}

impl LeadingAsk {
    pub fn waits_on_operator(&self) -> bool {
        self.wants_a_person
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeferredReview {
    pub signal: TaskAskQuestion,
}
