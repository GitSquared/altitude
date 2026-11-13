use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskInput {
    pub id: String,
    pub at_ms: i64,
    pub author: TaskInputAuthor,
    pub source: TaskInputSource,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum TaskInputAuthor {
    Operator { id: String, label: String },
    Participant { id: String, label: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskInputSource {
    TaskCreated,
    Altitude,
    // altitude's own news rather than something the operator said: a machine that
    // restarted under the work, a pull request that needs attention.
    Notice,
    TrackerComment {
        provider: TrackerKind,
        issue_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        document_id: Option<String>,
    },
}

// What a pause means, said once. The tracker writes it into a session and the
// interface writes it into its own stream, and a task that stopped is the same news in
// both places.
impl TaskExecutionPauseReason {
    pub fn notice(&self) -> &'static str {
        match self {
            Self::AwaitingOperator => "Waiting for a reply on this task.",
            Self::AwaitingReviewChecks => "Waiting for the pull request checks to settle.",
            Self::AgentWentQuiet => {
                "The agent kept taking turns without anything to report, so the task stopped here."
            }
            // What happened, not what will happen: a pause that is over by the time it
            // is read must not promise a resume the line under it contradicts.
            Self::ApplicationShutdown => "The machine running this task restarted.",
            // The one pause nobody has to clear. It names the clock's owner and not the
            // hour, because a time is right for one machine's timezone and not another's.
            Self::AwaitingLimit => {
                "The account's usage limit is used up. This task resumes on its own when the \
                 limit clears; nothing here is waiting on you."
            }
            Self::Transfer => {
                "The operator is taking this task over on their own machine; it keeps running there."
            }
            Self::TrackerEnded => "The issue this task was working on ended, so the task stopped.",
            Self::StoppedInTracker => "This task was stopped in the tracker, so it stopped here.",
            Self::DelegationWithdrawn => {
                "This issue is no longer delegated to altitude, so the task stopped."
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum TaskExecutionStatus {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum TaskExecutionPauseReason {
    AwaitingOperator,
    AwaitingReviewChecks,
    ApplicationShutdown,
    // A turn that ends by asking for another one and leaves nothing behind would say the
    // same thing again from the same state.
    AgentWentQuiet,
    // The only pause nobody is waiting on: the account's limit clears by itself, so
    // the task comes back without the operator doing anything.
    AwaitingLimit,
    Transfer,
    // The endings a tracker hands down keep the task: what happened elsewhere is something
    // the operator can read here, rather than work that quietly disappears.
    TrackerEnded,
    StoppedInTracker,
    DelegationWithdrawn,
}

impl TaskExecutionPauseReason {
    pub fn is_out_of_play(&self) -> bool {
        matches!(
            self,
            Self::TrackerEnded | Self::StoppedInTracker | Self::DelegationWithdrawn
        )
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum TaskExecutionMode {
    Autopilot,
    Operator,
}

#[derive(Clone, Debug)]
pub struct TaskAgentRun {
    pub id: String,
    pub task_id: String,
    pub answering_signal_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAgentRunOutcome {
    Continued,
    AwaitingOperator,
    // The account refused the turn and said when it clears: nothing about the work
    // went wrong.
    Limited,
    Failed,
}
