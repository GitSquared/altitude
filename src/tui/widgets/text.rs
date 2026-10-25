//! Text entry: one line, and several.
//!
//! Both are caret-owning buffers rather than renderers of someone else's string,
//! because a caret is the part that is easy to get subtly wrong: it is counted in
//! characters, indexed in bytes, and measured in terminal columns, and those are
//! three different numbers as soon as anything outside ASCII is typed.

use crate::tui::widgets::palette;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Block, BorderType, Padding, Paragraph},
};
use unicode_width::UnicodeWidthChar;

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
            // A chord is not text: ctrl-c is not the letter c.
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
            {
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
            // A chord is not text: ctrl-c is not the letter c.
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
            {
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

    /// Draws a bordered box holding these lines, wrapped to its width and
    /// scrolled to keep the caret in view.
    ///
    /// Wrapping is the box's, not the paragraph's, because the caret has to land
    /// on the character it belongs to: a renderer that wraps on its own knows
    /// where the text went and not where the caret should follow it.
    pub fn draw(&self, frame: &mut Frame, area: Rect, focused: bool) {
        self.draw_titled(frame, area, focused, None);
    }

    /// The same box with something said on its top border, for a box that is one
    /// question rather than a field of a larger page.
    pub fn draw_titled(&self, frame: &mut Frame, area: Rect, focused: bool, title: Option<&str>) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border_style(focused))
            .padding(Padding::horizontal(1));
        let block = match title {
            Some(title) => block.title(format!(" {title} ")),
            None => block,
        };
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let rows = self.rows(usize::from(inner.width));
        // The caret's row, not the caret's line: a long paragraph is several
        // rows, and scrolling by line would leave the caret off screen.
        let caret = rows
            .iter()
            .position(|row| row.line == self.row && row.end >= self.column)
            .unwrap_or(rows.len().saturating_sub(1));
        let visible = usize::from(inner.height);
        let offset = caret.saturating_sub(visible.saturating_sub(1));
        let text: Vec<Line> = rows
            .iter()
            .skip(offset)
            .take(visible)
            .map(|row| {
                Line::from(
                    self.lines[row.line]
                        .chars()
                        .skip(row.start)
                        .take(row.end - row.start)
                        .collect::<String>(),
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(text), inner);
        if focused {
            let row = &rows[caret];
            let before_caret = self.lines[row.line]
                .chars()
                .skip(row.start)
                .take(self.column - row.start)
                .collect::<String>();
            frame.set_cursor_position((
                inner.x + Line::from(before_caret).width() as u16,
                inner.y + (caret - offset) as u16,
            ));
        }
    }

    /// The lines as they are drawn at `width`, in rows rather than lines.
    fn rows(&self, width: usize) -> Vec<Row> {
        self.lines
            .iter()
            .enumerate()
            .flat_map(|(line, text)| {
                wrap(text, width)
                    .into_iter()
                    .map(move |(start, end)| Row { line, start, end })
            })
            .collect()
    }

    fn byte_column(&self) -> usize {
        self.lines[self.row]
            .char_indices()
            .nth(self.column)
            .map_or(self.lines[self.row].len(), |(index, _)| index)
    }
}

/// One drawn row of a box: a stretch of one of its lines, by character.
struct Row {
    line: usize,
    start: usize,
    end: usize,
}

/// Where `text` breaks to fit `width` terminal columns, as character ranges.
///
/// Breaks after the last space that fits, so a paragraph wraps between words;
/// with no space in reach it breaks at the column, because a URL is still one
/// word. The space broken at is not drawn again, so a wrapped paragraph does
/// not gain a leading space.
fn wrap(text: &str, width: usize) -> Vec<(usize, usize)> {
    let characters: Vec<char> = text.chars().collect();
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut start = 0;
    while start < characters.len() {
        let mut cells = 0;
        let mut space = None;
        let mut end = characters.len();
        let mut index = start;
        while index < characters.len() {
            let character_width = characters[index].width().unwrap_or(0);
            if cells + character_width > width {
                end = space.unwrap_or(index).max(start + 1);
                break;
            }
            cells += character_width;
            if characters[index] == ' ' {
                space = Some(index);
            }
            index += 1;
        }
        rows.push((start, end));
        start = if characters.get(end) == Some(&' ') {
            end + 1
        } else {
            end.max(start + 1)
        };
    }
    if rows.is_empty() {
        rows.push((0, 0));
    }
    rows
}

/// The border a field or box wears, so it is obvious which one has the caret.
fn border_style(focused: bool) -> Style {
    if focused {
        palette::accent()
    } else {
        palette::muted()
    }
}

/// A key event built by hand, for pasted newlines and characters.
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
