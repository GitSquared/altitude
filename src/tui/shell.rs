use super::palette;

use std::{
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    text::Line,
    widgets::{Block, BorderType, Clear},
};
use terminput::Encoding;
use terminput_crossterm::to_terminput;
use tui_term::{vt100, widget::PseudoTerminal};

pub struct Shell {
    parser: vt100::Parser<ClipboardCallbacks>,
    clipboard: Receiver<String>,
    clipboard_notice_until: Option<Instant>,
}

impl Shell {
    pub fn new(rows: u16, cols: u16, history: Vec<Vec<u8>>) -> Self {
        let (clipboard_sender, clipboard) = mpsc::channel();
        let mut parser = vt100::Parser::new_with_callbacks(
            rows,
            cols,
            1_000,
            ClipboardCallbacks { clipboard_sender },
        );
        for bytes in history {
            parser.process(&bytes);
        }
        while clipboard.try_recv().is_ok() {}
        Self {
            parser,
            clipboard,
            clipboard_notice_until: None,
        }
    }

    pub fn process(&mut self, bytes: Vec<u8>) -> Option<String> {
        self.parser.process(&bytes);
        self.clipboard.try_recv().ok()
    }

    pub fn show_clipboard_confirmation(&mut self) {
        self.clipboard_notice_until = Some(Instant::now() + Duration::from_secs(2));
    }

    pub fn has_active_notice(&self) -> bool {
        self.clipboard_notice_until
            .is_some_and(|until| Instant::now() < until)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    pub fn key_bytes(&mut self, key: KeyEvent) -> Option<Vec<u8>> {
        if !self.parser.screen().alternate_screen() {
            match key.code {
                KeyCode::PageUp => {
                    self.scroll_up();
                    return None;
                }
                KeyCode::PageDown => {
                    self.scroll_down();
                    return None;
                }
                _ => {}
            }
        }
        encode_input(Event::Key(key))
    }

    pub fn paste_bytes(&self, text: String) -> Option<Vec<u8>> {
        if self.parser.screen().bracketed_paste() {
            encode_input(Event::Paste(text))
        } else {
            Some(text.into_bytes())
        }
    }

    pub fn mouse_bytes(&mut self, mut mouse: MouseEvent, area: Rect) -> Option<Vec<u8>> {
        let inner = Block::bordered().inner(popup_area(area));
        if !inner.contains((mouse.column, mouse.row).into()) {
            return None;
        }
        let screen = self.parser.screen();
        let mode = screen.mouse_protocol_mode();
        if mode == vt100::MouseProtocolMode::None {
            if !screen.alternate_screen() {
                let offset = screen.scrollback();
                let offset = match mouse.kind {
                    MouseEventKind::ScrollUp => offset + 3,
                    MouseEventKind::ScrollDown => offset.saturating_sub(3),
                    _ => return None,
                };
                self.parser.screen_mut().set_scrollback(offset);
            }
            return None;
        }
        // terminput emits SGR mouse reports; do not send them to legacy-mode apps.
        if screen.mouse_protocol_encoding() != vt100::MouseProtocolEncoding::Sgr {
            return None;
        }
        match mouse.kind {
            MouseEventKind::Up(_) if mode == vt100::MouseProtocolMode::Press => return None,
            MouseEventKind::Drag(_)
                if !matches!(
                    mode,
                    vt100::MouseProtocolMode::ButtonMotion | vt100::MouseProtocolMode::AnyMotion
                ) =>
            {
                return None;
            }
            MouseEventKind::Moved if mode != vt100::MouseProtocolMode::AnyMotion => return None,
            _ => {}
        }
        mouse.column -= inner.x;
        mouse.row -= inner.y;
        encode_input(Event::Mouse(mouse))
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

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let area = popup_area(area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(palette::accent())
            .title(" shell ");
        let block = if self.has_active_notice() {
            block.title_bottom(Line::styled(" copied to clipboard ", palette::accent()))
        } else {
            block
        };
        frame.render_widget(Clear, area);
        frame.render_widget(PseudoTerminal::new(self.parser.screen()).block(block), area);
    }
}

struct ClipboardCallbacks {
    clipboard_sender: Sender<String>,
}

impl vt100::Callbacks for ClipboardCallbacks {
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, _: &[u8], data: &[u8]) {
        let Ok(bytes) = STANDARD.decode(data) else {
            return;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return;
        };
        let _ = self.clipboard_sender.send(text);
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

fn encode_input(event: Event) -> Option<Vec<u8>> {
    let event = to_terminput(event).ok()?;
    let capacity = match &event {
        terminput::Event::Paste(text) => text.len() + 12,
        _ => 64,
    };
    let mut bytes = vec![0; capacity];
    let len = event.encode(&mut bytes, Encoding::Xterm).ok()?;
    Some(bytes[..len].to_vec())
}
