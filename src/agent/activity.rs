use serde_json::Value;

// Both engines say the same three things about what they are doing — a tool they
// started, a file or command it is aimed at, and the words around it — in two
// different event shapes. The words the operator reads come from here so the two
// cannot drift apart.
pub(super) const NARRATION_LIMIT: usize = 160;

pub(super) fn tool_activity(tool: &str, args: Option<&Value>) -> String {
    let name = tool.to_ascii_lowercase();
    let verb = match name.as_str() {
        "bash" | "shell" => "running",
        "read" => "reading",
        "edit" | "write" | "multiedit" => "editing",
        "grep" | "search" => "searching for",
        "glob" | "find" => "looking for",
        "webfetch" | "fetch" => "fetching",
        "task" => "briefing a subagent",
        _ => tool,
    };
    let target = args
        .and_then(|args| args.get("command"))
        .and_then(Value::as_str)
        .map(|command| format!("`{}`", first_line(command, 80)))
        .or_else(|| {
            args.and_then(|args| {
                ["file_path", "path", "pattern"]
                    .iter()
                    .find_map(|key| args.get(*key).and_then(Value::as_str))
            })
            .map(|target| first_line(target, 80))
        });
    match target {
        Some(target) if !target.is_empty() => format!("{verb} {target}"),
        _ => verb.to_owned(),
    }
}

pub(super) fn narration(text: &str) -> Option<String> {
    let said = first_line(text, NARRATION_LIMIT);
    (!said.is_empty()).then(|| format!("\"{said}\""))
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
