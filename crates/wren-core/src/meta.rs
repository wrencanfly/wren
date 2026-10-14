//! Metadata database (SQLite). Plays the role Spanner plays for Scion: the
//! append-only changeset log, refs (moved only by compare-and-swap), the path
//! index, and the generic derived-data table.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::oid::Oid;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS changesets(
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    oid         BLOB NOT NULL UNIQUE,
    tree        BLOB NOT NULL,
    parents     BLOB NOT NULL,
    commit_time INTEGER NOT NULL,
    changes     BLOB NOT NULL
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
"#;

pub struct MetaDb {
    path: std::path::PathBuf,
    pool: Mutex<Vec<Connection>>,
}

#[derive(Clone, Debug)]
pub struct ChangesetRow {
    pub seq: i64,
    pub oid: Oid,
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub commit_time: i64,
    pub changes: Vec<u8>,
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

fn oid_col(b: Vec<u8>) -> rusqlite::Result<Oid> {
    Oid::from_bytes(&b).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, e.into())
    })
}

impl MetaDb {
    pub fn open(path: impl AsRef<Path>) -> Result<MetaDb> {
        let db = MetaDb {
            path: path.as_ref().to_path_buf(),
            pool: Mutex::new(Vec::new()),
        };
        db.with(|c| Ok(c.execute_batch(SCHEMA)?))?;
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

    pub fn insert_changeset(
        tx: &rusqlite::Transaction,
        oid: &Oid,
        tree: &Oid,
        parents: &[Oid],
        commit_time: i64,
        changes: &[u8],
    ) -> Result<()> {
        tx.prepare_cached(
            "INSERT OR IGNORE INTO changesets(oid, tree, parents, commit_time, changes) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![oid.as_bytes(), tree.as_bytes(), encode_oids(parents), commit_time, changes])?;
        Ok(())
    }

    pub fn changeset(&self, oid: &Oid) -> Result<Option<ChangesetRow>> {
        self.with(|c| {
            Ok(c.prepare_cached("SELECT seq, oid, tree, parents, commit_time, changes FROM changesets WHERE oid = ?1")?
                .query_row([oid.as_bytes()], |r| {
                    Ok(ChangesetRow {
                        seq: r.get(0)?,
                        oid: oid_col(r.get(1)?)?,
                        tree: oid_col(r.get(2)?)?,
                        parents: decode_oids(&r.get::<_, Vec<u8>>(3)?),
                        commit_time: r.get(4)?,
                        changes: r.get(5)?,
                    })
                })
                .optional()?)
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
