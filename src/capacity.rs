//! How much work a node will run at once.
//!
//! A queue with a visible reason, against a limit computed from the host rather
//! than configured by hand. The per-task CPU and memory values are ceilings, not
//! pre-allocations. See `design/decisions.md` (Q14) for the reasoning.
