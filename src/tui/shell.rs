use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    style::Style,
    text::Line,
    widgets::{Block, BorderType, Clear},
};
use tui_term::{vt100, widget::PseudoTerminal};

pub struct Shell {
    parser: vt100::Parser,
}

impl Shell {
    pub fn new(rows: u16, cols: u16, history: Vec<Vec<u8>>) -> Self {
        let mut parser = vt100::Parser::new(rows, cols, 1_000);
        for bytes in history {
            parser.process(&bytes);
        }
        Self { parser }
    }

    pub fn process(&mut self, bytes: Vec<u8>) {
        self.parser.process(&bytes);
    }

    pub fn key_bytes(&self, key: KeyEvent) -> Option<Vec<u8>> {
        key_bytes(key)
    }

    pub fn scroll_up(&mut self) {
        let rows = usize::from(self.parser.screen().size().0.saturating_sub(1));
        let scrollback = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(scrollback + rows);
    }

    pub fn scroll_down(&mut self) {
        let rows = usize::from(self.parser.screen().size().0.saturating_sub(1));
        let scrollback = self.parser.screen().scrollback();
        self.parser
            .screen_mut()
            .set_scrollback(scrollback.saturating_sub(rows));
    }

    pub fn draw(&self, frame: &mut Frame) {
        let area = popup_area(frame.area());
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().cyan())
            .title(" shell ")
            .title_bottom(Line::from(" pgup/pgdn history · ctrl-a back ").right_aligned());
        frame.render_widget(Clear, area);
        frame.render_widget(PseudoTerminal::new(self.parser.screen()).block(block), area);
    }
}

pub fn is_exit_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL)
}

pub fn size(area: Rect) -> (u16, u16) {
    let popup = popup_area(area);
    (
        popup.height.saturating_sub(2),
        popup.width.saturating_sub(2),
    )
}

fn popup_area(area: Rect) -> Rect {
    let [vertical] = Layout::vertical([Constraint::Percentage(80)])
        .flex(Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Percentage(80)])
        .flex(Flex::Center)
        .areas(vertical);
    horizontal
}

fn key_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let bytes = match key.code {
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            vec![(character.to_ascii_lowercase() as u8) & 0x1f]
        }
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        _ => return None,
    };
    Some(bytes)
}
