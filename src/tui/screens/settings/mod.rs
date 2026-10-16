//! The settings panel: what the interface shows, which agent drives a task and
//! how it signs in, the git identity tasks commit with, the environments a task
//! runs in, and the remotes it can be delegated to.
//!
//! Fullscreen, and three columns. The rail picks a list, the list picks a row,
//! and the third column says what is known about it. Only the column holding the
//! focus carries the inverted bar; the column ahead of it is drawn dimmed, as a
//! preview of where the next step goes, so nothing moves on screen when the
//! focus does.
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
