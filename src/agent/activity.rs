// What kind of thing the line is: what the agent said, what it is thinking, or a line
// another machine sent, which travels as text and keeps no kind with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    Speech,
    Thinking,
    Remote,
    // The machine's own news: it started, it is waiting on something, the sandbox
    // moved, a run stopped. It is the one kind marked with the mountain, so a reader
    // can tell what Altitude did from what the agent said.
    Altitude,
}

// A line of the agent's own words. It is the whole of what was said rather than a
// sentence cut to fit a row: a row no longer carries one, and a session is read at
// whatever length the thought took.
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

    pub fn altitude(text: String) -> Self {
        Self {
            text,
            kind: ActivityKind::Altitude,
        }
    }
}

// What the agent says and what it is thinking are both worth reading, and they are
// worth reading differently: one is addressed to whoever is watching, the other is a
// draft of it. What it is holding in its hands is not, and the tool calls it makes are
// the noisiest lines in the stream — a shell command is a pipeline of quoting, and its
// name alone answers less than the sentence around it.
pub(super) fn narration(text: &str) -> Option<Activity> {
    speaking(text).map(|said| Activity {
        text: said,
        kind: ActivityKind::Speech,
    })
}

pub(super) fn thinking(text: &str) -> Option<Activity> {
    speaking(text).map(|thought| Activity {
        text: thought,
        kind: ActivityKind::Thinking,
    })
}

fn speaking(text: &str) -> Option<String> {
    let said = text.trim();
    (!said.is_empty()).then(|| said.to_owned())
}
