use std::{
    env, fmt,
    io::{self, IsTerminal, Write},
    mem,
};

const WIDTH: usize = 78;

pub fn has_terminal() -> bool {
    io::stdin().is_terminal()
}

fn styled() -> bool {
    io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

pub fn bold(text: &str) -> String {
    if styled() {
        format!("\x1b[1m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

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

pub fn ask(prompt: &str, default: bool) -> io::Result<bool> {
    Ok(match line(prompt)? {
        None => default,
        Some(answer) if answer.is_empty() => default,
        Some(answer) => matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"),
    })
}

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

pub fn paragraph(text: &str) {
    prose(text, "", "");
}

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
