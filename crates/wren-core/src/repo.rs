//! A repository served by Wren: the shared object store, this repo's metadata
//! database, and (optionally) the upstream git mirror it is ingested from.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::changes::{self, Change};
use crate::derived::{Derivable, Memo};
use crate::git_source::{CatFile, GitMirror};
use crate::graph::GraphNode;
use crate::meta::{ChangesetRow, MetaConfig, MetaDb, RefRow, WriteOp};
use crate::object::{self, Commit, Kind, RawCommit, Tag, TreeEntry};
use crate::oid::Oid;
use crate::store::ObjectStore;
use crate::treegen::GitTree;

pub struct Repo {
    pub name: String,
    pub store: Arc<ObjectStore>,
    pub meta: MetaDb,
    pub upstream: Option<String>,
    dir: PathBuf,
    pub(crate) memo: Mutex<Memo>,
    /// Held by sync (fetch, absorb packs, ingest) and by pushes to the
    /// upstream: a push reads the mirror's packs, which absorb deletes.
    pub(crate) sync_lock: Mutex<()>,
    /// Rules for ref updates (see `write::Policy`).
    pub policy: std::sync::RwLock<crate::write::Policy>,
    /// Recent export lags in milliseconds (see `export`).
    pub(crate) export_lags: Mutex<std::collections::VecDeque<u64>>,
}

/// Exclusive use of the git mirror: within this process, and (on Postgres)
/// across every server sharing the repository.
pub(crate) struct MirrorLock<'a> {
    _local: std::sync::MutexGuard<'a, ()>,
    _shared: crate::meta::MetaLock,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncReport {
    pub new_changesets: usize,
    pub refs_updated: usize,
    pub objects_imported: usize,
    pub millis: u128,
}

impl Repo {
    /// A repository with SQLite metadata in `dir`.
    pub fn open(
        dir: impl Into<PathBuf>,
        name: &str,
        store: Arc<ObjectStore>,
        upstream: Option<&str>,
    ) -> Result<Repo> {
        Repo::open_with(dir, name, store, upstream, &MetaConfig::Sqlite)
    }

