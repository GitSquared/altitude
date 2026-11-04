use super::*;

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    text::Line,
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Widget, Wrap},
};

use crate::{
    project::{self},
    tui::widgets::{
        button, chrome,
        format::truncate,
        palette,
        select::{self},
        text::FieldStyle,
    },
};

pub fn draw_form(frame: &mut Frame, area: Rect, form: &mut Form, tick: Option<usize>) {
    let project = !form.target.is_remote();
    let area = if project {
        chrome::centered_at_most(area, 80, 84, area.height.saturating_sub(2).clamp(10, 40))
    } else {
        chrome::centered_at_most(area, 60, 60, chrome::modal_height(5))
    };
    let block = Block::bordered()
        .title(format!(" {} ", form.target.title()))
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(2));
    frame.render_widget(Clear, area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 6 {
        return;
    }

    let body = chrome::body(inner);
    let fields = body.content;
    if fields.width == 0 || fields.height == 0 {
        return;
    }
    let groups = groups(project);
    let mut tops = Vec::with_capacity(groups.len());
    let mut heights = Vec::with_capacity(groups.len());
    let mut top = 0u16;
    for group in &groups {
        let height = group.kind.rows();
        tops.push(top);
        heights.push(height);
        top = top.saturating_add(height).saturating_add(1);
    }
    let content_height = top;
    let max_scroll = content_height.saturating_sub(fields.height);
    form.scroll = form.scroll.min(max_scroll);
    if !form.free_scroll
        && let Some(index) = groups
            .iter()
            .position(|candidate| candidate.focus == form.focus)
    {
        let at = tops[index];
        let height = heights[index];
        if at < form.scroll {
            form.scroll = at;
        } else if at.saturating_add(height) > form.scroll.saturating_add(fields.height) {
            form.scroll = at
                .saturating_add(height)
                .saturating_sub(fields.height)
                .min(at);
        }
        form.scroll = form.scroll.min(max_scroll);
    }

    let mut content = Buffer::empty(Rect::new(0, 0, fields.width, content_height.max(1)));
    form.hits.clear();
    let mut cursor = None;
    let mut repository_anchor = None;
    let mut scope_anchor = None;
    let mut pill_anchor = None;
    for (index, group) in groups.iter().enumerate() {
        let rect = Rect {
            x: 0,
            y: tops[index],
            width: fields.width,
            height: heights[index],
        };
        let drawn = draw_group(&mut content, rect, form, group);
        if group.focus == form.focus {
            cursor = drawn;
        }
        let screen_y = i32::from(fields.y) + i32::from(tops[index]) - i32::from(form.scroll);
        if screen_y + i32::from(heights[index]) > i32::from(fields.y)
            && screen_y < i32::from(fields.bottom())
        {
            let anchor = Rect {
                y: u16::try_from(screen_y.max(i32::from(fields.y))).unwrap_or(fields.y),
                height: heights[index],
                ..fields
            };
            if form.is_enabled(group.focus) {
                form.hits.push((anchor, group.focus));
            }
            if group.focus == Focus::Repository {
                repository_anchor = Some(anchor);
            }
            if group.focus == Focus::Scope {
                scope_anchor = Some(anchor);
            }
            if group.focus.is_a_pill() && form.list_open() {
                pill_anchor = Some(anchor);
            }
        }
    }

    let target = frame.buffer_mut();
    for row in 0..fields.height {
        let source = form.scroll.saturating_add(row);
        if source >= content_height {
            break;
        }
        for column in 0..fields.width {
            target[(fields.x + column, fields.y + row)] = content[(column, source)].clone();
        }
    }
    if let Some((x, y)) = cursor
        && y >= form.scroll
        && y < form.scroll.saturating_add(fields.height)
    {
        frame.set_cursor_position((fields.x + x, fields.y + y - form.scroll));
    }
    if let Some(anchor) = repository_anchor
        && let Some(browser) = form.repositories.as_mut()
    {
        browser.draw(frame, fields, anchor, tick);
    }
    if let Some(anchor) = scope_anchor
        && let Some(scopes) = form.scopes.as_mut()
    {
        scopes.draw(frame, fields, anchor, tick);
    }
    if let Some(anchor) = pill_anchor {
        form.pill_rows = form.draw_list(frame, fields, anchor);
    }

    // The actions sit at the bottom right, where the eye ends up.
    let buttons = button::actions(
        frame,
        area,
        body.actions,
        &[
            button::Action::secondary("cancel").focused(form.focus == Focus::Cancel),
            button::Action::primary("save").focused(form.focus == Focus::Save),
        ],
    );
    for (rect, focus) in buttons.into_iter().zip([Focus::Cancel, Focus::Save]) {
        form.hits.push((rect, focus));
    }
}

