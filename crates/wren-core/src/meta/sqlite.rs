//! SQLite backend: one file per repository, for a single server.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::{decode_oids, encode_oids, Backend, ChangesetRow, MetaLock, RefRow, WriteOp};
use crate::oid::Oid;

const SCHEMA: &str = r#"
-- A changeset is what a git commit says, minus the tree: the tree (and the
-- commit object itself) are derived from it. Byte fields are kept verbatim
-- so that the derived commit hashes to `oid`.
CREATE TABLE IF NOT EXISTS changesets(
    seq           INTEGER PRIMARY KEY AUTOINCREMENT,
    oid           BLOB NOT NULL UNIQUE,
    parents       BLOB NOT NULL,
    author        BLOB NOT NULL,
    committer     BLOB,
    extra_headers BLOB NOT NULL,
    message       BLOB NOT NULL,
    commit_time   INTEGER NOT NULL,
    changes       BLOB NOT NULL,
    -- Set only for ingested commits that derivation cannot reproduce.
    tree_override BLOB,
    raw_commit    BLOB
);
CREATE TABLE IF NOT EXISTS path_index(
    path TEXT NOT NULL,
    gen  INTEGER NOT NULL,
    oid  BLOB NOT NULL,
    PRIMARY KEY(path, gen, oid)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS refs(
    name   TEXT PRIMARY KEY,
    target BLOB NOT NULL,
    peeled BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS derived(
    kind  TEXT NOT NULL,
    key   BLOB NOT NULL,
    value BLOB NOT NULL,
    PRIMARY KEY(kind, key)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS kv(
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- Append-only logs (the audit log).
CREATE TABLE IF NOT EXISTS log(
    seq    INTEGER PRIMARY KEY AUTOINCREMENT,
    stream TEXT NOT NULL,
    data   BLOB NOT NULL
);
-- Superseded derived-data formats; nothing reads them any more.
DELETE FROM derived WHERE kind = 'blame';
"#;

/// Bumped when the schema changes incompatibly; older databases are rebuilt.
const SCHEMA_VERSION: i64 = 2;

pub struct SqliteMeta {
    path: std::path::PathBuf,
    pool: Mutex<Vec<Connection>>,
}

fn oid_col(b: Vec<u8>) -> rusqlite::Result<Oid> {
    Oid::from_bytes(&b).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, e.into())
    })
}

impl SqliteMeta {
    pub fn open(path: impl AsRef<Path>) -> Result<SqliteMeta> {
        let db = SqliteMeta {
            path: path.as_ref().to_path_buf(),
            pool: Mutex::new(Vec::new()),
        };
        db.with(|c| {
            let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if version < SCHEMA_VERSION {
                // v0.1 stored a tree pointer in each changeset. Everything
                // below is re-ingested from the object store on the next
                // sync; `kv` (upstream, HEAD) survives.
                c.execute_batch(
                    "DROP TABLE IF EXISTS changesets; DROP TABLE IF EXISTS path_index;
                     DROP TABLE IF EXISTS refs; DROP TABLE IF EXISTS derived;",
                )?;
            }
            c.execute_batch(SCHEMA)?;
            c.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
            Ok(())
        })?;
        Ok(db)
    }

    fn connect(&self) -> Result<Connection> {
        let c = Connection::open(&self.path)?;
        c.busy_timeout(std::time::Duration::from_secs(30))?;
        c.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-65536;",
        )?;
        Ok(c)
    }

    /// Run `f` with a pooled connection.
    pub fn with<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let conn = self.pool.lock().unwrap().pop();
        let mut conn = match conn {
            Some(c) => c,
            None => self.connect()?,
        };
        let r = f(&mut conn);
        let mut pool = self.pool.lock().unwrap();
        if pool.len() < 16 {
            pool.push(conn);
        }
        r
    }

    /// Run `f` inside an IMMEDIATE transaction.
    pub fn write<T>(&self, f: impl FnOnce(&rusqlite::Transaction) -> Result<T>) -> Result<T> {
        self.with(|c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let r = f(&tx)?;
            tx.commit()?;
            Ok(r)
        })
    }

    // ---- changesets ---------------------------------------------------------

    pub fn insert_changeset(tx: &rusqlite::Transaction, cs: &ChangesetRow) -> Result<()> {
        tx.prepare_cached(
            "INSERT OR IGNORE INTO changesets(oid, parents, author, committer, extra_headers,
                 message, commit_time, changes, tree_override, raw_commit)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?
        .execute(params![
            cs.oid.as_bytes(),
            encode_oids(&cs.parents),
            cs.author,
            cs.committer,
            cs.extra_headers,
            cs.message,
            cs.commit_time,
            cs.changes,
            cs.tree_override.as_ref().map(|t| t.as_bytes().to_vec()),
            cs.raw_commit,
        ])?;
        Ok(())
    }

    pub fn changeset(&self, oid: &Oid) -> Result<Option<ChangesetRow>> {
        self.with(|c| {
            Ok(c.prepare_cached(
                "SELECT seq, parents, author, committer, extra_headers, message,
                            commit_time, changes, tree_override, raw_commit
                     FROM changesets WHERE oid = ?1",
            )?
            .query_row([oid.as_bytes()], |r| {
                Ok(ChangesetRow {
                    seq: r.get(0)?,
                    oid: *oid,
                    parents: decode_oids(&r.get::<_, Vec<u8>>(1)?),
                    author: r.get(2)?,
                    committer: r.get(3)?,
                    extra_headers: r.get(4)?,
                    message: r.get(5)?,
                    commit_time: r.get(6)?,
                    changes: r.get(7)?,
                    tree_override: r.get::<_, Option<Vec<u8>>>(8)?.map(oid_col).transpose()?,
                    raw_commit: r.get(9)?,
                })
            })
            .optional()?)
        })
    }

    /// How many changesets carry a tree override and a raw-commit override:
    /// commits from git that derivation could not reproduce on its own.
    pub fn override_counts(&self) -> Result<(i64, i64)> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT COUNT(tree_override), COUNT(raw_commit) FROM changesets",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
    }

    /// Just the encoded change records of a changeset (see `changes::encode_changes`).
    pub fn changeset_changes(&self, oid: &Oid) -> Result<Option<Vec<u8>>> {
        self.with(|c| {
            Ok(
                c.prepare_cached("SELECT changes FROM changesets WHERE oid = ?1")?
                    .query_row([oid.as_bytes()], |r| r.get(0))
                    .optional()?,
            )
        })
    }

    pub fn parents(&self, oid: &Oid) -> Result<Option<Vec<Oid>>> {
        self.with(|c| {
            Ok(
                c.prepare_cached("SELECT parents FROM changesets WHERE oid = ?1")?
                    .query_row([oid.as_bytes()], |r| {
                        Ok(decode_oids(&r.get::<_, Vec<u8>>(0)?))
                    })
                    .optional()?,
            )
        })
    }

    pub fn has_changeset(&self, oid: &Oid) -> Result<bool> {
        self.with(|c| {
            Ok(c.prepare_cached("SELECT 1 FROM changesets WHERE oid = ?1")?
                .exists([oid.as_bytes()])?)
        })
    }

    pub fn changeset_count(&self) -> Result<i64> {
        self.with(|c| Ok(c.query_row("SELECT COUNT(*) FROM changesets", [], |r| r.get(0))?))
    }

    /// Changesets whose id starts with the given hex prefix (at most `limit`).
    pub fn resolve_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<Oid>> {
        let lo_hex = format!("{prefix:0<40}");
        let hi_hex = format!("{prefix:f<40}");
        let lo = Oid::from_hex(&lo_hex)?;
        let hi = Oid::from_hex(&hi_hex)?;
        self.with(|c| {
            let mut st = c.prepare_cached(
                "SELECT oid FROM changesets WHERE oid BETWEEN ?1 AND ?2 LIMIT ?3",
            )?;
            let rows = st.query_map(params![lo.as_bytes(), hi.as_bytes(), limit as i64], |r| {
                oid_col(r.get(0)?)
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    // ---- path index ---------------------------------------------------------

    pub fn insert_path_index(
        tx: &rusqlite::Transaction,
        path: &str,
        generation: u32,
        oid: &Oid,
    ) -> Result<()> {
        tx.prepare_cached("INSERT OR IGNORE INTO path_index(path, gen, oid) VALUES (?1, ?2, ?3)")?
            .execute(params![path, generation, oid.as_bytes()])?;
        Ok(())
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
        let (gen_cursor, oid_cursor) = after;
        let oid_cursor: Vec<u8> = oid_cursor.map(|o| o.0.to_vec()).unwrap_or_default();
        self.with(|c| {
            let mut st = c.prepare_cached(
                "SELECT gen, oid FROM path_index WHERE path = ?1 AND (gen < ?2 OR (gen = ?2 AND oid > ?3)) \
                 ORDER BY gen DESC, oid ASC LIMIT ?4",
            )?;
            let rows = st.query_map(params![path, gen_cursor, oid_cursor, limit as i64], |r| {
                Ok((r.get(0)?, oid_col(r.get(1)?)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    // ---- refs ---------------------------------------------------------------

    pub fn refs(&self) -> Result<Vec<RefRow>> {
        self.with(|c| {
            let mut st = c.prepare_cached("SELECT name, target, peeled FROM refs ORDER BY name")?;
            let rows = st.query_map([], |r| {
                Ok(RefRow {
                    name: r.get(0)?,
                    target: oid_col(r.get(1)?)?,
                    peeled: oid_col(r.get(2)?)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn get_ref(&self, name: &str) -> Result<Option<RefRow>> {
        self.with(|c| {
            Ok(
                c.prepare_cached("SELECT name, target, peeled FROM refs WHERE name = ?1")?
                    .query_row([name], |r| {
                        Ok(RefRow {
                            name: r.get(0)?,
                            target: oid_col(r.get(1)?)?,
                            peeled: oid_col(r.get(2)?)?,
                        })
                    })
                    .optional()?,
            )
        })
    }

    /// Compare-and-swap a single ref. `expected == None` means "must not
    /// exist"; `new == None` deletes. Returns false if the ref had moved.
    /// Refs are the only mutable state, and moving one never blocks the
    /// changeset log.
    pub fn cas_ref(
        &self,
        name: &str,
        expected: Option<Oid>,
        new: Option<(Oid, Oid)>,
    ) -> Result<bool> {
        self.write(|tx| {
            let cur: Option<Vec<u8>> = tx
                .prepare_cached("SELECT target FROM refs WHERE name = ?1")?
                .query_row([name], |r| r.get(0))
                .optional()?;
            let cur = cur.map(|b| Oid::from_bytes(&b)).transpose()?;
            if cur != expected {
                return Ok(false);
            }
            match new {
                Some((target, peeled)) => {
                    tx.prepare_cached(
                        "INSERT OR REPLACE INTO refs(name, target, peeled) VALUES (?1, ?2, ?3)",
                    )?
                    .execute(params![
                        name,
                        target.as_bytes(),
                        peeled.as_bytes()
                    ])?;
                }
                None => {
                    tx.prepare_cached("DELETE FROM refs WHERE name = ?1")?
                        .execute([name])?;
                }
            }
            Ok(true)
        })
    }

    // ---- derived data -------------------------------------------------------

    pub fn get_derived(&self, kind: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.with(|c| {
            Ok(
                c.prepare_cached("SELECT value FROM derived WHERE kind = ?1 AND key = ?2")?
                    .query_row(params![kind, key], |r| r.get(0))
                    .optional()?,
            )
        })
    }

    /// Keys of every `kind` value whose key starts with `prefix` (values are
    /// not read).
    pub fn derived_keys_with_prefix(&self, kind: &str, prefix: &[u8]) -> Result<Vec<Vec<u8>>> {
        // Keys are compared as blobs (memcmp), so the keys with this prefix
        // are those in [prefix, prefix + 0xff 0xff ...). Bound the range by
        // the smallest blob greater than every such key, if there is one.
        let mut hi = prefix.to_vec();
        while hi.last() == Some(&0xff) {
            hi.pop();
        }
        self.with(|c| {
            let keys = if let Some(last) = hi.last_mut() {
                *last += 1;
                let mut st = c.prepare_cached(
                    "SELECT key FROM derived WHERE kind = ?1 AND key >= ?2 AND key < ?3",
                )?;
                let rows = st.query_map(params![kind, prefix, hi], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<Vec<Vec<u8>>>>()?
            } else {
                let mut st =
                    c.prepare_cached("SELECT key FROM derived WHERE kind = ?1 AND key >= ?2")?;
                let rows = st.query_map(params![kind, prefix], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<Vec<Vec<u8>>>>()?
            };
            Ok(keys.into_iter().filter(|k| k.starts_with(prefix)).collect())
        })
    }

    pub fn derived_with_prefix(
        &self,
        kind: &str,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut hi = prefix.to_vec();
        while hi.last() == Some(&0xff) {
            hi.pop();
        }
        self.with(|c| {
            let map = |r: &rusqlite::Row| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?));
            let rows = if let Some(last) = hi.last_mut() {
                *last += 1;
                let mut st = c.prepare_cached(
                    "SELECT key, value FROM derived WHERE kind = ?1 AND key >= ?2 AND key < ?3 ORDER BY key",
                )?;
                let rows = st.query_map(params![kind, prefix, hi], map)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            } else {
                let mut st = c.prepare_cached(
                    "SELECT key, value FROM derived WHERE kind = ?1 AND key >= ?2 ORDER BY key",
                )?;
                let rows = st.query_map(params![kind, prefix], map)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            Ok(rows.into_iter().filter(|(k, _)| k.starts_with(prefix)).collect())
        })
    }

    /// Derived values are a pure function of immutable inputs, so concurrent
    /// writers always agree and the first write simply wins.
    pub fn put_derived(&self, kind: &str, key: &[u8], value: &[u8]) -> Result<()> {
        self.with(|c| {
            c.prepare_cached(
                "INSERT OR IGNORE INTO derived(kind, key, value) VALUES (?1, ?2, ?3)",
            )?
            .execute(params![kind, key, value])?;
            Ok(())
        })
    }

    pub fn put_derived_tx(
        tx: &rusqlite::Transaction,
        kind: &str,
        key: &[u8],
        value: &[u8],
    ) -> Result<()> {
        tx.prepare_cached("INSERT OR IGNORE INTO derived(kind, key, value) VALUES (?1, ?2, ?3)")?
            .execute(params![kind, key, value])?;
        Ok(())
    }

    // ---- kv -----------------------------------------------------------------

    pub fn get_kv(&self, key: &str) -> Result<Option<String>> {
        self.with(|c| {
            Ok(c.prepare_cached("SELECT value FROM kv WHERE key = ?1")?
                .query_row([key], |r| r.get(0))
                .optional()?)
        })
    }

    pub fn set_kv(&self, key: &str, value: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO kv(key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
            Ok(())
        })
    }
}

impl Backend for SqliteMeta {
    fn changeset(&self, oid: &Oid) -> Result<Option<ChangesetRow>> {
        SqliteMeta::changeset(self, oid)
    }
    fn override_counts(&self) -> Result<(i64, i64)> {
        SqliteMeta::override_counts(self)
    }
    fn overridden_changesets(&self, limit: usize) -> Result<Vec<Oid>> {
        self.with(|c| {
            let mut st = c.prepare_cached(
                "SELECT oid FROM changesets WHERE tree_override IS NOT NULL ORDER BY seq LIMIT ?1",
            )?;
            let rows = st.query_map([limit as i64], |r| oid_col(r.get(0)?))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
    fn changeset_changes(&self, oid: &Oid) -> Result<Option<Vec<u8>>> {
        SqliteMeta::changeset_changes(self, oid)
    }
    fn parents(&self, oid: &Oid) -> Result<Option<Vec<Oid>>> {
        SqliteMeta::parents(self, oid)
    }
    fn has_changeset(&self, oid: &Oid) -> Result<bool> {
        SqliteMeta::has_changeset(self, oid)
    }
    fn changeset_count(&self) -> Result<i64> {
        SqliteMeta::changeset_count(self)
    }
    fn resolve_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<Oid>> {
        SqliteMeta::resolve_prefix(self, prefix, limit)
    }
    fn path_changes(
        &self,
        path: &str,
        after: (u32, Option<Oid>),
        limit: usize,
    ) -> Result<Vec<(u32, Oid)>> {
        SqliteMeta::path_changes(self, path, after, limit)
    }
    fn refs(&self) -> Result<Vec<RefRow>> {
        SqliteMeta::refs(self)
    }
    fn get_ref(&self, name: &str) -> Result<Option<RefRow>> {
        SqliteMeta::get_ref(self, name)
    }
    fn cas_ref(&self, name: &str, expected: Option<Oid>, new: Option<(Oid, Oid)>) -> Result<bool> {
        SqliteMeta::cas_ref(self, name, expected, new)
    }
    fn get_derived(&self, kind: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        SqliteMeta::get_derived(self, kind, key)
    }
    fn derived_keys_with_prefix(&self, kind: &str, prefix: &[u8]) -> Result<Vec<Vec<u8>>> {
        SqliteMeta::derived_keys_with_prefix(self, kind, prefix)
    }
    fn derived_with_prefix(&self, kind: &str, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        SqliteMeta::derived_with_prefix(self, kind, prefix)
    }
    fn put_derived(&self, kind: &str, key: &[u8], value: &[u8]) -> Result<()> {
        SqliteMeta::put_derived(self, kind, key, value)
    }
    fn write_batch(&self, ops: &[WriteOp]) -> Result<()> {
        self.write(|tx| {
            for op in ops {
                match op {
                    WriteOp::Changeset(cs) => SqliteMeta::insert_changeset(tx, cs)?,
                    WriteOp::PathIndex {
                        path,
                        generation,
                        oid,
                    } => SqliteMeta::insert_path_index(tx, path, *generation, oid)?,
                    WriteOp::Derived { kind, key, value } => {
                        SqliteMeta::put_derived_tx(tx, kind, key, value)?
                    }
                    WriteOp::DeleteDerived { kind, key } => {
                        tx.prepare_cached("DELETE FROM derived WHERE kind = ?1 AND key = ?2")?
                            .execute(params![kind, key])?;
                    }
                }
            }
            Ok(())
        })
    }
    fn get_kv(&self, key: &str) -> Result<Option<String>> {
        SqliteMeta::get_kv(self, key)
    }
    fn set_kv(&self, key: &str, value: &str) -> Result<()> {
        SqliteMeta::set_kv(self, key, value)
    }
    /// One server per SQLite database: the repository's in-process lock is
    /// all the exclusion there is to have.
    fn paths_after(&self, after: &str, limit: usize) -> Result<Vec<String>> {
        self.with(|c| {
            let mut st = c.prepare_cached(
                "SELECT DISTINCT path FROM path_index WHERE path > ?1 ORDER BY path LIMIT ?2",
            )?;
            let rows = st
                .query_map(params![after, limit as i64], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows)
        })
    }

    fn kv_with_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        self.with(|c| {
            let mut st = c.prepare_cached(
                "SELECT key, value FROM kv WHERE substr(key, 1, length(?1)) = ?1 ORDER BY key",
            )?;
            let rows = st
                .query_map([prefix], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn append_log(&self, stream: &str, data: &[u8]) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO log(stream, data) VALUES (?1, ?2)",
                params![stream, data],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    fn read_log(&self, stream: &str, after: i64, limit: usize) -> Result<Vec<(i64, Vec<u8>)>> {
        self.with(|c| {
            let mut st = c.prepare_cached(
                "SELECT seq, data FROM log WHERE stream = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
            )?;
            let rows = st
                .query_map(params![stream, after, limit as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn lock(&self, _name: &str) -> Result<MetaLock> {
        Ok(MetaLock::none())
    }
}
