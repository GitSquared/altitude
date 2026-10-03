//! Reaching a node from a client.
//!
//! Nodes are dialled by public key. The client↔node path needs no inbound port
//! and no public URL; tracker ingress is a separate concern. See
//! `design/decisions.md` (Q8) for the reasoning.
