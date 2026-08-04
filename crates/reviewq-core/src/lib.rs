//! Pure queue logic for reviewq.
//!
//! Nothing in this crate performs IO or knows about GitHub, SQLite or async.
//! Callers convert their own types into [`model`] snapshots at the boundary,
//! run the pure logic, and render or persist the result themselves.
//!
//! The point of the separation is the project's central invariant: **every
//! queue item carries a machine-generated reason naming the rule that produced
//! it.** Keeping that logic in a crate with no IO in its dependency graph makes
//! the invariant testable exhaustively and cheaply.

pub mod model;
