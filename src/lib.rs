#![recursion_limit = "256"]

//! figmog — a fold-backed local mirror of one Figma file.
//!
//! A sync engine pulls the file when it changes (change detection on the
//! cheap Tier-3 metadata endpoint, one Tier-1 fetch per real change) and a
//! `KeyedStream` upsert-diffs every node into materialized indexes. The CLI
//! reads those indexes locally: zero Figma calls, zero rate limits.
//!
//! See `docs/history/2026-08-15-figmog-build-design.md`.

pub(crate) mod api;
pub mod cache;
pub mod cli;
mod dispatch;
pub mod flatten;
pub(crate) mod ident;
pub(crate) mod mcp;
pub mod model;
mod proxy;
pub(crate) mod query;
mod serve;
mod sessions;
pub mod store;
pub(crate) mod upstream;
pub mod vars;
pub(crate) mod watch;
