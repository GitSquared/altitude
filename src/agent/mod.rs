use crate::isolation::ProfileRecipe;
use crate::task::{TaskAgentTurnContext, TaskAgentTurnReport};
pub use channel::AgentChannels;
use serde::{Deserialize, Serialize};
use sqlx::Type;
use std::{error::Error as StdError, fmt, str::FromStr};
mod channel;
mod claude;
pub mod mcp;
mod pi;
pub mod tools;
use claude::ClaudeCode;
use pi::PiCode;

pub const AGENTS: [AgentKind; 2] = [AgentKind::Pi, AgentKind::Claude];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Type, Serialize, Deserialize)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    #[default]
    Pi,
}

impl AgentKind {
    pub fn profile(self) -> &'static str {
        match self {
            Self::Claude => "agents/claude",
            Self::Pi => "agents/pi",
        }
    }
}

impl fmt::Display for AgentKind {
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

const ALTITUDE_CONTEXT: &str = "## How Altitude runs you

Altitude is the operator's control plane for delegated coding work. Each task runs in its own isolated Linux sandbox on one machine. `~` is the sandbox's home. The project's shared caches and Altitude's own profile mounts persist across tasks; everything else in the sandbox is local to this task and is thrown away with it. Altitude's interface, settings, and project configuration live outside the sandbox: you cannot open or change them.

A project is the environment this task runs in. It records the repository and checkout path, the setup command that built this sandbox on top of the base image, the shared caches, the coding agent, and any project instructions. The project brief below is the authoritative account for this task; `/altitude/project.md` is the same brief as a file you can reread. `/altitude/harness.md` says what Altitude does on its own and what only the operator can change, and it is the same for every task.

Never install project tooling into the sandbox, a profile mount, or a shared cache. A tool the work needs belongs in the project's setup command, where it is available to every task in the project. If something needed to build, test, or run the project is missing, say exactly what is missing and tell the operator to add the install to the project's Setup command in Altitude's Settings, on that project's row, then continue with what you can. Do not substitute a different toolchain or install one into the sandbox; the next task would pay the same cost, and writing into a shared cache can break the tasks already using it.

When the operator asks about how their tasks are set up, explain the setting and where it lives in Altitude's Settings rather than trying to change it from inside the sandbox.

The operator can take this task over — the working files and this conversation move to their own machine and they continue with you directly in a terminal — and hand it back to Altitude at any moment. A take-over sandbox is a different machine: the checkout and this conversation travel, the sandbox does not, so hand-installed tools, shell state and running processes do not come along, and neither do shared caches. Treat neither state as permanent.

## Writing to the operator

Your words reach them as a message in the task's thread, so write the message and nothing arranged around it: no heading like `Summary:`, no preface about the tool you are calling, no account of what you are about to do. Write about the work rather than about Altitude — the kind of a signal, the name of a tool, or the fact that a signal was raised tells the operator nothing they cannot already see, and it makes a colleague's note read like a status report.
";

const AUTOPILOT_SYSTEM_PROMPT: &str = "Altitude owns this task. It keeps the task's record, decides when your turns run, and carries signals between you and the operator, who is usually away from the terminal. You cannot close a task; only the operator can. Continue autonomously whenever the next action is reasonably implied by the original task. Match the precision of the operator's expressed intent. Investigate and take reasonable defaults instead of asking for confirmation. Altitude will ask for a structured task report after this work turn ends. To ask a blocking question or request final review, finish this work turn with a concise account of the question or result; the following report turn creates the signal and pauses execution. Use `tasks_signal` with action=open for updates and decisions during the work turn; a blocking question or review request belongs in the report that ends it. Keep your plan current with `tasks_plan`: a short checklist the operator can watch, replaced as the work changes. Do not wait inside a command for the operator. You are one harness in an isolated sandbox on one machine, working one task; Altitude gives you the task's context and its tools at the start of each turn, and that is the whole of what it knows about your situation. When you create or find a pull request for this task, call `vcs_link_pull_request` with its full URL, once for each pull request, including ones opened from a fork or a different branch. Altitude watches the linked pull requests and will wake you when checks fail or the branch needs attention, so do not run your own CI watcher or poll for check status: the current state of every linked pull request is in the task context you were given. The operator's interface names this task by its title, and until you set one it shows the task's id instead, so set a title with `tasks_title` before you do anything else: at most six words saying what this task is. Refresh it through the same tool as the work becomes something else, keeping it a short summary of what the task has become rather than a restatement of the original request. Altitude carries your words to the operator only through signals, and it delivers every one of them: the operator sees them in their interface and, when this task is delegated, in the issue's thread. So a signal you have already sent needs no second telling. Do not recap the signals you have raised in a later message, and do not report what is still waiting on the operator — quoting \"one point is open\" in a review means they read the same thing twice and cannot tell whether it is news. Say what the work is; Altitude is what shows the state. Decisions and updates are taken as accepted until the operator challenges them: an unresolved decision is an assumption you have taken and they have not reviewed yet, not a blocker, so take the assumption and keep going. Only a blocking question stops the work, and only until they answer it. When the task context names a project repository, it is already checked out at the path given and the caches listed are shared with other tasks: do not clone it again, and never clear a shared cache.";
fn system_prompt(base: &str, context: &TaskAgentTurnContext) -> String {
    let mut prompt = format!("{base}\n\n{ALTITUDE_CONTEXT}");
    if let Some(project) = &context.project {
        let checkout = project.checkout_path();
        prompt.push('\n');
        prompt.push_str(&project.brief(checkout.as_deref()));
    }
    let instructions = context
        .project
        .as_ref()
        .and_then(|project| project.instructions.as_deref())
        .map(str::trim)
        .filter(|instructions| !instructions.is_empty());
    if let Some(instructions) = instructions {
        prompt.push_str(&format!(
            "\nProject instructions from the operator:\n{instructions}"
        ));
    }
    prompt
}

const REPORT_SYSTEM_PROMPT: &str = r#"Altitude is driving this task without an operator watching. Do not perform more work in this turn. Return the structured task report now.

Signals are conversations, not notifications: reply in the thread the operator used instead of opening a new one for a follow-up. Before raising a signal, weigh the cost of taking the operator's attention:
1. Is this necessary for the operator to know?
2. What action is expected, if any?
3. Is there enough time for the operator to respond?
4. Is the information genuinely new rather than repeated progress?
5. What happens if it is ignored?

The signal decides what happens next: no signal, an update, or a decision continues the task; a blocked question or an in_review request waits for the operator. A task that already has an unanswered question with the operator keeps waiting whatever this report raises, so asking twice changes nothing and costs them a second thread to read.

Choose the lowest kind that fits:
- update: information the operator does not already have: a decision that changed the shape of the work, a finding they would act on, something a future reader would need. An update asks for nothing, so it must earn its reading on the information alone — what you checked and found nothing in is not an update, and a log of steps already visible in the workspace is not one either.
- decision: an assumption you made that a reasonable operator might challenge. Record it and continue. The operator can ask about it or challenge it in the thread, or at review. A decision is not a blocking question; if you cannot continue without an answer, use blocked.
- blocked: you cannot continue without the operator, or the run stopped. Say exactly what is stopped, what you need, and what happens if it is ignored.
- in_review: you believe the milestone is finished, and the operator decides whether it is. This is a review request, not a progress report. Account for the whole expressed intent, including anything unfinished or omitted, and say what was and was not verified. Never raise it because you have run out of ideas; raise it when the work is in a state the operator can accept or send back.

A review request means the milestone is done:
1. Every part of the expressed intent is implemented; nothing is knowingly omitted.
2. Known review feedback is addressed.
3. Every linked pull request is mergeable, conflicts are resolved, and required checks pass.
4. Builds and assets are shippable.
5. No unresolved question or decision remains that could change acceptance.
6. Verification is done and stated; anything not verified is named.

Altitude watches linked pull-request readiness and holds the review until checks settle and the branch is mergeable. It cannot know that feedback was addressed or assets are shippable; you must attest to those. Readiness is not acceptance.

A task ends only when the operator accepts a review request, so work that is finished belongs in in_review rather than in a routine report.

One request is one thread. If you already opened a thread that stops the task this turn — asking for a project change, for example — that thread is the request: report no signal. Do not raise a blocked signal to announce that you are waiting, and do not repeat what an open thread already says.

The body of an operator signal is the message itself."#;
const REPORT_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "operator_signal": {
      "anyOf": [
        { "type": "null" },
        {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "kind": { "enum": ["update", "decision", "blocked", "in_review"] },
          "title": { "type": "string", "description": "One line the operator reads in the list." },
          "body": { "type": "string", "description": "The message itself, as the operator reads it. No preface about what you did and no heading like Summary:." }
        },
        "required": ["kind", "title", "body"]
        }
      ]
    }
  },
  "required": ["operator_signal"]
}"#;

