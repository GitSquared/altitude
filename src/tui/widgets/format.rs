pub fn plain(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        match character {
            '\u{1b}' => match characters.next() {
                // A control sequence ends at the first letter.
                Some('[') => {
                    characters.by_ref().any(|c| c.is_ascii_alphabetic());
                }
                // An operating system command ends at a bell or a string terminator.
                Some(']') => {
                    let mut previous = ' ';
                    for c in characters.by_ref() {
                        if c == '\u{7}' || (previous == '\u{1b}' && c == '\\') {
                            break;
                        }
                        previous = c;
                    }
                }
                Some(_) | None => {}
            },
            '\n' | '\t' => plain.push(character),
            c if !c.is_control() => plain.push(c),
            _ => {}
        }
    }
    plain
}

pub fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut truncated = text.chars().take(width - 1).collect::<String>();
    truncated.push('…');
    truncated
}

pub fn since(at_ms: i64, now_ms: i64) -> String {
    let seconds = now_ms.saturating_sub(at_ms) / 1000;
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

pub fn ago(at_ms: i64, now_ms: i64) -> String {
    if now_ms.saturating_sub(at_ms) < 60_000 {
        "just now".to_owned()
    } else {
        format!("{} ago", since(at_ms, now_ms))
    }
}

// The hour and minute on the operator's own clock, because a time is something to
// read whereas a countdown is a number that goes stale in front of them.
pub fn clock(at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at_ms)
        .map(|at| at.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

// A run's cost can be a fraction of a cent, and two decimal places would round it to
// nothing: the precision follows the size of the number instead.
pub fn money(cost_usd_micros: i64) -> String {
    let dollars = cost_usd_micros as f64 / 1_000_000.0;
    if dollars >= 0.01 {
        format!("${dollars:.2}")
    } else {
        format!("${dollars:.4}")
    }
}

// A sign-in command answers with a line naming the account. Which line is the
// provider's word; reading the name out of it is the same for all of them.
pub fn identity_from_output(output: &str, markers: &[&str]) -> Option<String> {
    for line in plain(output).lines() {
        for marker in markers {
            let Some((_, rest)) = line.split_once(marker) else {
                continue;
            };
            let Some(name) = rest.split_whitespace().next() else {
                continue;
            };
            if name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Some(name.to_owned());
            }
        }
    }
    None
}
