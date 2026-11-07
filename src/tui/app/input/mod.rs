use std::mem;

use crossterm::event::KeyEvent;

use crate::tui::widgets::select::{self, Menu};

use super::*;

mod forms;
mod list;
mod panel;
mod signals;

impl App {
    pub(in crate::tui) fn dismiss_keys(&mut self) {
        if let Foreground::Keys(behind) = mem::take(&mut self.foreground) {
            self.foreground = *behind;
        }
    }

    // What lies over the screen in front, if anything does. A menu belongs to the
    // screen that draws it, so a question or a page in front has none of them: it is
    // asked for every key whole, by the popup that owns it.
    fn open_menu(&mut self) -> Option<&mut dyn Menu> {
        match self.foreground {
            Foreground::Tasks => {
                select::opened([&mut self.task_filter as &mut dyn Menu, &mut self.task_sort])
            }
            Foreground::Signals => select::opened([
                &mut self.signal_filter as &mut dyn Menu,
                &mut self.signal_sort,
            ]),
            _ => None,
        }
    }

    // A menu is given a key before the screen under it, which is why this is asked
    // before the foreground is: a screen that reads a key first takes one the
    // operator aimed at the menu it can see.
    pub(in crate::tui) fn menu_key(&mut self, key: KeyEvent) -> bool {
        let Some(menu) = self.open_menu() else {
            return false;
        };
        menu.key(key);
        // The list a menu filters is rebuilt under the cursor, which is found again
        // by what it was on rather than by where it was.
        match self.foreground {
            Foreground::Tasks => self.apply_task_view(),
            Foreground::Signals => self.apply_signal_view(),
            _ => {}
        }
        true
    }
}
