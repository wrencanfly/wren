//! Postgres backend: metadata shared by any number of stateless servers.
//!
//! Each repository lives in its own schema (`wren_<name>`), with the same
//! tables as the SQLite backend. Queries run on a small pool of worker
//! threads, each owning one connection with its prepared statements. (The
//! synchronous `postgres` client runs its own runtime, so it is kept off
//! the server's async threads.)
//!
//! Semantics the rest of Wren relies on:
//! * a ref moves with one conditional `UPDATE`/`INSERT`/`DELETE` that
//!   affects one row or none: a compare-and-swap;
//! * changesets, path-index rows and derived values are inserted with
//!   `ON CONFLICT DO NOTHING`: immutable, first write wins;
//! * a write batch is one transaction;
//! * `lock` is a session-level advisory lock on a connection of its own.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use postgres::types::ToSql;
use postgres::{Client, NoTls, Row, Statement};

use super::{decode_oids, encode_oids, Backend, ChangesetRow, MetaLock, RefRow, WriteOp};
use crate::oid::Oid;

const WORKERS: usize = 8;
/// Bumped when the schema changes incompatibly; older schemas are rebuilt.
const SCHEMA_VERSION: &str = "2";

/// A connection and its prepared statements, keyed by SQL text.
struct Conn {
    client: Client,
    stmts: HashMap<String, Statement>,
}

impl Conn {
    fn stmt(&mut self, sql: &str) -> Result<Statement> {
        if let Some(s) = self.stmts.get(sql) {
            return Ok(s.clone());
        }
        let s = self.client.prepare(sql)?;
        self.stmts.insert(sql.to_string(), s.clone());
        Ok(s)
    }
    fn query(&mut self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>> {
        let st = self.stmt(sql)?;
        Ok(self.client.query(&st, params)?)
    }
    fn execute(&mut self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64> {
        let st = self.stmt(sql)?;
        Ok(self.client.execute(&st, params)?)
    }
}

/// Work for a pool thread: given its connection, or the error from
/// connecting.
type Job = Box<dyn FnOnce(Result<&mut Conn, String>) + Send>;

pub struct PgMeta {
    url: String,
    /// `wren_<name>`, already quoted for use in SQL.
    schema: String,
    jobs: Mutex<Sender<Job>>,
}

fn schema_name(repo: &str) -> String {
    let clean: String = repo
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    // Different names can clean to the same text ("a-b", "a_b"): add a
    // short hash of the original name.
    let h = Oid::hash_object("blob", repo.as_bytes()).to_hex();
    format!("\"wren_{}_{}\"", clean, &h[..8])
}

fn oid(b: Vec<u8>) -> Result<Oid> {
    Oid::from_bytes(&b)
}

impl PgMeta {
    pub fn open(url: &str, repo: &str) -> Result<PgMeta> {
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..WORKERS {
            let rx = rx.clone();
            let url = url.to_string();
            std::thread::Builder::new()
                .name(format!("wren-pg-{i}"))
                .spawn(move || worker(url, rx))?;
        }
        let db = PgMeta {
            url: url.to_string(),
            schema: schema_name(repo),
            jobs: Mutex::new(tx),
        };
        db.create_schema()?;
        Ok(db)
    }

    /// Run `f` on a worker's connection and wait for its result.
    fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Conn) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (rtx, rrx) = channel();
        let job: Job = Box::new(move |c| {
            let _ = rtx.send(c.map_err(|e| anyhow!(e)).and_then(f));
        });
        self.jobs
            .lock()
            .unwrap()
            .send(job)
            .map_err(|_| anyhow!("postgres workers are gone"))?;
        rrx.recv().map_err(|_| anyhow!("postgres worker failed"))?
    }

    /// SQL with `{s}` replaced by this repository's schema.
    fn sql(&self, q: &str) -> String {
        q.replace("{s}", &self.schema)
    }

