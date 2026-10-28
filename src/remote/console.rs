//! How the serve role talks to a terminal.
//!
//! Tracker installation and service installation are both guides: they report what
//! this machine holds, ask one question at a time, and have to keep working when
//! nobody is at the keyboard. Everything they print or ask lives here, so the two
//! read the same way and a new guide inherits the shape.

use std::{
    env, fmt,
    io::{self, IsTerminal, Write},
    mem,
};

/// The width the serve role reads at.
const WIDTH: usize = 78;

/// Whether there is someone at the keyboard to answer.
pub fn has_terminal() -> bool {
    io::stdin().is_terminal()
}

/// Whether this output can carry emphasis: a terminal on stdout, with colour not
/// turned off the way the rest of the CLI turns it off.
fn styled() -> bool {
    io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

/// The text in bold, where the terminal can show it.
pub fn bold(text: &str) -> String {
    if styled() {
        format!("\x1b[1m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// One answer, or nothing when there is no terminal to read it from.
fn line(prompt: &str) -> io::Result<Option<String>> {
    if !has_terminal() {
        return Ok(None);
    }
    print!("\n{}  › ", bold(prompt));
    io::stdout().flush()?;
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer)? == 0 {
        return Ok(None);
    }
    Ok(Some(answer.trim().to_owned()))
}

/// A yes or no, where pressing enter means `default`.
///
/// A run with no terminal answers with `default` instead of waiting for a keypress
/// nobody is there to make. A step that writes to the machine checks for a
/// terminal itself rather than trusting this.
pub fn ask(prompt: &str, default: bool) -> io::Result<bool> {
    Ok(match line(prompt)? {
        None => default,
        Some(answer) if answer.is_empty() => default,
        Some(answer) => matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"),
    })
}

/// A question with a small number of answers, given by their number.
///
/// An answer that is not one of them is asked for again rather than guessed at.
pub fn choose(prompt: &str, count: usize, default: usize) -> io::Result<usize> {
    loop {
        match line(prompt)? {
            None => return Ok(default),
            Some(answer) if answer.is_empty() => return Ok(default),
            Some(answer) => match answer.parse::<usize>() {
                Ok(number) if (1..=count).contains(&number) => return Ok(number - 1),
                _ => println!("  Answer with a number from 1 to {count}."),
            },
        }
    }
}

pub fn field(label: &str, value: impl fmt::Display) {
    println!("  {}{value}", bold(&format!("{label:<12}")));
}

/// A wrapped paragraph.
pub fn paragraph(text: &str) {
    prose(text, "", "");
}

/// A wrapped paragraph whose first line is led by `lead` and whose other lines start
/// with `indent`, for text that sits beside a number or a label.
pub fn prose(text: &str, lead: &str, indent: &str) {
    let mut lines = wrap(text, WIDTH.saturating_sub(lead.chars().count())).into_iter();
    if let Some(first) = lines.next() {
        println!("{lead}{first}");
    }
    for line in lines {
        println!("{indent}{line}");
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
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
