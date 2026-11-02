use serde::{Deserialize, Serialize};

// What kind of thing the line is: what the agent said, what it thought, or a line
// another machine sent, which travels as text and keeps no kind with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityKind {
    Speech,
    Thinking,
    Remote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    pub text: String,
    pub kind: ActivityKind,
}

impl Activity {
    pub fn remote(text: String) -> Self {
        Self {
            text,
            kind: ActivityKind::Remote,
        }
    }
}

// A message the agent sent and a thought it is thinking are the two things worth
// reading; what it is holding in its hands is not, and the tool calls it makes are
// the noisiest lines in the stream: a shell command is a pipeline of quoting, and
// its name alone answers less than the sentence around it.
pub(super) const NARRATION_LIMIT: usize = 160;

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
