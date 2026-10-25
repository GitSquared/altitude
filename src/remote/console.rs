//! How the serve role talks to a terminal.
//!
//! Tracker installation and service installation are both guides: they report what
//! this machine holds, ask one question at a time, and have to keep working when
//! nobody is at the keyboard. Everything they print or ask lives here, so the two
//! read the same way and a new guide inherits the shape.

use std::{
    fmt,
    io::{self, IsTerminal, Write},
    mem,
};

/// The width the serve role reads at.
pub const WIDTH: usize = 78;

/// Whether there is someone at the keyboard to answer.
pub fn has_terminal() -> bool {
    io::stdin().is_terminal()
}

/// A question with an answer that pressing enter gives.
///
/// A run with no terminal answers with `default` instead of waiting for a keypress
/// nobody is there to make. A step that writes to the machine checks for a
/// terminal itself rather than trusting this.
pub fn ask(prompt: &str, default: bool) -> io::Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(default);
    }
    print!("\n{prompt}  › ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(match answer.trim() {
        "" => default,
        answer => matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"),
    })
}

/// A question with a small number of answers, given by their number.
///
/// Nothing is assumed about a caller's ordering beyond it being the order shown,
/// and an answer that is not one of them is asked for again rather than guessed at.
pub fn choose(prompt: &str, count: usize, default: usize) -> io::Result<usize> {
    if !has_terminal() {
        return Ok(default);
    }
    loop {
        print!("\n{prompt}  › ");
        io::stdout().flush()?;
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer)? == 0 {
            return Ok(default);
        }
        let answer = answer.trim();
        if answer.is_empty() {
            return Ok(default);
        }
        match answer.parse::<usize>() {
            Ok(number) if (1..=count).contains(&number) => return Ok(number - 1),
            _ => println!("  Answer with a number from 1 to {count}."),
        }
    }
}

/// One labelled value, aligned with every other line that uses this.
pub fn field(label: &str, value: impl fmt::Display) {
    println!("  {label:<10}{value}");
}

/// A paragraph wrapped to what the rest of this output assumes, with the first
/// line led by `lead` and the rest by `indent`.
pub fn prose(text: &str, lead: &str, indent: &str) {
    let mut lines = wrap(text, WIDTH.saturating_sub(lead.len())).into_iter();
    if let Some(first) = lines.next() {
        println!("{lead}{first}");
    }
    for line in lines {
        println!("{indent}{line}");
    }
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            lines.push(mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}