    fn create_schema(&self) -> Result<()> {
        let s = self.schema.clone();
        let ddl = self.sql(
            "CREATE SCHEMA IF NOT EXISTS {s};
             CREATE TABLE IF NOT EXISTS {s}.kv(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        );
        let tables = self.sql(
            "CREATE TABLE IF NOT EXISTS {s}.changesets(
                 seq           BIGSERIAL PRIMARY KEY,
                 oid           BYTEA NOT NULL UNIQUE,
                 parents       BYTEA NOT NULL,
                 author        BYTEA NOT NULL,
                 committer     BYTEA,
                 extra_headers BYTEA NOT NULL,
                 message       BYTEA NOT NULL,
                 commit_time   BIGINT NOT NULL,
                 changes       BYTEA NOT NULL,
                 tree_override BYTEA,
                 raw_commit    BYTEA);
             CREATE TABLE IF NOT EXISTS {s}.path_index(
                 path TEXT NOT NULL, gen BIGINT NOT NULL, oid BYTEA NOT NULL,
                 PRIMARY KEY(path, gen, oid));
             CREATE TABLE IF NOT EXISTS {s}.refs(
                 name TEXT PRIMARY KEY, target BYTEA NOT NULL, peeled BYTEA NOT NULL);
             CREATE TABLE IF NOT EXISTS {s}.derived(
                 kind TEXT NOT NULL, key BYTEA NOT NULL, value BYTEA NOT NULL,
                 PRIMARY KEY(kind, key));
             CREATE TABLE IF NOT EXISTS {s}.log(
                 seq BIGSERIAL PRIMARY KEY, stream TEXT NOT NULL, data BYTEA NOT NULL);",
        );
        let drop = self.sql(
            "DROP TABLE IF EXISTS {s}.changesets; DROP TABLE IF EXISTS {s}.path_index;
             DROP TABLE IF EXISTS {s}.refs; DROP TABLE IF EXISTS {s}.derived;
             DROP TABLE IF EXISTS {s}.log;",
        );
        let version_q = self.sql("SELECT value FROM {s}.kv WHERE key = 'schema_version'");
        let set_version = self.sql(
            "INSERT INTO {s}.kv(key, value) VALUES ('schema_version', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        );
        self.run(move |c| {
            // Servers starting together must not race on the DDL.
            let mut tx = c.client.transaction()?;
            tx.execute("SELECT pg_advisory_xact_lock(hashtext($1))", &[&s])?;
            tx.batch_execute(&ddl)?;
            let version: Option<String> = tx.query_opt(&version_q, &[])?.map(|r| r.get(0));
            if version.as_deref() != Some(SCHEMA_VERSION) {
                tx.batch_execute(&drop)?;
            }
            tx.batch_execute(&tables)?;
            tx.execute(&set_version, &[&SCHEMA_VERSION])?;
            tx.commit()?;
            Ok(())
        })
    }
}

fn worker(url: String, rx: Arc<Mutex<Receiver<Job>>>) {
    let mut conn: Option<Conn> = None;
    loop {
        let job = match rx.lock().unwrap().recv() {
            Ok(j) => j,
            Err(_) => return, // PgMeta dropped
        };
        // (Re)connect lazily; a broken connection is replaced on the next job.
        if conn.as_ref().is_none_or(|c| c.client.is_closed()) {
            match Client::connect(&url, NoTls) {
                Ok(client) => {
                    conn = Some(Conn {
                        client,
                        stmts: HashMap::new(),
                    })
                }
                Err(e) => {
                    job(Err(format!("connecting to postgres: {e}")));
                    continue;
                }
            }
        }
        job(Ok(conn.as_mut().unwrap()));
    }
}

fn changeset_row(r: &Row) -> Result<ChangesetRow> {
    Ok(ChangesetRow {
        seq: r.get(0),
        oid: oid(r.get(1))?,
        parents: decode_oids(&r.get::<_, Vec<u8>>(2)),
        author: r.get(3),
        committer: r.get(4),
        extra_headers: r.get(5),
        message: r.get(6),
        commit_time: r.get(7),
        changes: r.get(8),
        tree_override: r.get::<_, Option<Vec<u8>>>(9).map(oid).transpose()?,
        raw_commit: r.get(10),
    })
}

fn insert_changeset_sql(p: &PgMeta) -> String {
    p.sql(
        "INSERT INTO {s}.changesets(oid, parents, author, committer, extra_headers,
             message, commit_time, changes, tree_override, raw_commit)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT (oid) DO NOTHING",
    )
}

