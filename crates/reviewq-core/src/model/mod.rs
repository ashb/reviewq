//! Snapshot types, attention reasons, and the classifier that maps one to the
//! other.
//!
//! The types double as the on-disk fixture format, and the rendered reason
//! strings are a stable, user-visible API — so both are snapshot-tested.

mod classify;
mod reason;
mod snapshot;

pub use classify::{ClassifyCtx, Mention, ReviewRequest, Said, classify};
pub use reason::{Attention, AttentionReason, OnMyPr};
pub use snapshot::{MyState, PrSnapshot, PrState, ReviewerVerdict, ThreadState, Verdict};
