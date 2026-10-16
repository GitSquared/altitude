//! The settings panel: what the interface shows, which agent drives a task and
//! how it signs in, the git identity tasks commit with, the environments a task
//! runs in, and the remotes it can be delegated to.
//!
//! Fullscreen, and three columns. The rail picks a list and the list picks a
//! row. The third column is not a place the focus goes: it only says what is
//! known about the row the list has selected, and stays readable while the list
//! is being used.
//!
//! The base environment is built in and therefore has no form: selecting it
//! describes it instead, which is the only place an operator can see what a
//! sandbox is actually made of.

mod draw;
mod form;
mod pages;
mod panel;
// The rows carry the types the panel hands back, so their module is reachable
// where those are: `View` from the settings view, and `Row` from the selection.
pub(in crate::tui) mod rows;
mod view;

pub(in crate::tui) use draw::draw;
pub(in crate::tui) use form::{Focus, Form, Target};
pub(in crate::tui) use pages::{remote_setup_lines, remote_tracker_setup_lines, tracker_dot};
pub(in crate::tui) use panel::{Depth, Panel, Section, Setting};
pub(in crate::tui) use rows::{CredentialKind, GitIdentity, RowKind};
