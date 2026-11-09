// The mark Altitude signs its own words with. It is one character because it has
// to survive the places the words go: a terminal title, a log line, and a body of
// markdown Linear renders back to the operator.
pub const MOUNTAIN: &str = "⛰";
pub const MOUNTAIN_CHAR: char = '⛰';

pub const TITLE: &str = "⛰  altitude";

// Altitude's own voice, marked so a reader can tell the machine's news from the
// agent's. It never marks the same line twice: the text a run already prefixed is
// the same voice, and a second mountain is a typo.
pub fn mark(text: impl AsRef<str>) -> String {
    let text = text.as_ref().trim_start();
    if text.starts_with(MOUNTAIN) {
        text.to_owned()
    } else {
        format!("{MOUNTAIN} {text}")
    }
}
