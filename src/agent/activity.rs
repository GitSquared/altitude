use serde::{Deserialize, Serialize};

// What kind of thing the line is, because the three read differently: a message is
// quoted, a thought says it is thinking, and a tool is a passing action rather than
// something the agent said. `Remote` is a line another machine sent, which travels
// as text and keeps no kind with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityKind {
    Speech,
    Thinking,
    Tool,
    Remote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    pub text: String,
    pub kind: ActivityKind,
}

impl Activity {
    pub(super) fn tool(tool: &str) -> Self {
        Self {
            text: tool_activity(tool),
            kind: ActivityKind::Tool,
        }
    }

    pub fn remote(text: String) -> Self {
        Self {
            text,
            kind: ActivityKind::Remote,
        }
    }
}

// Both engines say the same three things about what they are doing — a tool they
// started, a file or command it is aimed at, and the words around it — in two
// different event shapes. The words the operator reads come from here so the two
// cannot drift apart.
pub(super) const NARRATION_LIMIT: usize = 160;

pub(super) fn tool_activity(tool: &str) -> String {
    // One shape for every harness's tool: what it is using, named the way its own
    // stream names it. The arguments are not shown — a shell command is a pipeline
    // of quoting, and a path is the tool's business rather than the operator's:
    // neither answers "what is it doing" faster than the tool's own name.
    format!("using {}…", tool.to_ascii_lowercase())
}

pub(super) fn narration(text: &str) -> Option<Activity> {
    speaking(text).map(|said| Activity {
        text: format!("\"{said}\""),
        kind: ActivityKind::Speech,
    })
}

pub(super) fn thinking(text: &str) -> Option<Activity> {
    // The word is in the line, not only in the style: a line that leaves this
    // machine as text has to read as thinking wherever it lands.
    speaking(text).map(|thought| Activity {
        text: format!("thinking: {thought}"),
        kind: ActivityKind::Thinking,
    })
}

fn speaking(text: &str) -> Option<String> {
    let said = first_line(text, NARRATION_LIMIT);
    (!said.is_empty()).then_some(said)
}

pub(super) fn first_line(text: &str, limit: usize) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut shortened: String = line.chars().take(limit).collect();
    if shortened.len() < line.len() {
        shortened.push('…');
    }
    shortened
}