impl Backend for PgMeta {
    fn changeset(&self, o: &Oid) -> Result<Option<ChangesetRow>> {
        let q = self.sql(
            "SELECT seq, oid, parents, author, committer, extra_headers, message,
                    commit_time, changes, tree_override, raw_commit
             FROM {s}.changesets WHERE oid = $1",
        );
        let key = o.as_bytes().to_vec();
        self.run(move |c| c.query(&q, &[&key])?.first().map(changeset_row).transpose())
    }

    fn override_counts(&self) -> Result<(i64, i64)> {
        let q = self.sql("SELECT COUNT(tree_override), COUNT(raw_commit) FROM {s}.changesets");
        self.run(move |c| {
            let r = &c.query(&q, &[])?[0];
            Ok((r.get(0), r.get(1)))
        })
    }

    fn overridden_changesets(&self, limit: usize) -> Result<Vec<Oid>> {
        let q = self.sql(
            "SELECT oid FROM {s}.changesets WHERE tree_override IS NOT NULL ORDER BY seq LIMIT $1",
        );
        self.run(move |c| {
            c.query(&q, &[&(limit as i64)])?
                .into_iter()
                .map(|r| oid(r.get(0)))
                .collect()
        })
    }

    fn changeset_changes(&self, o: &Oid) -> Result<Option<Vec<u8>>> {
        let q = self.sql("SELECT changes FROM {s}.changesets WHERE oid = $1");
        let key = o.as_bytes().to_vec();
        self.run(move |c| Ok(c.query(&q, &[&key])?.first().map(|r| r.get(0))))
    }

    fn parents(&self, o: &Oid) -> Result<Option<Vec<Oid>>> {
        let q = self.sql("SELECT parents FROM {s}.changesets WHERE oid = $1");
        let key = o.as_bytes().to_vec();
        self.run(move |c| {
            Ok(c.query(&q, &[&key])?
                .first()
                .map(|r| decode_oids(&r.get::<_, Vec<u8>>(0))))
        })
    }

    fn has_changeset(&self, o: &Oid) -> Result<bool> {
        let q = self.sql("SELECT 1 FROM {s}.changesets WHERE oid = $1");
        let key = o.as_bytes().to_vec();
        self.run(move |c| Ok(!c.query(&q, &[&key])?.is_empty()))
    }

    fn changeset_count(&self) -> Result<i64> {
        let q = self.sql("SELECT COUNT(*) FROM {s}.changesets");
        self.run(move |c| Ok(c.query(&q, &[])?[0].get(0)))
    }

    fn resolve_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<Oid>> {
        let lo = Oid::from_hex(&format!("{prefix:0<40}"))?
            .as_bytes()
            .to_vec();
        let hi = Oid::from_hex(&format!("{prefix:f<40}"))?
            .as_bytes()
            .to_vec();
        let q = self.sql("SELECT oid FROM {s}.changesets WHERE oid BETWEEN $1 AND $2 LIMIT $3");
        self.run(move |c| {
            c.query(&q, &[&lo, &hi, &(limit as i64)])?
                .into_iter()
                .map(|r| oid(r.get(0)))
                .collect()
        })
    }

    fn path_changes(
        &self,
        path: &str,
        after: (u32, Option<Oid>),
        limit: usize,
    ) -> Result<Vec<(u32, Oid)>> {
        let q = self.sql(
            "SELECT gen, oid FROM {s}.path_index
             WHERE path = $1 AND (gen < $2 OR (gen = $2 AND oid > $3))
             ORDER BY gen DESC, oid ASC LIMIT $4",
        );
        let path = path.to_string();
        let g = after.0 as i64;
        let cursor: Vec<u8> = after.1.map(|o| o.0.to_vec()).unwrap_or_default();
        self.run(move |c| {
            c.query(&q, &[&path, &g, &cursor, &(limit as i64)])?
                .into_iter()
                .map(|r| Ok((r.get::<_, i64>(0) as u32, oid(r.get(1))?)))
                .collect()
        })
    }

