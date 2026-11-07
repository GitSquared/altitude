// The agent's own words are the only lines worth reporting. What it is holding in its
// hands is not, and the tool calls it makes are the noisiest lines in the stream: a
// shell command is a pipeline of quoting, and its name alone answers less than the
// sentence around it. Reasoning is not reported either — it is a draft of the words,
// it arrives several times a minute, and it turns a session into a scroll.
pub(super) fn narration(text: &str) -> Option<String> {
    let said = text.trim();
    (!said.is_empty()).then(|| said.to_owned())
}
