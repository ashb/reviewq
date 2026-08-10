//! What the CLI and the interface both do, so neither has to depend on the other.
//!
//! Both read config, find the ledger, resolve a bare PR number to the repo it
//! lives on, run a sync, mark a PR done, and describe a PR's history in the same
//! words. None of that is presentation, so it lives here rather than in whichever
//! frontend needed it first.
//!
//! It is *not* a facade over the crates below. A frontend still depends on
//! `reviewq-ledger` for the queue it reads and on `reviewq-core` for the model
//! those rows are made of, and re-exporting those through here would add a layer
//! of indirection to change nothing about who knows what. What this crate owns is
//! shared *behaviour*; the shared *types* stay where they are defined.
//!
//! The one boundary it does draw is output. Nothing here writes to stdout or
//! stderr — a sync reports what it is doing through [`sync::SyncProgress`], and
//! the frontend decides whether that becomes a progress line, a status bar, or
//! nothing at all.

#[cfg(test)]
mod fake_forge;

pub mod actions;
pub mod config;
pub mod paths;
pub mod present;
pub mod resolve;
pub mod review;
pub mod sync;
