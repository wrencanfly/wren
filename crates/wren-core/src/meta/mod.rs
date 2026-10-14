//! Metadata store. Plays the role Spanner plays for Scion: the append-only
//! changeset log, refs (moved only by compare-and-swap), the path index, and
//! the generic derived-data table.
//!
//! Two backends behind one interface:
//!
//! * SQLite (`sqlite`): a file per repository, for a single server.
//! * Postgres (`pg`): shared by any number of stateless servers. Each
//!   repository is a schema. Ref moves stay single-row compare-and-swaps,
//!   inserts of changesets and derived values are "insert if absent", and
//!   the operations that must not run concurrently across servers (syncing
//!   the git mirror, pushing to the host) take an advisory lock.
//!
//! The interface needs only these semantics, which is what lets it move to
//! any store that has them (the design names Spanner's).

mod pg;
mod sqlite;

use std::path::Path;

use anyhow::Result;

use crate::oid::Oid;

#[derive(Clone, Debug)]
pub struct ChangesetRow {
    pub seq: i64,
    pub oid: Oid,
    pub parents: Vec<Oid>,
    /// Header values as git writes them: `Name <email> 1700000000 +0100`.
    pub author: Vec<u8>,
    /// `None` means "same as the author".
    pub committer: Option<Vec<u8>>,
    /// Further commit headers (`encoding`, `mergetag`, `gpgsig`, ...),
    /// verbatim; see `object::RawCommit`.
    pub extra_headers: Vec<u8>,
    pub message: Vec<u8>,
    pub commit_time: i64,
    /// Encoded change records relative to the first parent.
    pub changes: Vec<u8>,
    /// The git tree, for an ingested commit whose tree is not what
    /// `treegen::apply` derives (non-canonical trees).
    pub tree_override: Option<Oid>,
    /// The git commit bytes, for an ingested commit that does not split
    /// canonically into the fields above.
    pub raw_commit: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefRow {
    pub name: String,
    /// What the ref points at (a commit, or an annotated tag object).
    pub target: Oid,
    /// The commit it ultimately names.
    pub peeled: Oid,
}

pub fn encode_oids(oids: &[Oid]) -> Vec<u8> {
    oids.iter().flat_map(|o| o.0).collect()
}

pub fn decode_oids(b: &[u8]) -> Vec<Oid> {
    b.as_chunks::<20>()
        .0
        .iter()
        .map(|c| Oid::from_bytes(c).unwrap())
        .collect()
}

/// One write in an atomic batch. Every insert is "if absent": rows are
/// immutable once written, so a repeated write is a no-op.
pub enum WriteOp<'a> {
    Changeset(&'a ChangesetRow),
    PathIndex {
        path: &'a str,
        generation: u32,
        oid: &'a Oid,
    },
    Derived {
        kind: &'a str,
        key: &'a [u8],
        value: &'a [u8],
    },
}

/// A held exclusive lock (see [`MetaDb::lock`]); released on drop.
pub struct MetaLock(#[allow(dead_code)] Option<Box<dyn Send>>);

impl MetaLock {
    pub(crate) fn none() -> MetaLock {
        MetaLock(None)
    }
}

/// Where a repository's metadata lives.
#[derive(Clone, Debug, Default)]
pub enum MetaConfig {
    /// `meta.sqlite` in the repository's directory.
    #[default]
    Sqlite,
    /// A Postgres connection string (`postgres://...` or `host=... user=...`).
    Postgres(String),
}

pub(crate) trait Backend: Send + Sync {
    fn changeset(&self, oid: &Oid) -> Result<Option<ChangesetRow>>;
    fn override_counts(&self) -> Result<(i64, i64)>;
    fn overridden_changesets(&self, limit: usize) -> Result<Vec<Oid>>;
    fn changeset_changes(&self, oid: &Oid) -> Result<Option<Vec<u8>>>;
    fn parents(&self, oid: &Oid) -> Result<Option<Vec<Oid>>>;
    fn has_changeset(&self, oid: &Oid) -> Result<bool>;
    fn changeset_count(&self) -> Result<i64>;
    fn resolve_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<Oid>>;
    fn path_changes(
        &self,
        path: &str,
        after: (u32, Option<Oid>),
        limit: usize,
    ) -> Result<Vec<(u32, Oid)>>;
    fn refs(&self) -> Result<Vec<RefRow>>;
    fn get_ref(&self, name: &str) -> Result<Option<RefRow>>;
    fn cas_ref(&self, name: &str, expected: Option<Oid>, new: Option<(Oid, Oid)>) -> Result<bool>;
    fn get_derived(&self, kind: &str, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn derived_keys_with_prefix(&self, kind: &str, prefix: &[u8]) -> Result<Vec<Vec<u8>>>;
    fn put_derived(&self, kind: &str, key: &[u8], value: &[u8]) -> Result<()>;
    fn write_batch(&self, ops: &[WriteOp]) -> Result<()>;
    fn get_kv(&self, key: &str) -> Result<Option<String>>;
    fn set_kv(&self, key: &str, value: &str) -> Result<()>;
    fn lock(&self, name: &str) -> Result<MetaLock>;
}

pub struct MetaDb {
    backend: Box<dyn Backend>,
}

impl MetaDb {
    /// SQLite at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<MetaDb> {
        Ok(MetaDb {
            backend: Box::new(sqlite::SqliteMeta::open(path)?),
        })
    }

    /// The metadata of repository `repo` (whose directory is `dir`).
    pub fn open_config(config: &MetaConfig, dir: &Path, repo: &str) -> Result<MetaDb> {
        Ok(MetaDb {
            backend: match config {
                MetaConfig::Sqlite => Box::new(sqlite::SqliteMeta::open(dir.join("meta.sqlite"))?),
                MetaConfig::Postgres(url) => Box::new(pg::PgMeta::open(url, repo)?),
            },
        })
    }

    pub fn changeset(&self, oid: &Oid) -> Result<Option<ChangesetRow>> {
        self.backend.changeset(oid)
    }
    /// How many changesets carry a tree override and a raw-commit override:
    /// commits from git that derivation could not reproduce on its own.
    pub fn override_counts(&self) -> Result<(i64, i64)> {
        self.backend.override_counts()
    }
    /// The first `limit` changesets (in ingest order) with a tree override.
    pub fn overridden_changesets(&self, limit: usize) -> Result<Vec<Oid>> {
        self.backend.overridden_changesets(limit)
    }
    /// Just the encoded change records of a changeset (see `changes::encode_changes`).
    pub fn changeset_changes(&self, oid: &Oid) -> Result<Option<Vec<u8>>> {
        self.backend.changeset_changes(oid)
    }
    pub fn parents(&self, oid: &Oid) -> Result<Option<Vec<Oid>>> {
        self.backend.parents(oid)
    }
    pub fn has_changeset(&self, oid: &Oid) -> Result<bool> {
        self.backend.has_changeset(oid)
    }
    pub fn changeset_count(&self) -> Result<i64> {
        self.backend.changeset_count()
    }
    /// Changesets whose id starts with the given hex prefix (at most `limit`).
    pub fn resolve_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<Oid>> {
        self.backend.resolve_prefix(prefix, limit)
    }
    /// Commits that changed `path` (relative to their first parent), ordered
    /// by (generation desc, oid asc), strictly after the keyset cursor
    /// `after`. Pass `(max_gen, None)` to start at generation `max_gen`.
    pub fn path_changes(
        &self,
        path: &str,
        after: (u32, Option<Oid>),
        limit: usize,
    ) -> Result<Vec<(u32, Oid)>> {
        self.backend.path_changes(path, after, limit)
    }
    pub fn refs(&self) -> Result<Vec<RefRow>> {
        self.backend.refs()
    }
    pub fn get_ref(&self, name: &str) -> Result<Option<RefRow>> {
        self.backend.get_ref(name)
    }
    /// Move `name` from `expected` to `new` (`None`: absent / delete) if it
    /// still has `expected`. Returns whether it moved.
    pub fn cas_ref(
        &self,
        name: &str,
        expected: Option<Oid>,
        new: Option<(Oid, Oid)>,
    ) -> Result<bool> {
        self.backend.cas_ref(name, expected, new)
    }
    pub fn get_derived(&self, kind: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.backend.get_derived(kind, key)
    }
    /// Keys of every `kind` value whose key starts with `prefix` (values are
    /// not read).
    pub fn derived_keys_with_prefix(&self, kind: &str, prefix: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.backend.derived_keys_with_prefix(kind, prefix)
    }
    /// Derived values are a pure function of immutable inputs, so concurrent
    /// writers always agree and the first write simply wins.
    pub fn put_derived(&self, kind: &str, key: &[u8], value: &[u8]) -> Result<()> {
        self.backend.put_derived(kind, key, value)
    }
    /// Apply `ops` atomically.
    pub fn write_batch(&self, ops: &[WriteOp]) -> Result<()> {
        self.backend.write_batch(ops)
    }
    pub fn get_kv(&self, key: &str) -> Result<Option<String>> {
        self.backend.get_kv(key)
    }
    pub fn set_kv(&self, key: &str, value: &str) -> Result<()> {
        self.backend.set_kv(key, value)
    }
    /// Exclusive lock `name` across every server sharing this metadata (a
    /// no-op for SQLite, which has one server). Held until the guard drops.
    pub fn lock(&self, name: &str) -> Result<MetaLock> {
        self.backend.lock(name)
    }
}
