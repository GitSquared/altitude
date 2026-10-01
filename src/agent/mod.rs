use crate::isolation::EnvironmentRecipe;
use crate::tasks::{TaskAgentTurnContext, TaskAgentTurnReport};
use serde::{Deserialize, Serialize};
use sqlx::Type;
use std::{error::Error as StdError, fmt, str::FromStr};
mod claude;
mod pi;
use claude::ClaudeCode;
use pi::PiCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Type, Serialize, Deserialize)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Pi,
}

impl Default for AgentKind {
    /// An environment that names no harness gets Pi, which needs no provider
    /// selection of its own.
    fn default() -> Self {
        Self::Pi
    }
}

impl fmt::Display for AgentKind {
    /// How the operator reads this harness in the interface, which is not how
    /// it is spelled in settings, where the serde name is lowercase.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Claude => "Claude",
            Self::Pi => "Pi",
        })
    }
}

impl FromStr for AgentKind {
    type Err = AgentError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "pi" => Ok(Self::Pi),
            _ => Err(AgentError::new(format!(
                "unknown agent harness {value:?}; use claude or pi"
            ))),
        }
    }
}

#[derive(Debug)]
pub struct AgentError(String);

impl AgentError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl StdError for AgentError {}

/// The autonomy Altitude expects from every harness while no operator is watching.
const AUTOPILOT_SYSTEM_PROMPT: &str = "Altitude is driving this task without an operator watching. Continue autonomously whenever the next action is reasonably implied by the original task. Match the precision of the operator's expressed intent. Investigate and take reasonable defaults instead of asking for confirmation. Altitude will ask for a structured task report after this work turn ends.";
const REPORT_SYSTEM_PROMPT: &str = r#"Altitude is driving this task without an operator watching. Do not perform more work in this turn. Return the structured task report now.

Keep routine reasoning, progress, recoverable failures, and decisions you can reasonably infer in the summary. Before raising an operator signal, weigh the cost of taking the operator's attention:
1. Is this necessary for the operator to know?
2. What action is expected, if any?
3. Is there enough time for the operator to respond?
4. Is the information genuinely new rather than repeated progress?
5. What happens if it is ignored?

Raise no signal unless that reasoning justifies one. Signals have strict meanings:
- update: unique, durable information worth seeing, with no operator action required. Choose continue.
- decision: new context makes the original intent genuinely ambiguous and no reasonable default remains. State what is missing, why no reasonable path can be inferred, and what happens if ignored. Choose await_operator. If independent useful work remains, continue it before requesting a blocking decision.
- review: the requested work and applicable machine verification are complete, and human acceptance or requested changes are now needed. Account for the whole expressed intent, including any unfinished or omitted work. Include concrete review evidence and verification limits; pending CI or automated review is still work in progress. Choose await_operator; never treat review readiness as operator acceptance.

Priority reflects response time and impact if ignored: low is useful whenever convenient; normal deserves routine attention; high should be handled before dependent work proceeds; urgent means delay risks serious or irreversible harm. Do not inflate priority.

Include every pull-request or merge-request URL already associated with this task in code_contribution_urls. Do not create or publish one merely to populate that field.

Choose complete only when the task needs neither operator review nor any other operator action."#;
/// The report contract, written once for every harness.
///
/// Claude enforces it through its own CLI and Pi through a tool extension, but
/// both return the same object and Altitude validates it the same way.
const REPORT_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "requested_action": { "enum": ["continue", "await_operator", "complete"] },
    "summary": { "type": "string" },
    "operator_signal": {
      "anyOf": [
        { "type": "null" },
        {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "kind": { "enum": ["update", "decision", "review"] },
          "priority": { "enum": ["low", "normal", "high", "urgent"] },
          "title": { "type": "string" },
          "body": { "type": "string" }
        },
        "required": ["kind", "priority", "title", "body"]
        }
      ]
    },
    "code_contribution_urls": {
      "type": "array",
      "items": { "type": "string" }
    }
  },
  "required": ["requested_action", "summary", "operator_signal", "code_contribution_urls"]
}"#;

/// What Altitude needs from a coding harness.
///
/// Altitude owns the intent of a turn and the shape of a task report; a harness
/// owns how its own command line, session storage, and output encoding express
/// them. Nothing outside this module knows which harness is running.
pub trait AgentEngine: Send + Sync {
    fn environment_recipe(&self) -> EnvironmentRecipe;
    fn new_conversation_id(&self) -> String;
    fn conversation_transcript_path(&self, conversation_id: &str) -> String;
    /// A work turn: the harness continues the conversation autonomously.
    fn autopilot_command(
        &self,
        conversation_id: &str,
        prompt: &str,
        context: &TaskAgentTurnContext,
        resume: bool,
    ) -> String;
    /// A report turn: the harness returns Altitude's report and does no work.
    fn report_command(&self, conversation_id: &str, context: &TaskAgentTurnContext) -> String;
    /// An operator turn: the harness hands its terminal to the operator.
    fn operator_command(&self, conversation_id: &str) -> String;
    /// Whether handing the terminal to the operator needs an existing
    /// conversation. Claude resumes one specific conversation and fails without
    /// it; Pi creates the session on demand, which is what lets an operator log
    /// in before a task's first turn has ever succeeded.
    fn operator_requires_conversation(&self) -> bool;
    /// Decodes this harness's report-turn output into Altitude's task report.
    fn report_decoder(&self) -> Box<dyn TaskReportDecoder>;
}

/// Turns one harness's report-turn output into Altitude's task report.
///
/// Harnesses encode the report differently, so each owns the decoding while
/// Altitude owns the report itself.
pub trait TaskReportDecoder: Send {
    fn process(&mut self, bytes: &[u8]);
    fn report(self: Box<Self>) -> Result<TaskAgentTurnReport, String>;
}

/// The harnesses Altitude can drive, chosen per environment.
#[derive(Clone)]
pub struct AgentEngines {
    claude: ClaudeCode,
    pi: PiCode,
}

impl AgentEngines {
    pub fn new() -> Self {
        Self {
            claude: ClaudeCode,
            pi: PiCode,
        }
    }

    pub fn engine(&self, kind: AgentKind) -> &dyn AgentEngine {
        match kind {
            AgentKind::Claude => &self.claude,
            AgentKind::Pi => &self.pi,
        }
    }
}

impl Default for AgentEngines {
    fn default() -> Self {
        Self::new()
    }
}
