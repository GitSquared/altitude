//! The interface: what the operator sees, and what their keys reach.
//!
//! A screen fills the terminal and is the work itself; a pop-up sits over
//! whatever is there and asks one thing; a widget is drawn by both. What the
//! application is holding, and what it does with it, is in `app`. The loop that
//! reads events and draws frames is in `run`.

mod app;
mod boot;
mod popups;
mod run;
mod screens;
mod widgets;

pub use app::App;
pub use boot::{
    database as boot_database, isolation as boot_isolation, task_resume as boot_task_resume,
};
pub use run::run;