    fn refs(&self) -> Result<Vec<RefRow>> {
        let q = self.sql("SELECT name, target, peeled FROM {s}.refs ORDER BY name");
        self.run(move |c| {
            c.query(&q, &[])?
                .into_iter()
                .map(|r| {
                    Ok(RefRow {
                        name: r.get(0),
                        target: oid(r.get(1))?,
                        peeled: oid(r.get(2))?,
                    })
                })
                .collect()
        })
    }

    fn get_ref(&self, name: &str) -> Result<Option<RefRow>> {
        let q = self.sql("SELECT name, target, peeled FROM {s}.refs WHERE name = $1");
        let name = name.to_string();
        self.run(move |c| {
            c.query(&q, &[&name])?
                .first()
                .map(|r| {
                    Ok(RefRow {
                        name: r.get(0),
                        target: oid(r.get(1))?,
                        peeled: oid(r.get(2))?,
                    })
                })
                .transpose()
        })
    }

    fn cas_ref(&self, name: &str, expected: Option<Oid>, new: Option<(Oid, Oid)>) -> Result<bool> {
        let name = name.to_string();
        let exp = expected.map(|o| o.as_bytes().to_vec());
        let new = new.map(|(t, p)| (t.as_bytes().to_vec(), p.as_bytes().to_vec()));
        // Each case is one statement that affects one row exactly when the
        // ref still has the expected value.
        let q = match (&exp, &new) {
            (None, Some(_)) => self.sql(
                "INSERT INTO {s}.refs(name, target, peeled) VALUES ($1, $2, $3)
                 ON CONFLICT (name) DO NOTHING",
            ),
            (Some(_), Some(_)) => self.sql(
                "UPDATE {s}.refs SET target = $2, peeled = $3 WHERE name = $1 AND target = $4",
            ),
            (Some(_), None) => self.sql("DELETE FROM {s}.refs WHERE name = $1 AND target = $2"),
            (None, None) => self.sql("SELECT 1 FROM {s}.refs WHERE name = $1"),
        };
        self.run(move |c| {
            Ok(match (exp, new) {
                (None, Some((t, p))) => c.execute(&q, &[&name, &t, &p])? == 1,
                (Some(e), Some((t, p))) => c.execute(&q, &[&name, &t, &p, &e])? == 1,
                (Some(e), None) => c.execute(&q, &[&name, &e])? == 1,
                // "Absent, stays absent": true exactly when it is absent.
                (None, None) => c.query(&q, &[&name])?.is_empty(),
            })
        })
    }

    fn get_derived(&self, kind: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let q = self.sql("SELECT value FROM {s}.derived WHERE kind = $1 AND key = $2");
        let (kind, key) = (kind.to_string(), key.to_vec());
        self.run(move |c| Ok(c.query(&q, &[&kind, &key])?.first().map(|r| r.get(0))))
    }

    fn derived_keys_with_prefix(&self, kind: &str, prefix: &[u8]) -> Result<Vec<Vec<u8>>> {
        // bytea compares as memcmp, like SQLite blobs: the keys with this
        // prefix are those in [prefix, next) where next is the smallest
        // value above every key with the prefix, if there is one.
        let mut hi = prefix.to_vec();
        while hi.last() == Some(&0xff) {
            hi.pop();
        }
        let bounded = if let Some(last) = hi.last_mut() {
            *last += 1;
            true
        } else {
            false
        };
        let q = if bounded {
            self.sql("SELECT key FROM {s}.derived WHERE kind = $1 AND key >= $2 AND key < $3")
        } else {
            self.sql("SELECT key FROM {s}.derived WHERE kind = $1 AND key >= $2")
        };
        let (kind, lo) = (kind.to_string(), prefix.to_vec());
        let prefix = prefix.to_vec();
        self.run(move |c| {
            let rows = if bounded {
                c.query(&q, &[&kind, &lo, &hi])?
            } else {
                c.query(&q, &[&kind, &lo])?
            };
            Ok(rows
                .into_iter()
                .map(|r| r.get::<_, Vec<u8>>(0))
                .filter(|k| k.starts_with(&prefix))
                .collect())
        })
    }

