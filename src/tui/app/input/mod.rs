use std::mem;

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
}
