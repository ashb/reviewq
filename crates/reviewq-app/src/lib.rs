//! Everything reviewq does that isn't a way of showing it.
//!
//! The CLI and the TUI both need to read config, find the ledger on disk,
//! resolve a bare PR number to the repo it lives on, and run a sync. None of
//! that is presentation, so none of it belongs in a frontend: it lives here,
//! and each frontend depends on this crate rather than on the other.
//!
//! The boundary is drawn at output. Nothing in this crate writes to stdout or
//! stderr — a sync reports what it's doing through [`sync::SyncProgress`], and
//! the frontend decides whether that becomes a progress line, a status bar, or
//! nothing at all.

#[cfg(test)]
mod fake_forge;

pub mod actions;
pub mod config;
pub mod paths;
pub mod resolve;
pub mod review;
pub mod sync;
