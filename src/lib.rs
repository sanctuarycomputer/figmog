#![recursion_limit = "256"]

//! figmog — a fold-backed local mirror of one Figma file.
//!
//! A sync engine pulls the file when it changes (change detection on the
//! cheap Tier-3 metadata endpoint, one Tier-1 fetch per real change) and a
//! `KeyedStream` upsert-diffs every node into materialized indexes. The CLI
//! reads those indexes locally: zero Figma calls, zero rate limits.
//!
//! See `docs/superpowers/specs/2026-08-15-figmog-build-design.md`.

pub mod api;
pub mod cache;
pub mod cli;
mod dispatch;
pub mod flatten;
pub mod ident;
pub mod mcp;
pub mod model;
mod proxy;
pub mod query;
pub(crate) mod serve;
mod sessions;
pub mod store;
pub mod upstream;
pub mod vars;
pub mod watch;
