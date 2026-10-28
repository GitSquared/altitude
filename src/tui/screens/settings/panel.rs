//! The panel's shape: the lists it holds, what a preference is, and where the
//! focus is.

use super::form::Form;
use super::rows::{Row, View};
use crate::{
    credentials::{Service, Slot},
    tui::widgets::list,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Interface,
    Agents,
    VersionControl,
    Trackers,
    Projects,
    Remotes,
}

impl Section {
    pub const ALL: [Section; 6] = [
        Self::Interface,
        Self::Agents,
        Self::VersionControl,
        Self::Trackers,
        Self::Projects,
        Self::Remotes,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Interface => "Interface",
            Self::Agents => "Coding agents",
            Self::VersionControl => "Version control",
            Self::Trackers => "Task tracker",
            Self::Projects => "Projects",
            Self::Remotes => "Remotes",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Interface => "Adjust look and feel",
            Self::Agents => "The agents that write the code",
            Self::VersionControl => "Git and commit signing",
            Self::Trackers => "Keep your team in the loop",
            Self::Projects => "Organize work streams",
            Self::Remotes => "Automate and keep work going",
        }
    }

    /// Whether this list can grow and shrink.
    ///
    /// Every list but the preferences holds rows the operator adds and removes.
    pub fn grows(self) -> bool {
        self.slot().is_some() || matches!(self, Self::Projects | Self::Remotes)
    }

    /// The project slot this section's identities fill, when it is one of the
    /// service sections rather than a list of projects, remotes, or preferences.
    pub fn slot(self) -> Option<Slot> {
        match self {
            Self::Agents => Some(Slot::Agent),
            Self::VersionControl => Some(Slot::VersionControl),
            Self::Trackers => Some(Slot::Tracker),
            Self::Interface | Self::Projects | Self::Remotes => None,
        }
    }

    pub fn for_service(service: Service) -> Self {
        match service.slot() {
            Slot::Agent => Self::Agents,
            Slot::VersionControl => Self::VersionControl,
            Slot::Tracker => Self::Trackers,
        }
    }

    /// The services a new identity in this section can be, in the order the
    /// service picker offers them.
    pub fn services(self) -> Vec<Service> {
        Service::ALL
            .into_iter()
            .filter(|service| self.slot() == Some(service.slot()))
            .collect()
    }
}

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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Rail,
    List,
}

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

    pub fn selected(&self) -> usize {
        self.cursor.selected()
    }

    pub fn select_row(&mut self, index: usize) {
        self.cursor.select(index);
    }

    pub fn selected_row<'a>(&self, view: &'a View) -> Option<&'a Row> {
        view.rows.get(self.selected())
    }
}
