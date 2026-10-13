//! A sandbox asking to open a link.
//!
//! The program in a sandbox has no browser, so the opener it was given hands the
//! address to the terminal Altitude streams, and this is the question that puts to
//! the operator. Nothing in the sandbox is waiting on the answer — a real
//! `xdg-open` returns once it has handed the address over, and so does Altitude's —
//! so the question can be answered late, answered no, or replaced by a newer one.
//!
//! What it opens is the whole address, never a shortened one: a display that opens
//! something longer than it shows is exactly how a link is made to look like
//! another link.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};

use crate::tui::{
    app::ShellId,
    keys,
    widgets::{chrome, palette},
};

/// The longest address this will carry, which is longer than any login page and
/// shorter than a payload.
const LONGEST: usize = 2_048;

/// An address a sandbox asked for, checked into something a browser may be given.
#[derive(Debug, PartialEq, Eq)]
pub struct Link {
    url: String,
}

impl Link {
    /// What a browser may be handed, or nothing.
    ///
    /// A sandbox chooses this, so only the two schemes that mean "a page" get
    /// through: `file://` and `javascript:` and anything else a launcher might
    /// treat as something other than a page are refused, not escaped.
    pub fn parse(url: &str) -> Option<Self> {
        if url.len() > LONGEST || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return None;
        }
        let (scheme, rest) = url.split_once("://")?;
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return None;
        }
        let host = rest.split(['/', '?', '#']).next()?;
        (!host.is_empty()).then(|| Self {
            url: url.to_owned(),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

/// The question, over the terminal that asked.
pub struct OpenLink {
    /// The terminal to go back to, whether or not this is answered.
    shell: ShellId,
    link: Link,
    /// 0 is not now, 1 is open.
    button: usize,
}

impl OpenLink {
    pub fn new(shell: ShellId, link: Link) -> Self {
        Self {
            shell,
            link,
            // Starting on not now means the key that raised this is never the key
            // that opens something.
            button: 0,
        }
    }

    /// The terminal this is over, which keeps drawing while the question is up.
    pub fn shell(&self) -> &ShellId {
        &self.shell
    }

    /// The address it is about, as the whole of what would open.
    pub fn url(&self) -> &str {
        self.link.url()
    }

    pub fn hints(&self) -> Vec<chrome::Hint> {
        vec![
            chrome::Hint::new("tab", "switch"),
            chrome::Hint::new("enter", "the one chosen"),
            chrome::Hint::new("esc", "not now"),
        ]
    }

    /// What the operator answered, or nothing while the question stands.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Decision> {
        if keys::is_dismiss(key) {
            return Some(Decision::No);
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                self.button = 1 - self.button;
                None
            }
            KeyCode::Enter => Some(if self.button == 1 {
                Decision::Open
            } else {
                Decision::No
            }),
            _ => None,
        }
    }
}

/// What the operator answered.
pub enum Decision {
    No,
    Open,
}

pub fn draw(frame: &mut Frame, area: Rect, asking: &OpenLink) {
    let popup = chrome::centered(area, 60, 9);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Sandbox asks ")
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(2));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [question, buttons] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(2)]).areas(inner);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled("Sandbox wants to open:", palette::muted()),
            Line::styled(asking.link.url(), palette::primary()),
            Line::from(""),
            Line::from("Open in browser now?"),
        ])
        .wrap(Wrap { trim: true }),
        question,
    );
    let [_, cancel, _, open] = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(10),
        Constraint::Length(3),
        Constraint::Length(10),
    ])
    .areas(buttons);
    chrome::button(frame, cancel, "cancel", asking.button == 0, false);
    chrome::button(frame, open, "open", asking.button == 1, true);
}
