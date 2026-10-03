//! Moving a task capsule between nodes.
//!
//! A move is a move: the source relinquishes liveness before the payload leaves,
//! and the destination becomes live only once it holds everything. The only
//! ambiguous moment is the final acknowledgement, and it resolves to
//! fail-stopped rather than a guess.
//!
//! Pure state machine. No I/O, no database, no network. See
//! `design/decisions.md` (Q4, Q12) for the reasoning.
