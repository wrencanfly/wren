//! Wren core: storage, ingestion, derived data and query engine.
//!
//! Wren is a git-compatible acceleration layer for agent-scale repositories,
//! inspired by Anthropic's Scion. See `docs/` for the design.

pub mod api;
pub mod blame;
pub mod bounded;
pub mod changes;
pub mod derived;
pub mod git_source;
pub mod gitproto;
pub mod graph;
pub mod grep;
pub mod index;
pub mod meta;
pub mod object;
pub mod oid;
pub mod pack;
pub mod pathindex;
pub mod renames;
pub mod repo;
pub mod segments;
pub mod snapshot;
pub mod sparse;
pub mod store;
#[doc(hidden)]
pub mod testpg;
pub mod treegen;
pub mod wire;
pub mod write;

pub use oid::Oid;

/// Error for a revision, changeset, object, path or repo that does not exist.
/// Servers map it to HTTP 404; every other error is the caller's fault (400).
#[derive(Debug)]
pub struct NotFound(pub String);

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotFound {}

pub(crate) fn not_found(msg: impl Into<String>) -> anyhow::Error {
    NotFound(msg.into()).into()
}

/// Error caused by the request itself (bad revision syntax, ambiguous name,
/// invalid argument). Servers map it to HTTP 400. Errors that are neither
/// `NotFound` nor `BadRequest` are server faults (500).
#[derive(Debug)]
pub struct BadRequest(pub String);

impl std::fmt::Display for BadRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadRequest {}

pub fn bad_request(msg: impl Into<String>) -> anyhow::Error {
    BadRequest(msg.into()).into()
}
pub use repo::Repo;
pub use store::ObjectStore;
