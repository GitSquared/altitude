mod draw;
mod form;
mod pages;
mod panel;
// The rows carry the types the panel hands back, so their module is reachable
// where those are: `View` from the settings view, and `Row` from the selection.
pub(in crate::tui) mod rows;
mod view;

pub(in crate::tui) use draw::{SettingsHit, draw};
pub(in crate::tui) use form::{Focus, Form, Target};
pub(in crate::tui) use pages::{
    hostname, remote_setup_lines, remote_tracker_setup_lines, tilde, tracker_dot,
};
pub(in crate::tui) use panel::{Depth, Panel, Section, Setting};
pub(in crate::tui) use rows::{CredentialKind, GitIdentity, RowKind};