pub(super) fn draw_group(
    buffer: &mut Buffer,
    area: Rect,
    form: &Form,
    group: &Group,
) -> Option<(u16, u16)> {
    let focused = form.focus == group.focus;
    match group.kind {
        Kind::Single => {
            let label = if group.focus == Focus::Scope {
                form.scope_label.as_str()
            } else {
                group.label
            };
            draw_label(buffer, area, label, focused);
            let field = match group.focus {
                Focus::Primary => &form.primary,
                _ => return None,
            };
            field.render(
                buffer,
                below_label(area),
                &FieldStyle {
                    focused,
                    title: None,
                    prefix: "",
                },
            )
        }
        Kind::Multi => {
            draw_label(buffer, area, group.label, focused);
            let text = match group.focus {
                Focus::Setup => &form.setup,
                Focus::Caches => &form.caches,
                Focus::Instructions => &form.instructions,
                _ => return None,
            };
            text.render_titled(buffer, below_label(area), focused, None)
        }
        Kind::Choice => {
            let label = if group.focus == Focus::Scope {
                form.scope_label.as_str()
            } else {
                group.label
            };
            // A pill beside its label rather than a value under it: a value you
            // can change reads as a control, and the page gets a line back for
            // every one of them.
            let value = match group.focus {
                Focus::Repository => {
                    let value = form.repository.value();
                    match project::repository_display(form.vcs_provider(), value) {
                        Some(display) => display,
                        None => "not chosen".to_owned(),
                    }
                }
                Focus::Scope => {
                    if form.has_tracker() {
                        let scope = form.scope.value();
                        if scope.trim().is_empty() {
                            "whole account".to_owned()
                        } else {
                            scope.to_owned()
                        }
                    } else {
                        "—".to_owned()
                    }
                }
                focus => form.pick_value(focus),
            };
            let width = match group.focus {
                Focus::Repository | Focus::Scope => {
                    // Its list is a page of its own, so the pill only says what
                    // the field holds and its key opens that page.
                    (value.chars().count() as u16 + 3).min(area.width)
                }
                focus => form.pill_width(focus).min(area.width),
            };
            let pill = Rect {
                x: area.right().saturating_sub(width),
                width,
                height: 1,
                ..area
            };
            match group.focus {
                Focus::Repository | Focus::Scope => {
                    select::shown_pill(buffer, pill, &value, focused)
                }
                focus => form.draw_pill(focus, buffer, pill, focused),
            }
            draw_label(
                buffer,
                Rect {
                    width: area.width.saturating_sub(width).saturating_sub(1),
                    height: 1,
                    ..area
                },
                label,
                focused,
            );
            None
        }
        Kind::Toggle => {
            let mark = if form.accepts_delegated_work {
                "[x]"
            } else {
                "[ ]"
            };
            let style = if focused {
                palette::primary().bold()
            } else {
                palette::muted()
            };
            Paragraph::new(Line::styled(format!("{mark} {}", group.label), style))
                .render(Rect { height: 1, ..area }, buffer);
            if area.height > 2 {
                Paragraph::new(vec![
                    Line::styled(
                        "If a remote and task tracker are connected, turn this on to let \
                         your task tracker start altitude agents on @mention and delegation.",
                        palette::muted(),
                    ),
                    Line::from(""),
                    Line::styled(
                        "This enables you to pilot your task entirely from your task tracker.",
                        palette::muted(),
                    ),
                ])
                .wrap(Wrap { trim: true })
                .render(
                    Rect {
                        y: area.y.saturating_add(2),
                        height: area.height.saturating_sub(2),
                        ..area
                    },
                    buffer,
                );
            }
            None
        }
    }
}

pub(super) fn draw_label(buffer: &mut Buffer, area: Rect, text: &str, focused: bool) {
    let style = if focused {
        palette::primary().bold()
    } else {
        palette::muted()
    };
    let mark = if focused { "› " } else { "  " };
    let room = usize::from(area.width).saturating_sub(mark.chars().count());
    Paragraph::new(Line::styled(
        format!("{mark}{}", truncate(text, room)),
        style,
    ))
    .render(
        Rect {
            height: area.height.min(1),
            ..area
        },
        buffer,
    );
}

pub(super) fn below_label(area: Rect) -> Rect {
    Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    }
}
