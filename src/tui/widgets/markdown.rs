use std::mem;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::palette;

pub fn lines(body: &str, width: u16) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut block = Block::new(width);
    for event in Parser::new_ext(body, options) {
        block.event(event);
    }
    block.finish()
}

// A one-line stand-in for a body the operator only glances at, in a list.
pub fn preview(body: &str) -> String {
    lines(body, 400)
        .iter()
        .find(|line| line.width() > 0)
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .unwrap_or_default()
}

struct Block {
    width: usize,
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    used: usize,
    is_open: bool,
    // The marker a list item puts before its first line, and the blank one its
    // wrapped lines line up under instead.
    lead: String,
    hang: String,
    at_block_start: bool,
    quote_depth: usize,
    numbers: Vec<Option<u64>>,
    // A code block is written as it was given: the one thing in a body that must
    // not be reflowed or restyled.
    is_code: bool,
    cell: Option<String>,
    cells: Vec<String>,
    is_table_head: bool,
    link: Option<String>,
    image: Option<String>,
    style: Style,
    styles: Vec<Style>,
}

impl Block {
    fn new(width: u16) -> Self {
        Self {
            width: usize::from(width).max(1),
            lines: Vec::new(),
            spans: Vec::new(),
            used: 0,
            is_open: false,
            lead: String::new(),
            hang: String::new(),
            at_block_start: true,
            quote_depth: 0,
            numbers: Vec::new(),
            is_code: false,
            cell: None,
            cells: Vec::new(),
            is_table_head: false,
            link: None,
            image: None,
            style: palette::primary(),
            styles: Vec::new(),
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.close();
        while self.lines.last().is_some_and(|line| line.width() == 0) {
            self.lines.pop();
        }
        self.lines
    }

    fn event(&mut self, event: Event<'_>) {
        if let Some(cell) = self.cell.as_mut() {
            match &event {
                Event::Text(text) | Event::Code(text) => {
                    if !cell.is_empty() {
                        cell.push(' ');
                    }
                    cell.push_str(text);
                    return;
                }
                Event::End(TagEnd::TableCell) => {}
                _ => return,
            }
        }
        match event {
            Event::Text(text) if self.is_code => self.code_line(&text),
            Event::Text(text) => self.emit(&text, self.style),
            Event::Code(text) => self.emit(&text, palette::decision()),
            Event::SoftBreak => self.emit(" ", self.style),
            Event::HardBreak => self.wrap(),
            Event::Rule => {
                self.begin_block();
                let rule = "─".repeat(self.width);
                self.emit(&rule, palette::muted());
                self.close();
            }
            Event::TaskListMarker(done) => {
                self.emit(if done { "[x] " } else { "[ ] " }, palette::muted());
            }
            Event::Html(text) | Event::InlineHtml(text) => self.emit(&text, palette::muted()),
            Event::FootnoteReference(name) => self.emit(&format!("[{name}]"), palette::muted()),
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.emit(&text, palette::decision());
            }
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.begin_block(),
            Tag::Heading { level, .. } => {
                self.begin_block();
                let style = if level as usize <= 2 {
                    palette::heading()
                } else {
                    palette::heading().add_modifier(Modifier::DIM)
                };
                self.push_style(style);
            }
            Tag::BlockQuote(_) => {
                self.close();
                self.quote_depth = self.quote_depth.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                self.begin_block();
                self.is_code = true;
                if let CodeBlockKind::Fenced(language) = kind {
                    let language = language.trim();
                    if !language.is_empty() {
                        let label = format!(" {language} ");
                        self.emit(&label, palette::muted().add_modifier(Modifier::DIM));
                        self.close();
                    }
                }
            }
            Tag::List(start) => {
                if self.numbers.is_empty() {
                    self.begin_block();
                }
                self.numbers.push(start);
            }
            Tag::Item => {
                let marker = match self.numbers.last_mut() {
                    Some(Some(number)) => {
                        *number = number.saturating_add(1);
                        format!("{}. ", number.saturating_sub(1))
                    }
                    _ => "• ".to_owned(),
                };
                self.close();
                let depth = self.numbers.len().saturating_sub(1);
                self.lead = format!("{}{marker}", "  ".repeat(depth));
                self.hang = format!("{}{}", "  ".repeat(depth), " ".repeat(marker.width()));
                self.at_block_start = true;
            }
            Tag::Table(_) => self.begin_block(),
            Tag::TableRow => self.close(),
            Tag::TableHead => {
                self.close();
                self.is_table_head = true;
            }
            Tag::TableCell => self.cell = Some(String::new()),
            Tag::Emphasis => self.push_style(palette::primary().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(palette::primary().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_style(palette::muted().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { dest_url, .. } => {
                self.link = Some(dest_url.to_string());
                self.push_style(palette::accent().add_modifier(Modifier::UNDERLINED));
            }
            Tag::Image { dest_url, .. } => self.image = Some(dest_url.to_string()),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.close(),
            TagEnd::Heading(_) => {
                self.pop_style();
                self.close();
            }
            TagEnd::BlockQuote(_) => {
                self.close();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.is_code = false;
                self.close();
            }
            TagEnd::List(_) => {
                self.numbers.pop();
                self.lead.clear();
                self.hang.clear();
                self.at_block_start = true;
            }
            TagEnd::Item => {
                self.close();
                self.lead.clear();
                self.hang.clear();
                self.at_block_start = true;
            }
            TagEnd::TableCell => {
                if let Some(cell) = self.cell.take() {
                    let cell = cell.trim().to_owned();
                    self.cells.push(cell);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => self.row(),
            TagEnd::Table => self.close(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link => {
                self.pop_style();
                if let Some(url) = self.link.take()
                    && !url.is_empty()
                {
                    self.emit(&format!(" ({url})"), palette::muted());
                }
            }
            TagEnd::Image => {
                if let Some(url) = self.image.take() {
                    self.emit(&format!(" [image: {url}]"), palette::muted());
                }
            }
            _ => {}
        }
    }

    fn push_style(&mut self, style: Style) {
        self.styles.push(self.style);
        self.style = style;
    }

    fn pop_style(&mut self) {
        self.style = self.styles.pop().unwrap_or_else(palette::primary);
    }

    fn begin_block(&mut self) {
        self.close();
        if self.lines.last().is_some_and(|line| line.width() > 0) {
            self.lines.push(Line::default());
        }
    }

    fn row(&mut self) {
        let cells = mem::take(&mut self.cells);
        let is_head = mem::take(&mut self.is_table_head);
        if cells.is_empty() {
            return;
        }
        self.close();
        let tone = if is_head {
            palette::heading()
        } else {
            palette::primary()
        };
        for (index, cell) in cells.iter().enumerate() {
            if index > 0 {
                self.emit(" │ ", palette::muted());
            }
            self.emit(cell, tone);
        }
        self.close();
    }

    fn code_line(&mut self, text: &str) {
        for line in text.lines() {
            if line.is_empty() {
                self.open();
                self.close();
                continue;
            }
            self.emit(line, palette::muted());
            self.close();
        }
    }

    fn open(&mut self) {
        if self.is_open {
            return;
        }
        let marker = if self.at_block_start {
            self.lead.clone()
        } else {
            self.hang.clone()
        };
        let prefix = format!("{}{marker}", "│ ".repeat(self.quote_depth));
        self.used = prefix.width();
        if !prefix.is_empty() {
            self.spans.push(Span::styled(prefix, palette::muted()));
        }
        self.is_open = true;
        self.at_block_start = false;
    }

    fn room(&self) -> usize {
        self.width.saturating_sub(self.used)
    }

    fn wrap(&mut self) {
        self.close();
        self.open();
    }

    fn close(&mut self) {
        if !self.is_open {
            return;
        }
        self.lines.push(Line::from(mem::take(&mut self.spans)));
        self.used = 0;
        self.is_open = false;
    }

    fn push(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        self.used = self.used.saturating_add(text.width());
        self.spans.push(Span::styled(text.to_owned(), style));
    }

    fn emit(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        self.open();
        let mut rest = text;
        loop {
            let room = self.room().max(1);
            if rest.width() <= room {
                self.push(rest, style);
                return;
            }
            let (head, tail) = split_at(rest, room);
            if !head.is_empty() {
                self.push(head.trim_end(), style);
                let tail = tail.trim_start();
                if tail.is_empty() {
                    return;
                }
                self.wrap();
                rest = tail;
                continue;
            }
            // One glyph wider than the whole line still has to go somewhere.
            let mut boundaries = rest.char_indices();
            boundaries.next();
            let cut = boundaries.next().map_or(rest.len(), |(index, _)| index);
            self.push(&rest[..cut], style);
            let tail = rest[cut..].trim_start();
            if tail.is_empty() {
                return;
            }
            self.wrap();
            rest = tail;
        }
    }
}

// Splitting at the last space before the edge keeps words whole; a word longer
// than the line has to be cut, or it would never fit.
fn split_at(text: &str, room: usize) -> (&str, &str) {
    let mut used = 0;
    let mut cut = None;
    let mut space = None;
    for (index, character) in text.char_indices() {
        let width = character.to_string().width();
        if used + width > room {
            break;
        }
        used += width;
        cut = Some(index + character.len_utf8());
        if character == ' ' {
            space = Some(index);
        }
    }
    match (cut, space) {
        (Some(_cut), Some(space)) if space > 0 => (&text[..space], &text[space + 1..]),
        (Some(cut), _) => text.split_at(cut),
        (None, _) => ("", text),
    }
}
