use crate::tui::widgets::{palette, utils::shimmer};

use std::{
    str,
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    text::Line,
    widgets::{Block, BorderType, Clear, Paragraph},
};
use terminput::Encoding;
use terminput_crossterm::to_terminput;
use tui_term::{vt100, widget::PseudoTerminal};

/// What the program in a sandbox asked this machine to do, because a terminal
/// cannot do it from in there.
pub enum GuestRequest {
    /// Put this on the clipboard.
    Copy(String),
    /// Open this in a browser, which is on this side of the terminal.
    Open(String),
}

pub struct Shell {
    parser: vt100::Parser<HostCallbacks>,
    requests: Receiver<GuestRequest>,
    clipboard_notice_until: Option<Instant>,
    /// What this terminal is, on its frame: a task's run, or the shell opened to set
    /// up credentials.
    title: String,
}

impl Shell {
    pub fn new(rows: u16, cols: u16, history: Vec<Vec<u8>>, title: String) -> Self {
        let (host, requests) = mpsc::channel();
        let mut parser =
            vt100::Parser::new_with_callbacks(rows, cols, 1_000, HostCallbacks { host });
        for bytes in history {
            parser.process(&bytes);
        }
        // What the history asked for is dropped: a link an agent printed while
        // nobody was watching must not open a browser when someone attaches.
        while requests.try_recv().is_ok() {}
        Self {
            parser,
            requests,
            clipboard_notice_until: None,
            title,
        }
    }

    /// Feed the terminal what its program printed, and answer with what it asked
    /// for along the way.
    pub fn process(&mut self, bytes: Vec<u8>) -> Vec<GuestRequest> {
        self.parser.process(&bytes);
        self.requests.try_iter().collect()
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
            .title(format!(" {} ", self.title));
        let block = if self.has_active_notice() {
            block.title_bottom(Line::styled(" copied to clipboard ", palette::accent()))
        } else {
            block
        };
        frame.render_widget(Clear, area);
        frame.render_widget(PseudoTerminal::new(self.parser.screen()).block(block), area);
    }
}

pub fn draw_preparing(frame: &mut Frame, area: Rect, display: &str, progress: &str, tick: usize) {
    let popup = popup_area(area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(palette::accent())
        .title(format!(" Sign in to {display} "));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    let [line] = Layout::vertical([Constraint::Length(1)])
        .flex(Flex::Center)
        .areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from_iter(shimmer(progress, tick))).centered(),
        line,
    );
}

/// The escape sequences a program in a sandbox uses to ask for something only this
/// side of the terminal can do: a clipboard, and a browser.
struct HostCallbacks {
    host: Sender<GuestRequest>,
}

impl vt100::Callbacks for HostCallbacks {
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, _: &[u8], data: &[u8]) {
        let Ok(bytes) = STANDARD.decode(data) else {
            return;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return;
        };
        let _ = self.host.send(GuestRequest::Copy(text));
    }

    /// Altitude's own sequence, written by the opener every sandbox is given. The
    /// parser hands over anything that is not a sequence it knows, which is what
    /// makes this one ours to define.
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if let [b"1337", b"open", url] = params
            && let Ok(url) = str::from_utf8(url)
        {
            let _ = self.host.send(GuestRequest::Open(url.to_owned()));
        }
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