    fn derived_with_prefix(&self, kind: &str, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut hi = prefix.to_vec();
        while hi.last() == Some(&0xff) {
            hi.pop();
        }
        let bounded = if let Some(last) = hi.last_mut() {
            *last += 1;
            true
        } else {
            false
        };
        let q = if bounded {
            self.sql("SELECT key, value FROM {s}.derived WHERE kind = $1 AND key >= $2 AND key < $3 ORDER BY key")
        } else {
            self.sql(
                "SELECT key, value FROM {s}.derived WHERE kind = $1 AND key >= $2 ORDER BY key",
            )
        };
        let (kind, lo) = (kind.to_string(), prefix.to_vec());
        let prefix = prefix.to_vec();
        self.run(move |c| {
            let rows = if bounded {
                c.query(&q, &[&kind, &lo, &hi])?
            } else {
                c.query(&q, &[&kind, &lo])?
            };
            Ok(rows
                .into_iter()
                .map(|r| (r.get::<_, Vec<u8>>(0), r.get::<_, Vec<u8>>(1)))
                .filter(|(k, _)| k.starts_with(&prefix))
                .collect())
        })
    }

    fn put_derived(&self, kind: &str, key: &[u8], value: &[u8]) -> Result<()> {
        let q = self.sql(
            "INSERT INTO {s}.derived(kind, key, value) VALUES ($1, $2, $3)
             ON CONFLICT (kind, key) DO NOTHING",
        );
        let (kind, key, value) = (kind.to_string(), key.to_vec(), value.to_vec());
        self.run(move |c| {
            c.execute(&q, &[&kind, &key, &value])?;
            Ok(())
        })
    }

