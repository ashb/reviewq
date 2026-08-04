//! Snapshot types and attention reasons.
//!
//! Classification itself — the pure function turning a snapshot into a list of
//! [`Attention`] entries — is not written yet. Its input and output types are,
//! because they double as the on-disk test fixture format and because the
//! rendered reason strings are a stable, user-visible API.

mod reason;
mod snapshot;

pub use reason::{Attention, AttentionReason};
pub use snapshot::{MyState, PrSnapshot, PrState, ThreadState, Verdict};