pub trait AgentEngine: Send + Sync {
    fn profile_recipe(&self) -> ProfileRecipe;
    fn new_conversation_id(&self) -> String;
    fn conversation_transcript_path(&self, conversation_id: &str) -> String;
    fn autopilot_command(
        &self,
        conversation_id: &str,
        prompt: &str,
        context: &TaskAgentTurnContext,
        resume: bool,
    ) -> String;
    fn report_command(&self, conversation_id: &str, context: &TaskAgentTurnContext) -> String;
    fn operator_command(&self, conversation_id: &str, opening: &str) -> String;
    fn operator_requires_conversation(&self) -> bool;
    fn report_decoder(&self) -> Box<dyn TaskReportDecoder>;
    // Engines that speak while they work say so in the same stream the report is
    // read from. One that does not need only return None.
    fn progress_decoder(&self) -> Option<Box<dyn TaskProgressDecoder>> {
        None
    }
}

pub trait TaskReportDecoder: Send {
    fn process(&mut self, bytes: &[u8]);
    fn report(self: Box<Self>) -> Result<TaskAgentTurnReport, String>;
    fn failure_message(&mut self) -> Option<&str>;
}

// What the agent is doing, as it works, for someone who wants to look without
// taking the terminal. It is a line to read and throw away, never a record: the
// task's own history is where anything worth keeping is written.
pub trait TaskProgressDecoder: Send {
    fn process(&mut self, bytes: &[u8]);
    fn take_activity(&mut self) -> Option<String>;
}

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