    fn write_batch(&self, ops: &[WriteOp]) -> Result<()> {
        enum Op {
            Changeset(Box<ChangesetRow>),
            PathIndex(String, i64, Vec<u8>),
            Derived(String, Vec<u8>, Vec<u8>),
            Delete(String, Vec<u8>),
        }
        let ops: Vec<Op> = ops
            .iter()
            .map(|op| match op {
                WriteOp::Changeset(cs) => Op::Changeset(Box::new((*cs).clone())),
                WriteOp::PathIndex {
                    path,
                    generation,
                    oid,
                } => Op::PathIndex(
                    path.to_string(),
                    *generation as i64,
                    oid.as_bytes().to_vec(),
                ),
                WriteOp::Derived { kind, key, value } => {
                    Op::Derived(kind.to_string(), key.to_vec(), value.to_vec())
                }
                WriteOp::DeleteDerived { kind, key } => Op::Delete(kind.to_string(), key.to_vec()),
            })
            .collect();
        let ins_cs = insert_changeset_sql(self);
        let ins_path = self.sql(
            "INSERT INTO {s}.path_index(path, gen, oid) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        );
        let ins_derived = self.sql(
            "INSERT INTO {s}.derived(kind, key, value) VALUES ($1, $2, $3)
             ON CONFLICT (kind, key) DO NOTHING",
        );
        let del_derived = self.sql("DELETE FROM {s}.derived WHERE kind = $1 AND key = $2");
        self.run(move |c| {
            let (s_cs, s_path, s_der, s_del) = (
                c.stmt(&ins_cs)?,
                c.stmt(&ins_path)?,
                c.stmt(&ins_derived)?,
                c.stmt(&del_derived)?,
            );
            let mut tx = c.client.transaction()?;
            for op in &ops {
                match op {
                    Op::Changeset(cs) => {
                        let tree = cs.tree_override.map(|t| t.as_bytes().to_vec());
                        tx.execute(
                            &s_cs,
                            &[
                                &cs.oid.as_bytes().to_vec(),
                                &encode_oids(&cs.parents),
                                &cs.author,
                                &cs.committer,
                                &cs.extra_headers,
                                &cs.message,
                                &cs.commit_time,
                                &cs.changes,
                                &tree,
                                &cs.raw_commit,
                            ],
                        )?;
                    }
                    Op::PathIndex(p, g, o) => {
                        tx.execute(&s_path, &[p, g, o])?;
                    }
                    Op::Derived(k, key, v) => {
                        tx.execute(&s_der, &[k, key, v])?;
                    }
                    Op::Delete(k, key) => {
                        tx.execute(&s_del, &[k, key])?;
                    }
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    fn get_kv(&self, key: &str) -> Result<Option<String>> {
        let q = self.sql("SELECT value FROM {s}.kv WHERE key = $1");
        let key = key.to_string();
        self.run(move |c| Ok(c.query(&q, &[&key])?.first().map(|r| r.get(0))))
    }

    fn set_kv(&self, key: &str, value: &str) -> Result<()> {
        let q = self.sql(
            "INSERT INTO {s}.kv(key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        );
        let (key, value) = (key.to_string(), value.to_string());
        self.run(move |c| {
            c.execute(&q, &[&key, &value])?;
            Ok(())
        })
    }

    fn paths_after(&self, after: &str, limit: usize) -> Result<Vec<String>> {
        let q = self
            .sql("SELECT DISTINCT path FROM {s}.path_index WHERE path > $1 ORDER BY path LIMIT $2");
        let (after, limit) = (after.to_string(), limit as i64);
        self.run(move |c| {
            Ok(c.query(&q, &[&after, &limit])?
                .iter()
                .map(|r| r.get(0))
                .collect())
        })
    }

    fn kv_with_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let q = self.sql(
            "SELECT key, value FROM {s}.kv WHERE substr(key, 1, length($1)) = $1 ORDER BY key",
        );
        let prefix = prefix.to_string();
        self.run(move |c| {
            Ok(c.query(&q, &[&prefix])?
                .iter()
                .map(|r| (r.get(0), r.get(1)))
                .collect())
        })
    }

    fn append_log(&self, stream: &str, data: &[u8]) -> Result<i64> {
        let q = self.sql("INSERT INTO {s}.log(stream, data) VALUES ($1, $2) RETURNING seq");
        let (stream, data) = (stream.to_string(), data.to_vec());
        self.run(move |c| Ok(c.query(&q, &[&stream, &data])?[0].get(0)))
    }

    fn read_log(&self, stream: &str, after: i64, limit: usize) -> Result<Vec<(i64, Vec<u8>)>> {
        let q = self.sql(
            "SELECT seq, data FROM {s}.log WHERE stream = $1 AND seq > $2 ORDER BY seq LIMIT $3",
        );
        let (stream, limit) = (stream.to_string(), limit as i64);
        self.run(move |c| {
            Ok(c.query(&q, &[&stream, &after, &limit])?
                .iter()
                .map(|r| (r.get(0), r.get(1)))
                .collect())
        })
    }

    /// A session-level advisory lock, held on a connection of its own by a
    /// thread that waits for the guard to drop. If that connection dies the
    /// lock is released with it, so a crashed server cannot hold it.
    fn lock(&self, name: &str) -> Result<MetaLock> {
        let key = format!("{}:{name}", self.schema);
        let url = self.url.clone();
        let (acquired_tx, acquired_rx) = channel::<Result<()>>();
        let (release_tx, release_rx) = channel::<()>();
        std::thread::Builder::new()
            .name("wren-pg-lock".into())
            .spawn(move || {
                let mut client = match Client::connect(&url, NoTls)
                    .context("connecting to postgres for a lock")
                {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = acquired_tx.send(Err(e));
                        return;
                    }
                };
                if let Err(e) = client.execute("SELECT pg_advisory_lock(hashtext($1))", &[&key]) {
                    let _ = acquired_tx.send(Err(e.into()));
                    return;
                }
                let _ = acquired_tx.send(Ok(()));
                let _ = release_rx.recv(); // the guard dropped
                let _ = client.execute("SELECT pg_advisory_unlock(hashtext($1))", &[&key]);
            })?;
        acquired_rx
            .recv()
            .map_err(|_| anyhow!("postgres lock thread failed"))??;
        Ok(MetaLock(Some(Box::new(release_tx))))
    }
}
