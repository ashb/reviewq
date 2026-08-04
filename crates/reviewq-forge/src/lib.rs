//! Forge access for reviewq.
//!
//! Everything that knows how to reach a pull request's host lives here: which
//! host a repo is on, which adapter speaks to it, where its token comes from,
//! and — today — the GitHub adapter itself. The layers above deal in resolved
//! [`ForgeHost`]s and plain data types and never touch a wire format.
//!
//! There is deliberately no `Forge` trait yet. GitHub is the only provider with
//! an adapter, and the shape a trait should take only becomes clear once
//! ingestion exists to constrain it. When a second provider arrives, the trait
//! and its adapters slot in beside [`github`] without the crates above changing.

mod host;

pub mod github;

pub use host::{
    DEFAULT_HOST, ForgeHost, ForgeTable, Token, TokenSource, resolve_host, resolve_token,
};
