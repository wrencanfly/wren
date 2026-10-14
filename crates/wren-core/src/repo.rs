//! A repository served by Wren: the shared object store, this repo's metadata
//! database, and (optionally) the upstream git mirror it is ingested from.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::changes::{self, Change};
use crate::derived::Memo;
use crate::git_source::{CatFile, GitMirror};
use crate::graph::GraphNode;
use crate::meta::{MetaDb, RefRow};
use crate::object::{Commit, Kind, Tag, TreeEntry};
use crate::oid::Oid;
use crate::store::ObjectStore;

pub struct Repo {
    pub name: String,
    pub store: Arc<ObjectStore>,
    pub meta: MetaDb,
    pub upstream: Option<String>,
    dir: PathBuf,
    pub(crate) memo: Mutex<Memo>,
    sync_lock: Mutex<()>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncReport {
    pub new_changesets: usize,
    pub refs_updated: usize,
    pub objects_imported: usize,
    pub millis: u128,
}

impl Repo {
    pub fn open(
        dir: impl Into<PathBuf>,
        name: &str,
        store: Arc<ObjectStore>,
        upstream: Option<&str>,
    ) -> Result<Repo> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let meta = MetaDb::open(dir.join("meta.sqlite"))?;
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
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
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
        let _guard = self.sync_lock.lock().unwrap();
        let start = Instant::now();
        let url = self.upstream.as_deref().context("repo has no upstream")?;
        let mirror = GitMirror::open(self.dir.join("mirror.git"), url, self.store.root())?;
        mirror.fetch()?;
        if let Some(head) = mirror.default_branch()? {
            self.meta.set_kv("HEAD", &head)?;
        }
        let mut report = SyncReport::default();

        // Move whatever the fetch brought into the object store (one pass over
        // the new pack, then the pack is deleted). Fetched objects arrive in
        // arbitrary order, so the store's "tree present => subtree present"
        // invariant only holds once this completes; that is why it always runs
        // before any commit is ingested, and an interrupted absorb is redone.
        let store = self.store.clone();
        mirror.absorb_objects(|oid, raw| {
            if !store.has(&oid) {
                store.put_raw(&oid, raw)?;
                report.objects_imported += 1;
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
                let data = self.import_object(&mut cat, &oid, &mut report)?;
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
            self.ingest_commit(&mut cat, oid, &mut report)?;
        }

        // Publish refs only after everything they point to is ingested.
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
        cat: &mut CatFile,
        oid: &Oid,
        report: &mut SyncReport,
    ) -> Result<Vec<u8>> {
        if let Some((_, data)) = self.store.get(oid)? {
            return Ok(data);
        }
        let (kind, data) = cat.get(oid)?;
        self.store
            .put_raw(oid, &crate::object::encode_loose(kind, &data))?;
        report.objects_imported += 1;
        Ok(data)
    }

    /// Import a tree and everything below it. Children are stored before their
    /// parent, so "tree present" implies "whole subtree present" and already
    /// known subtrees are skipped: cost scales with the change, not the repo.
    fn import_tree(&self, cat: &mut CatFile, oid: &Oid, report: &mut SyncReport) -> Result<()> {
        if self.store.has(oid) {
            return Ok(());
        }
        let (kind, data) = cat.get(oid)?;
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

    fn ingest_commit(&self, cat: &mut CatFile, oid: &Oid, report: &mut SyncReport) -> Result<()> {
        let data = self.import_object(cat, oid, report)?;
        let commit = Commit::parse(&data)?;
        self.import_tree(cat, &commit.tree, report)?;
        let parent_tree = match commit.parents.first() {
            Some(p) => Some(
                self.meta
                    .changeset(p)?
                    .with_context(|| format!("parent {p} not ingested"))?
                    .tree,
            ),
            None => None,
        };
        let changes = changes::diff_trees(&self.store, parent_tree, Some(commit.tree))?;
        // Generation number, from the (already ingested) parents. The path
        // index needs it, and it must be written in the same transaction as
        // the changeset: a changeset that exists is never re-ingested, so an
        // index row lost to a crash between two writes would never come back.
        let mut generation = 1;
        for p in &commit.parents {
            generation = generation.max(self.node(p)?.generation + 1);
        }
        // Every changed path and each of its parent directories.
        let mut paths = BTreeSet::new();
        for c in &changes {
            let mut p = c.path.as_str();
            paths.insert(p.to_string());
            while let Some(i) = p.rfind('/') {
                p = &p[..i];
                paths.insert(p.to_string());
            }
        }
        self.meta.write(|tx| {
            MetaDb::insert_changeset(
                tx,
                oid,
                &commit.tree,
                &commit.parents,
                commit.committer.time,
                &changes::encode_changes(&changes),
            )?;
            for p in &paths {
                MetaDb::insert_path_index(tx, p, generation, oid)?;
            }
            Ok(())
        })?;
        // Derived eagerly, but safe to lose: recomputed on demand if missing.
        let node = self.derived::<GraphNode>(oid)?;
        debug_assert_eq!(node.generation, generation);
        report.new_changesets += 1;
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
        self.store.read_commit(oid)
    }

    /// The changes a changeset made relative to its first parent.
    pub fn changeset_changes(&self, oid: &Oid) -> Result<Vec<Change>> {
        let row = self
            .meta
            .changeset(oid)?
            .ok_or_else(|| crate::not_found(format!("unknown changeset {oid}")))?;
        changes::decode_changes(&row.changes)
    }

    pub fn tree_of(&self, oid: &Oid) -> Result<Oid> {
        Ok(self
            .meta
            .changeset(oid)?
            .ok_or_else(|| crate::not_found(format!("unknown changeset {oid}")))?
            .tree)
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
