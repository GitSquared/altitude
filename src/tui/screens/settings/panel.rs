//! The panel's shape: the lists it holds, what a preference is, and where the
//! focus is.

use super::form::Form;
use super::rows::{Row, View};
use crate::tui::widgets::list;

/// The lists the panel holds, in the order the rail shows them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Interface,
    Agents,
    VersionControl,
    Trackers,
    Environments,
    Remotes,
}

impl Section {
    pub const ALL: [Section; 6] = [
        Self::Interface,
        Self::Agents,
        Self::VersionControl,
        Self::Trackers,
        Self::Environments,
        Self::Remotes,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Interface => "Interface",
            Self::Agents => "Agents",
            Self::VersionControl => "Version control",
            Self::Trackers => "Task tracker",
            Self::Environments => "Environments",
            Self::Remotes => "Remotes",
        }
    }

    /// One line under the section's name in the rail.
    pub fn description(self) -> &'static str {
        match self {
            Self::Interface => "Adjust look and feel",
            Self::Agents => "Connect your favorite",
            Self::VersionControl => "Git and commit signing",
            Self::Trackers => "Keep your team in the loop",
            Self::Environments => "Give agents more tools",
            Self::Remotes => "Automate and keep work going",
        }
    }

    /// Whether this list can grow and shrink.
    ///
    /// Only the environments and the remotes are the operator's to add and
    /// remove: the other lists are what the installation already holds.
    pub fn grows(self) -> bool {
        matches!(self, Self::Environments | Self::Remotes)
    }

    /// The credential profiles this section asks about, when it asks about any.
    ///
    /// A profile's name is what decides which list it belongs to, so the prefix
    /// that matches it is the section's own fact rather than a caller's.
    pub fn profile_prefix(self) -> Option<&'static str> {
        match self {
            Self::Agents => Some("agents/"),
            Self::VersionControl => Some("vcs/"),
            _ => None,
        }
    }
}

/// A preference that is on or off.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    KeyboardHints,
    Animations,
}

impl Setting {
    pub fn label(self) -> &'static str {
        match self {
            Self::KeyboardHints => "Keyboard hints",
            Self::Animations => "Animations",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::KeyboardHints => {
                "List the keys that work, at the bottom of the screen. Press ? at any time to \
                 see them, whether or not this is on."
            }
            Self::Animations => "Enable or disable terminal interface animations.",
        }
    }
}

/// Whether the focus is in the rail or the list.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Rail,
    List,
}

/// The panel's state: where the focus is, and whatever is drawn over it.
pub struct Panel {
    pub section: Section,
    /// The list's selection and where its view starts, so a long list can be
    /// stepped through and scrolled like any other.
    pub cursor: list::Cursor,
    pub depth: Depth,
    pub form: Option<Form>,
    pub detail_scroll: u16,
}

impl Panel {
    pub fn new() -> Self {
        Self {
            section: Section::ALL[0],
            cursor: list::Cursor::default(),
            // The rail, so the panel opens on its shape rather than on the
            // details of whatever row happens to be first.
            depth: Depth::Rail,
            form: None,
            detail_scroll: 0,
        }
    }
}

impl Default for Panel {
    fn default() -> Self {
        Self::new()
    }
}

impl Panel {
    /// Move within the focused list, which at the rail means the section.
    pub fn move_selection(&mut self, forward: bool, rows: usize) {
        self.detail_scroll = 0;
        match self.depth {
            Depth::Rail => {
                let at = Section::ALL
                    .iter()
                    .position(|section| *section == self.section)
                    .unwrap_or(0);
                let next = if forward {
                    (at + 1) % Section::ALL.len()
                } else {
                    (at + Section::ALL.len() - 1) % Section::ALL.len()
                };
                self.section = Section::ALL[next];
                self.cursor.reset();
            }
            Depth::List => self.cursor.move_by(forward, rows),
        }
    }

    /// Where the list's cursor is.
    pub fn selected(&self) -> usize {
        self.cursor.selected()
    }

    /// Put the list's cursor on a row.
    pub fn select_row(&mut self, index: usize) {
        self.cursor.select(index);
    }

    /// The row the focus is on, in the list the rail has selected.
    pub fn selected_row<'a>(&self, view: &'a View) -> Option<&'a Row> {
        view.rows.get(self.selected())
    }
}
