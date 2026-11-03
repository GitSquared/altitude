mod linear;
mod linear_auth;

pub use linear::Linear;
// The automation reads the same team out of Linear's issue, through the same parser.
pub(super) use linear::scope_of;
