use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    style::Stylize,
    text::Line,
    widgets::{Block, BorderType, Clear, Padding, Paragraph},
};

pub struct Prompt {
    title: &'static str,
    value: String,
    cursor: usize,
}

pub enum PromptAction {
    Cancel,
    Submit(String),
}

impl Prompt {
    pub fn new(title: &'static str) -> Self {
        Self {
            title,
            value: String::new(),
            cursor: 0,
        }
    }

    pub fn paste(&mut self, text: &str) {
        self.value.insert_str(self.byte_index(), text);
        self.cursor += text.chars().count();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<PromptAction> {
        match key.code {
            KeyCode::Esc => Some(PromptAction::Cancel),
            KeyCode::Enter if !self.value.trim().is_empty() => {
                Some(PromptAction::Submit(self.value.trim().to_owned()))
            }
            KeyCode::Char(character) => {
                self.value.insert(self.byte_index(), character);
                self.cursor += 1;
                None
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let index = self.byte_index();
                self.value.remove(index);
                None
            }
            KeyCode::Delete if self.cursor < self.value.chars().count() => {
                self.value.remove(self.byte_index());
                None
            }
            KeyCode::Left if self.cursor > 0 => {
                self.cursor -= 1;
                None
            }
            KeyCode::Right if self.cursor < self.value.chars().count() => {
                self.cursor += 1;
                None
            }
            KeyCode::Home => {
                self.cursor = 0;
                None
            }
            KeyCode::End => {
                self.cursor = self.value.chars().count();
                None
            }
            _ => None,
        }
    }

    pub fn draw(&self, frame: &mut Frame) {
        let area = centered_rect(70, 7, frame.area());
        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .title(format!(" {} ", self.title))
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1));
        let input_area = block.inner(area);
        frame.render_widget(block, area);
        if input_area.width < 3 || input_area.height == 0 {
            return;
        }
        // Keep the caret visible, measuring terminal columns rather than Unicode scalars.
        let available = usize::from(input_area.width - 3);
        let before_cursor = self.value[..self.byte_index()].replace(['\r', '\n'], "↵");
        let mut start = 0;
        while Line::from(&before_cursor[start..]).width() > available {
            start += before_cursor[start..].chars().next().unwrap().len_utf8();
        }
        let value = self.value.replace(['\r', '\n'], "↵");
        let input_width = Line::from(&before_cursor[start..]).width() as u16;
        frame.render_widget(
            Paragraph::new(Line::from(format!("❯ {}", &value[start..])).white()),
            input_area,
        );
        frame.set_cursor_position((input_area.x + 2 + input_width, input_area.y));
    }

    fn byte_index(&self) -> usize {
        self.value
            .char_indices()
            .nth(self.cursor)
            .map_or(self.value.len(), |(index, _)| index)
    }
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .areas(vertical);
    horizontal
}
