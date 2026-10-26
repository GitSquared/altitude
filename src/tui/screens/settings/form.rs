//! The page that edits one project or remote, and the fields behind it.

use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, BorderType, Clear, Padding, Paragraph},
};

use crate::tui::widgets::{
    chrome, palette,
    text::{Field, FieldStyle, TextBox},
};

/// What an action applies to: the list it saves into, and what it replaces.
///
/// One type for the form and for the confirmation, because both need to know
/// which list they are about and which row they are about.
#[derive(Clone, PartialEq, Eq)]
pub enum Target {
    NewProject,
    /// An id when an existing project is being edited, so the fields this form
    /// does not show survive the save.
    Project {
        id: Option<String>,
        name: String,
    },
    NewRemote,
    Remote {
        original: String,
    },
}

impl Target {
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::NewRemote | Self::Remote { .. })
    }

    pub fn title(&self) -> &'static str {
        match self {
            Self::NewProject => "New project",
            Self::Project { .. } => "Edit project",
            Self::NewRemote => "Add remote",
            Self::Remote { .. } => "Edit remote",
        }
    }

    /// The first field's label: what it asks for.
    pub fn primary_label(&self) -> &'static str {
        if self.is_remote() { "Address" } else { "Name" }
    }

    /// What the form replaces, if anything.
    pub fn original(&self) -> Option<&str> {
        match self {
            Self::Remote { original } => Some(original),
            _ => None,
        }
    }

    /// The project this form edits, when it edits one.
    pub fn project_id(&self) -> Option<&str> {
        match self {
            Self::Project { id: Some(id), .. } => Some(id),
            _ => None,
        }
    }

    /// The fields tab walks, in order.
    pub fn fields(&self) -> &'static [Focus] {
        if self.is_remote() {
            &[Focus::Primary, Focus::Cancel, Focus::Save]
        } else {
            &[Focus::Primary, Focus::Setup, Focus::Cancel, Focus::Save]
        }
    }
}

/// Which part of the form holds the caret.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Primary,
    Setup,
    Cancel,
    Save,
}

impl Focus {
    /// Whether this part takes typed text, and so owns enter and escape.
    pub fn takes_text(self) -> bool {
        matches!(self, Self::Primary | Self::Setup)
    }
}

/// A page over the panel, holding one project or one remote.
pub struct Form {
    pub target: Target,
    /// The project's name, or the remote's address.
    pub primary: Field,
    pub setup: TextBox,
    pub focus: Focus,
}

impl Form {
    pub fn new(target: Target, primary: &str, setup: &str) -> Self {
        Self {
            target,
            primary: Field::with(primary),
            setup: TextBox::new(setup),
            focus: Focus::Primary,
        }
    }

    /// Move the caret to the next or previous field, wrapping.
    pub fn move_focus(&mut self, forward: bool) {
        let fields = self.target.fields();
        let at = fields
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % fields.len()
        } else {
            (at + fields.len() - 1) % fields.len()
        };
        self.focus = fields[next];
    }

    /// Whether the form is ready to save, and why not when it is not.
    pub fn refusal(&self) -> Option<&'static str> {
        if self.primary.is_blank() {
            return Some(if self.target.is_remote() {
                "an address is needed"
            } else {
                "a name is needed"
            });
        }
        None
    }
}

pub(super) fn draw_form(frame: &mut Frame, area: Rect, form: &Form) {
    let area = if form.target.is_remote() {
        chrome::centered(area, 45, chrome::modal_height(5))
    } else {
        chrome::centered(area, 65, chrome::modal_height(14))
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
    // Four rows hold the first field and its label; whatever a short window
    // leaves the rest takes, so a small terminal loses the setup box rather than
    // the whole page.
    let named = Rect {
        height: fields.height.min(4),
        ..fields
    };

    draw_label(frame, named, form.target.primary_label());
    form.primary.draw(
        frame,
        field_box(named),
        &FieldStyle {
            focused: form.focus == Focus::Primary,
            title: None,
            prefix: "",
        },
    );

    if !form.target.is_remote() {
        let setup = Rect {
            y: named.bottom(),
            height: fields.height.saturating_sub(named.height),
            ..fields
        };
        draw_label(frame, setup, "Setup command");
        form.setup
            .draw(frame, field_box(setup), form.focus == Focus::Setup);
    }

    // The actions sit at the bottom right, where the eye ends up.
    chrome::actions(
        frame,
        area,
        body.actions,
        &[
            chrome::Action::secondary("cancel").focused(form.focus == Focus::Cancel),
            chrome::Action::primary("save").focused(form.focus == Focus::Save),
        ],
    );
}

fn draw_label(frame: &mut Frame, area: Rect, text: &str) {
    frame.render_widget(
        Paragraph::new(Line::styled(text, palette::muted())),
        Rect {
            height: area.height.min(1),
            ..area
        },
    );
}

/// The box a field's input is drawn in, one row below its label.
fn field_box(area: Rect) -> Rect {
    Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    }
}