    /// A repository whose metadata lives where `meta` says. Several servers
    /// can open the same repository on Postgres, sharing `dir` (the git
    /// mirror) and the object store.
    pub fn open_with(
        dir: impl Into<PathBuf>,
        name: &str,
        store: Arc<ObjectStore>,
        upstream: Option<&str>,
        meta: &MetaConfig,
    ) -> Result<Repo> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let meta = MetaDb::open_config(meta, &dir, name)?;
        let upstream = match upstream {
            Some(u) => {
                meta.set_kv("upstream", u)?;
                Some(u.to_string())
            }
            None => meta.get_kv("upstream")?,
        };
        Ok(Repo {
            name: name.to_string(),
            store,
            meta,
            upstream,
            dir,
            memo: Mutex::new(Memo::default()),
            sync_lock: Mutex::new(()),
            policy: Default::default(),
            export_lags: Default::default(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Take the mirror lock (see [`MirrorLock`]).
    pub(crate) fn lock_mirror(&self) -> Result<MirrorLock<'_>> {
        let local = self.sync_lock.lock().unwrap();
        let shared = self.meta.lock("mirror")?;
        // git reads the store through alternates, so it must be complete
        // on local disk: fetch what other servers added to the bucket.
        self.store.hydrate()?;
        Ok(MirrorLock {
            _local: local,
            _shared: shared,
        })
    }

    // ---- ingest ---------------------------------------------------------------

    /// Fetch from upstream and ingest every new commit as a changeset.
    pub fn sync(&self) -> Result<SyncReport> {
        let r = self.sync_inner();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        match &r {
            Ok(_) => {
                self.meta.set_kv("last_sync", &now.to_string())?;
                self.meta.set_kv("last_sync_error", "")?;
            }
            Err(e) => self.meta.set_kv("last_sync_error", &format!("{e:#}"))?,
        }
        r
    }

    /// Sync, unless the store's segments are shared through a bucket and
    /// some server (any of them) synced less than `max_age` ago. Servers
    /// sharing a repository then take turns instead of all fetching, and
    /// one that skips needs no complete local copy (a sync downloads every
    /// segment first; see `lock_mirror`). Returns `None` when skipped.
    pub fn sync_unless_fresh(&self, max_age: std::time::Duration) -> Result<Option<SyncReport>> {
        if self.store.remote().is_some() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            if let (Some(t), None) = self.sync_status()? {
                if now - t < max_age.as_secs() as i64 {
                    return Ok(None);
                }
            }
        }
        self.sync().map(Some)
    }

    /// Whether Wren is the source of truth for this repository's refs (the
    /// git host is then a downstream mirror, see `export`), rather than
    /// the git host (write-proxy mode).
    pub fn authoritative(&self) -> Result<bool> {
        Ok(self.meta.get_kv("authority")?.as_deref() == Some("wren"))
    }

    /// Make Wren (or the git host) the source of truth. Flipping to Wren
    /// records that the host has what Wren has now, so the exporter starts
    /// from there. Flipping back is only safe once the export has caught
    /// up.
    pub fn set_authoritative(&self, wren: bool) -> Result<()> {
        if wren == self.authoritative()? {
            return Ok(());
        }
        if wren {
            // Start from what the host has now.
            if self.upstream.is_some() {
                self.sync()?;
            }
            let _guard = self.lock_mirror()?;
            for r in self.meta.refs()? {
                if self.meta.get_kv(&format!("exported:{}", r.name))?.is_none() {
                    self.meta
                        .set_kv(&format!("exported:{}", r.name), &r.target.to_hex())?;
                }
            }
            self.meta.set_kv("authority", "wren")?;
        } else {
            self.meta.set_kv("authority", "host")?;
        }
        Ok(())
    }

    /// Ingest the commits a push brought (they are in the store already):
    /// everything reachable from `tip` that is not a changeset yet, parents
    /// first. A tag is peeled. Returns the commit `tip` names.
    pub fn ingest_pushed(&self, tip: &Oid) -> Result<Oid> {
        let mut commit = *tip;
        loop {
            let (kind, data) = self
                .store
                .get(&commit)?
                .with_context(|| format!("{commit} is missing"))?;
            match kind {
                Kind::Commit => break,
                Kind::Tag => commit = Tag::parse(&data)?.object,
                k => bail!("{tip} is a {}, not a commit or tag", k.as_str()),
            }
        }
        // Depth-first, emitting a commit after its parents.
        let mut order = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![(commit, false)];
        while let Some((c, expanded)) = stack.pop() {
            if expanded {
                order.push(c);
                continue;
            }
            if !seen.insert(c) || self.meta.has_changeset(&c)? {
                continue;
            }
            stack.push((c, true));
            for p in self.store.read_commit(&c)?.parents {
                stack.push((p, false));
            }
        }
        let mut report = SyncReport::default();
        for c in &order {
            if !self.meta.has_changeset(c)? {
                self.ingest_commit(&mut None, c, &mut report)?;
            }
        }
        Ok(commit)
    }

    /// (unix time of last successful sync, error of the latest attempt)
    pub fn sync_status(&self) -> Result<(Option<i64>, Option<String>)> {
        let t = self.meta.get_kv("last_sync")?.and_then(|v| v.parse().ok());
        let e = self
            .meta
            .get_kv("last_sync_error")?
            .filter(|e| !e.is_empty());
        Ok((t, e))
    }

    fn sync_inner(&self) -> Result<SyncReport> {
        // Once Wren is the source of truth, the host is a downstream mirror:
        // nothing is fetched from it (see `export`).
        if self.authoritative()? {
            return Ok(SyncReport::default());
        }
        let _guard = self.lock_mirror()?;
        let start = Instant::now();
        let url = self.upstream.as_deref().context("repo has no upstream")?;
        let mirror = GitMirror::open(self.dir.join("mirror.git"), url, self.store.root())?;
        if self.store.remote().is_some() {
            // This server's mirror may be behind the shared metadata.
            let refs: Vec<(String, Oid)> = self
                .meta
                .refs()?
                .into_iter()
                .map(|r| (r.name, r.target))
                .collect();
            mirror.set_refs(&refs)?;
        }
        // The host can move a ref between the ref advertisement and the
        // fetch (another writer landing), which makes the fetch fail with
        // "not our ref". Such failures are transient: retry.
        let mut attempt = 0;
        while let Err(e) = mirror.fetch() {
            attempt += 1;
            if attempt == 4 {
                return Err(e);
            }
            std::thread::sleep(std::time::Duration::from_millis(50 << attempt));
        }
        if let Some(head) = mirror.default_branch()? {
            self.meta.set_kv("HEAD", &head)?;
        }
        let mut report = SyncReport::default();

        // Move whatever the fetch brought into the object store: its pack
        // becomes a segment as is. A pack holds everything the fetch
        // brought, so "tree present => subtree present" holds once it is
        // in; an interrupted absorb is redone before any commit is ingested.
        let store = self.store.clone();
        report.objects_imported += mirror.absorb_objects(&store, |oid, raw| {
            if !store.has(&oid) {
                store.put_raw(&oid, raw)?;
            }
            Ok(())
        })?;

        let mut cat = mirror.cat_file()?;
        let upstream_refs = mirror.list_refs()?;
        let mut wanted: Vec<(String, Oid, Oid)> = Vec::new(); // (name, target, peeled commit)
        for r in &upstream_refs {
            let mut oid = r.oid;
            let mut kind = r.kind;
            while kind == Kind::Tag {
                let data = self.import_object(&mut Some(&mut cat), &oid, &mut report)?;
                let tag = Tag::parse(&data)?;
                oid = tag.object;
                kind = tag.kind;
            }
            if kind == Kind::Commit {
                wanted.push((r.name.clone(), r.oid, oid));
            }
        }

        let known: Vec<Oid> = self.meta.refs()?.iter().map(|r| r.peeled).collect();
        let tips: Vec<Oid> = wanted.iter().map(|w| w.2).collect();
        let new_commits = mirror.rev_list(&tips, &known)?;
        for oid in &new_commits {
            if self.meta.has_changeset(oid)? {
                continue;
            }
            self.ingest_commit(&mut Some(&mut cat), oid, &mut report)?;
        }

        // Publish refs only after everything they point to is ingested, and
        // readable by every server (fetched packs are uploaded as they are
        // absorbed; this seals anything written loose).
        self.store.publish()?;
        let current: HashMap<String, RefRow> = self
            .meta
            .refs()?
            .into_iter()
            .map(|r| (r.name.clone(), r))
            .collect();
        for (name, target, peeled) in &wanted {
            let cur = current.get(name);
            if cur.map(|r| r.target) != Some(*target)
                && self
                    .meta
                    .cas_ref(name, cur.map(|r| r.target), Some((*target, *peeled)))?
            {
                report.refs_updated += 1;
            }
        }
        let wanted_names: BTreeSet<&str> = wanted.iter().map(|w| w.0.as_str()).collect();
        for (name, r) in &current {
            if !wanted_names.contains(name.as_str())
                && self.meta.cas_ref(name, Some(r.target), None)?
            {
                report.refs_updated += 1;
            }
        }
        report.millis = start.elapsed().as_millis();
        Ok(report)
    }

    fn import_object(
        &self,
        cat: &mut Option<&mut CatFile>,
        oid: &Oid,
        report: &mut SyncReport,
    ) -> Result<Vec<u8>> {
        if let Some((_, data)) = self.store.get(oid)? {
            return Ok(data);
        }
        let (kind, data) = cat
            .as_deref_mut()
            .with_context(|| format!("object {oid} is missing"))?
            .get(oid)?;
        self.store
            .put_raw(oid, &crate::object::encode_loose(kind, &data))?;
        report.objects_imported += 1;
        Ok(data)
    }

    /// Import a tree and everything below it. Children are stored before their
    /// parent, so "tree present" implies "whole subtree present" and already
    /// known subtrees are skipped: cost scales with the change, not the repo.
    fn import_tree(
        &self,
        cat: &mut Option<&mut CatFile>,
        oid: &Oid,
        report: &mut SyncReport,
    ) -> Result<()> {
        if self.store.has(oid) {
            return Ok(());
        }
        let (kind, data) = cat
            .as_deref_mut()
            .with_context(|| format!("tree {oid} is missing"))?
            .get(oid)?;
        if kind != Kind::Tree {
            bail!("{oid} is not a tree");
        }
        for e in crate::object::parse_tree(&data)? {
            if e.is_tree() {
                self.import_tree(cat, &e.oid, report)?;
            } else if !e.is_gitlink() && !self.store.has(&e.oid) {
                self.import_object(cat, &e.oid, report)?;
            }
        }
        self.store
            .put_raw(oid, &crate::object::encode_loose(Kind::Tree, &data))?;
        report.objects_imported += 1;
        Ok(())
    }

    /// Ingest one git commit as a changeset. The changeset keeps everything
    /// the commit says except its tree; the tree and the commit object are
    /// derived from it. Both are checked here against git's own: what
    /// derivation cannot reproduce is stored as an override, so ids never
    /// change (see `treegen`).
    fn ingest_commit(
        &self,
        cat: &mut Option<&mut CatFile>,
        oid: &Oid,
        report: &mut SyncReport,
    ) -> Result<()> {
        let data = self.import_object(cat, oid, report)?;
        let commit = Commit::parse(&data)?;
        // The git mirror still reads its history from the object store, so
        // git's trees are kept there; the derived ones are identical.
        self.import_tree(cat, &commit.tree, report)?;
        let parent_tree = match commit.parents.first() {
            Some(p) => Some(self.tree_of(p)?),
            None => None,
        };
        let changes = changes::diff_trees(&self.store, parent_tree, Some(commit.tree))?;
        let derived_tree = crate::treegen::apply(&self.store, parent_tree, &changes);
        let tree_override = match derived_tree {
            Ok(t) if t == commit.tree => None,
            _ => Some(commit.tree),
        };
        let row = match RawCommit::split(&data).filter(|rc| {
            rc.tree == commit.tree && rc.parents == commit.parents && rc.encode() == data
        }) {
            Some(rc) => ChangesetRow {
                seq: 0,
                oid: *oid,
                parents: rc.parents,
                author: rc.author,
                committer: Some(rc.committer),
                extra_headers: rc.extra_headers,
                message: rc.message,
                commit_time: commit.committer.time,
                changes: changes::encode_changes(&changes),
                tree_override,
                raw_commit: None,
            },
            None => {
                let header = |k: &[u8]| object::commit_header(&data, k).unwrap_or(b"").to_vec();
                ChangesetRow {
                    seq: 0,
                    oid: *oid,
                    parents: commit.parents.clone(),
                    author: header(b"author"),
                    committer: Some(header(b"committer")),
                    extra_headers: Vec::new(),
                    message: commit.message.clone().into_bytes(),
                    commit_time: commit.committer.time,
                    changes: changes::encode_changes(&changes),
                    tree_override,
                    raw_commit: Some(data.clone()),
                }
            }
        };
        self.insert_changeset(&row, &changes, commit.tree)?;
        report.new_changesets += 1;
        Ok(())
    }

    /// Persist a changeset whose tree is known: the row, its path index rows
    /// and its derived tree, in one transaction. Parents must already exist.
    pub(crate) fn insert_changeset(
        &self,
        row: &ChangesetRow,
        changes: &[Change],
        tree: Oid,
    ) -> Result<()> {
        // The path index (a derived type, see `pathindex`) is derived
        // eagerly and written in the same transaction as the changeset,
        // materialized into its table: a changeset that exists is never
        // re-ingested, so an index row lost to a crash between two writes
        // would never come back. Its generation comes from the (already
        // ingested) parents.
        let generation = 1 + row
            .parents
            .iter()
            .map(|p| self.node(p).map(|n| n.generation))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        let index = crate::pathindex::PathIndex {
            generation,
            paths: crate::pathindex::changed_paths(changes),
        };
        let index_value = index.encode();
        let trigrams = crate::pathindex::trigram_keys(changes);
        let oid = &row.oid;
        let mut ops = vec![WriteOp::Changeset(row)];
        ops.extend(index.rows(oid));
        ops.push(WriteOp::Derived {
            kind: crate::pathindex::PathIndex::KIND,
            key: oid.as_bytes(),
            value: &index_value,
        });
        ops.extend(trigrams.iter().map(|k| WriteOp::Derived {
            kind: crate::pathindex::TRIGRAMS,
            key: k,
            value: b"",
        }));
        // The tree is derived data, but descendants derive theirs from it,
        // so it is persisted with the changeset.
        ops.push(WriteOp::Derived {
            kind: GitTree::KIND,
            key: oid.as_bytes(),
            value: tree.as_bytes(),
        });
        self.meta.write_batch(&ops)?;
        // Derived eagerly, but safe to lose: recomputed on demand if missing.
        let node = self.derived::<GraphNode>(oid)?;
        debug_assert_eq!(node.generation, generation);
        Ok(())
    }

    // ---- resolution -----------------------------------------------------------

    pub fn head_ref(&self) -> Result<Option<String>> {
        self.meta.get_kv("HEAD")
    }

    fn resolve_base(&self, base: &str) -> Result<Oid> {
        if base == "HEAD" || base.is_empty() {
            let head = self
                .head_ref()?
                .ok_or_else(|| crate::not_found("repo has no HEAD"))?;
            return Ok(self
                .meta
                .get_ref(&head)?
                .ok_or_else(|| crate::not_found(format!("HEAD ref {head} missing")))?
                .peeled);
        }
        let is_hex = base.bytes().all(|b| b.is_ascii_hexdigit());
        // A full object id always names that object (as in git), so a query
        // pinned to full ids can never be redirected by a ref of that name.
        if base.len() == 40 && is_hex {
            let oid = Oid::from_hex(&base.to_ascii_lowercase())?;
            if self.meta.has_changeset(&oid)? {
                return Ok(oid);
            }
            return Err(crate::not_found(format!("unknown revision {base}")));
        }
        // git's ref lookup order (gitrevisions(7)).
        for cand in [
            base.to_string(),
            format!("refs/{base}"),
            format!("refs/tags/{base}"),
            format!("refs/heads/{base}"),
        ] {
            if let Some(r) = self.meta.get_ref(&cand)? {
                return Ok(r.peeled);
            }
        }
        if base.len() >= 4 && is_hex {
            let found = self.meta.resolve_prefix(&base.to_ascii_lowercase(), 2)?;
            match found.len() {
                1 => return Ok(found[0]),
                0 => {}
                _ => return Err(crate::bad_request(format!("ambiguous revision {base}"))),
            }
        }
        Err(crate::not_found(format!("unknown revision {base:?}")))
    }

    /// Resolve a revision to a commit: a ref name, a (possibly abbreviated)
    /// commit id, or either followed by `~N`, `^N`, `^{commit}` or `^{}`, as
    /// in git. Tags are peeled: a Wren revision always names a commit.
    pub fn resolve(&self, rev: &str) -> Result<Oid> {
        let split = rev.find(['~', '^']).unwrap_or(rev.len());
        let mut oid = self.resolve_base(&rev[..split])?;
        let mut rest = &rev[split..];
        while !rest.is_empty() {
            if let Some(r) = rest
                .strip_prefix("^{commit}")
                .or_else(|| rest.strip_prefix("^{}"))
            {
                rest = r;
                continue;
            }
            let op = rest.as_bytes()[0];
            if op != b'~' && op != b'^' {
                return Err(crate::bad_request(format!(
                    "invalid revision syntax {rev:?}"
                )));
            }
            let digits_end = rest[1..]
                .find(|c: char| !c.is_ascii_digit())
                .map(|i| i + 1)
                .unwrap_or(rest.len());
            let n: u32 = if digits_end > 1 {
                rest[1..digits_end]
                    .parse()
                    .map_err(|_| crate::bad_request(format!("invalid revision syntax {rev:?}")))?
            } else {
                1
            };
            rest = &rest[digits_end..];
            oid = match op {
                b'~' => {
                    let depth = self.node(&oid)?.depth;
                    if n > depth {
                        return Err(crate::not_found(format!("{rev}: not enough history")));
                    }
                    self.first_parent_steps(oid, n)?
                }
                _ if n == 0 => oid,
                _ => *self
                    .node(&oid)?
                    .parents
                    .get(n as usize - 1)
                    .ok_or_else(|| crate::not_found(format!("{rev}: no such parent")))?,
            };
        }
        Ok(oid)
    }

    // ---- queries --------------------------------------------------------------

    pub fn commit(&self, oid: &Oid) -> Result<Commit> {
        Commit::parse(&self.commit_object(oid)?).with_context(|| format!("parsing commit {oid}"))
    }

    /// The git commit object of a changeset, derived from it: its fields plus
    /// the derived tree. It hashes to `oid` (checked at ingest).
    pub fn commit_object(&self, oid: &Oid) -> Result<Vec<u8>> {
        let row = self
            .meta
            .changeset(oid)?
            .ok_or_else(|| crate::not_found(format!("unknown changeset {oid}")))?;
        if let Some(raw) = row.raw_commit {
            return Ok(raw);
        }
        Ok(RawCommit {
            tree: self.tree_of(oid)?,
            parents: row.parents,
            committer: row.committer.unwrap_or_else(|| row.author.clone()),
            author: row.author,
            extra_headers: row.extra_headers,
            message: row.message,
        }
        .encode())
    }

    /// The changes a changeset made relative to its first parent.
    pub fn changeset_changes(&self, oid: &Oid) -> Result<Vec<Change>> {
        let row = self
            .meta
            .changeset(oid)?
            .ok_or_else(|| crate::not_found(format!("unknown changeset {oid}")))?;
        changes::decode_changes(&row.changes)
    }

    /// The root tree of a changeset (derived data).
    pub fn tree_of(&self, oid: &Oid) -> Result<Oid> {
        Ok(self.derived::<GitTree>(oid)?.0)
    }

    pub fn diff(&self, base: &Oid, head: &Oid) -> Result<Vec<Change>> {
        changes::diff_trees(
            &self.store,
            Some(self.tree_of(base)?),
            Some(self.tree_of(head)?),
        )
    }

    pub fn entry(&self, rev: &Oid, path: &str) -> Result<Option<TreeEntry>> {
        changes::entry_at(&self.store, self.tree_of(rev)?, path)
    }

    /// First-parent history of `tip`, optionally limited to commits that
    /// changed `path` (a file or directory), newest first.
    pub fn log(&self, tip: &Oid, path: Option<&str>, limit: usize) -> Result<Vec<Oid>> {
        let path = path.map(changes::normalize_path).filter(|p| !p.is_empty());
        let mut out = Vec::new();
        match path {
            None => {
                let mut cur = Some(*tip);
                while let Some(c) = cur {
                    if out.len() >= limit {
                        break;
                    }
                    out.push(c);
                    cur = self.node(&c)?.parents.first().copied();
                }
            }
            Some(path) => {
                // Index lookup: only commits that touched the path are visited,
                // never the whole log.
                let mut cursor = (self.node(tip)?.generation, None);
                'outer: loop {
                    let page = self.meta.path_changes(&path, cursor, 512)?;
                    for (_, oid) in &page {
                        if self.on_first_parent_chain(tip, oid)? {
                            out.push(*oid);
                            if out.len() >= limit {
                                break 'outer;
                            }
                        }
                    }
                    match page.last() {
                        Some(&(g, o)) if page.len() == 512 => cursor = (g, Some(o)),
                        _ => break,
                    }
                }
            }
        }
        Ok(out)
    }
}
