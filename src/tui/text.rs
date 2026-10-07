//! Text entry: one line, and several.
//!
//! Both are caret-owning buffers rather than renderers of someone else's string,
//! because a caret is the part that is easy to get subtly wrong: it is counted in
//! characters, indexed in bytes, and measured in terminal columns, and those are
//! three different numbers as soon as anything outside ASCII is typed.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Block, BorderType, Padding, Paragraph},
};

/// How a field should be drawn.
pub struct FieldStyle<'a> {
    pub focused: bool,
    /// Drawn on the box's top border, when a box is drawn at all.
    pub title: Option<&'a str>,
    /// Drawn before the text, inside the box.
    pub prefix: &'a str,
}

/// One line of text, with a caret.
#[derive(Clone, Default)]
pub struct Field {
    value: String,
    cursor: usize,
}

impl Field {
    pub fn new() -> Self {
        Self::default()
    }

    /// A field holding `value`, with the caret at the end of it.
    pub fn with(value: &str) -> Self {
        Self {
            value: value.to_owned(),
            cursor: value.chars().count(),
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    /// Whether there is nothing but whitespace.
    pub fn is_blank(&self) -> bool {
        self.value.trim().is_empty()
    }

    pub fn paste(&mut self, text: &str) {
        self.value.insert_str(self.byte_index(), text);
        self.cursor += text.chars().count();
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(character) => {
                self.value.insert(self.byte_index(), character);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.value.remove(self.byte_index());
            }
            KeyCode::Delete if self.cursor < self.value.chars().count() => {
                self.value.remove(self.byte_index());
            }
            KeyCode::Left if self.cursor > 0 => self.cursor -= 1,
            KeyCode::Right if self.cursor < self.value.chars().count() => self.cursor += 1,
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.chars().count(),
            _ => {}
        }
    }

    /// Draws a bordered box holding this field, and the caret when focused.
    ///
    /// The value scrolls rather than wraps, so a long line stays one line. The
    /// caret is kept on screen by measuring terminal columns, which is not the
    /// same as counting characters.
    pub fn draw(&self, frame: &mut Frame, area: Rect, style: &FieldStyle) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border_style(style.focused))
            .padding(Padding::horizontal(1));
        let block = match style.title {
            Some(title) => block.title(format!(" {title} ")),
            None => block,
        };
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let prefix_width = Line::from(style.prefix).width();
        let available = usize::from(inner.width).saturating_sub(prefix_width);
        let before_caret = self.value[..self.byte_index()].replace(['\r', '\n'], "↵");
        let mut start = 0;
        while Line::from(&before_caret[start..]).width() > available {
            start += before_caret[start..]
                .chars()
                .next()
                .expect("a char begins the slice")
                .len_utf8();
        }
        let value = self.value.replace(['\r', '\n'], "↵");
        let caret_column = Line::from(&before_caret[start..]).width();
        frame.render_widget(
            Paragraph::new(Line::from(format!(
                "{}{}",
                style.prefix,
                &value[start.min(value.len())..]
            ))),
            inner,
        );
        if style.focused {
            frame.set_cursor_position((
                inner.x + prefix_width as u16 + caret_column as u16,
                inner.y,
            ));
        }
    }

    fn byte_index(&self) -> usize {
        self.value
            .char_indices()
            .nth(self.cursor)
            .map_or(self.value.len(), |(index, _)| index)
    }
}

/// Several lines of text, with a caret somewhere among them.
///
/// A command is a shell script, so it holds newlines; enter adds one rather than
/// leaving the field, and leaving is what tab and escape are for.
#[derive(Clone)]
pub struct TextBox {
    lines: Vec<String>,
    row: usize,
    column: usize,
}

impl TextBox {
    /// A box holding `value`, with the caret after the last character of it.
    pub fn new(value: &str) -> Self {
        let lines: Vec<String> = if value.is_empty() {
            vec![String::new()]
        } else {
            value.split('\n').map(str::to_owned).collect()
        };
        let row = lines.len() - 1;
        let column = lines[row].chars().count();
        Self { lines, row, column }
    }

    /// The whole box as one string, which is what a setup command is.
    pub fn value(&self) -> String {
        self.lines.join("\n")
    }

    pub fn paste(&mut self, text: &str) {
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                self.handle_key(key(KeyCode::Enter));
            }
            for character in line.chars() {
                self.handle_key(key(KeyCode::Char(character)));
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Indexed rather than borrowed: every branch reads the caret's byte
        // position and then writes, so nothing may hold a line across the read.
        match key.code {
            KeyCode::Enter => {
                let at = self.byte_column();
                let rest = self.lines[self.row].split_off(at);
                self.lines.insert(self.row + 1, rest);
                self.row += 1;
                self.column = 0;
            }
            KeyCode::Char(character) => {
                let at = self.byte_column();
                self.lines[self.row].insert(at, character);
                self.column += 1;
            }
            KeyCode::Backspace if self.column > 0 => {
                self.column -= 1;
                let at = self.byte_column();
                self.lines[self.row].remove(at);
            }
            KeyCode::Backspace => {
                if self.row > 0 {
                    let rest = self.lines.remove(self.row);
                    self.row -= 1;
                    self.column = self.lines[self.row].chars().count();
                    self.lines[self.row].push_str(&rest);
                }
            }
            KeyCode::Delete if self.column < self.lines[self.row].chars().count() => {
                let at = self.byte_column();
                self.lines[self.row].remove(at);
            }
            KeyCode::Left if self.column > 0 => self.column -= 1,
            KeyCode::Right if self.column < self.lines[self.row].chars().count() => {
                self.column += 1
            }
            KeyCode::Up if self.row > 0 => {
                self.row -= 1;
                self.column = self.column.min(self.lines[self.row].chars().count());
            }
            KeyCode::Down if self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.column = self.column.min(self.lines[self.row].chars().count());
            }
            KeyCode::Home => self.column = 0,
            KeyCode::End => self.column = self.lines[self.row].chars().count(),
            _ => {}
        }
    }

    /// Draws a bordered box holding these lines, scrolled to keep the caret in
    /// view.
    pub fn draw(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border_style(focused))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let visible = usize::from(inner.height);
        let offset = self.row.saturating_sub(visible.saturating_sub(1));
        let text: Vec<Line> = self
            .lines
            .iter()
            .skip(offset)
            .take(visible)
            .map(|line| Line::from(line.clone()))
            .collect();
        frame.render_widget(Paragraph::new(text), inner);
        if focused {
            let before_caret = &self.lines[self.row][..self.byte_column()];
            frame.set_cursor_position((
                inner.x + Line::from(before_caret).width() as u16,
                inner.y + (self.row - offset) as u16,
            ));
        }
    }

    fn byte_column(&self) -> usize {
        self.lines[self.row]
            .char_indices()
            .nth(self.column)
            .map_or(self.lines[self.row].len(), |(index, _)| index)
    }
}

/// The border a field or box wears, so it is obvious which one has the caret.
fn border_style(focused: bool) -> Style {
    if focused {
        Style::new().cyan()
    } else {
        Style::new().dim()
    }
}

/// A key event built by hand, for pasted newlines and characters.
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
