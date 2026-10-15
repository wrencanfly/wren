//! The path index and path search, as derived data.
//!
//! `PathIndex` is a derived type like any other: for a changeset, every
//! path it changed (and each parent directory) plus its generation number,
//! defined from the changeset and its parents' values. Its value is also
//! materialized into the `path_index` table, which `log -- <path>` and
//! blame read by (path, generation): that table *is* the index, and the
//! derived value says what to put in it. Both are written in the same
//! transaction as the changeset (eagerly), because a changeset that exists
//! is never ingested again.
//!
//! Path search uses trigrams of every path that was ever added (lowercase),
//! kept as derived rows `path3: <trigram><path>`. A query for a substring of
//! three bytes or more intersects the paths of its trigrams, then keeps
//! those present at the requested revision.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;

use crate::changes::{self, Change};
use crate::derived::Derivable;
use crate::meta::{ChangesetRow, WriteOp};
use crate::oid::Oid;
use crate::repo::Repo;

pub struct PathIndex {
    pub generation: u32,
    /// Changed paths and their parent directories, sorted.
    pub paths: Vec<String>,
}

/// Every changed path and each of its parent directories.
pub fn changed_paths(changes: &[Change]) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for c in changes {
        let mut p = c.path.as_str();
        paths.insert(p.to_string());
        while let Some(i) = p.rfind('/') {
            p = &p[..i];
            paths.insert(p.to_string());
        }
    }
    paths.into_iter().collect()
}

impl Derivable for PathIndex {
    const KIND: &'static str = "paths.v1";

    fn derive(_repo: &Repo, cs: &ChangesetRow, parents: &[Arc<Self>]) -> Result<Self> {
        Ok(PathIndex {
            generation: 1 + parents.iter().map(|p| p.generation).max().unwrap_or(0),
            paths: changed_paths(&changes::decode_changes(&cs.changes)?),
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut b = self.generation.to_le_bytes().to_vec();
        b.extend(self.paths.join("\0").as_bytes());
        b
    }

    fn decode(b: &[u8]) -> Result<Self> {
        anyhow::ensure!(b.len() >= 4, "short path index value");
        let generation = u32::from_le_bytes(b[..4].try_into().unwrap());
        let rest = std::str::from_utf8(&b[4..])?;
        Ok(PathIndex {
            generation,
            paths: if rest.is_empty() {
                Vec::new()
            } else {
                rest.split('\0').map(str::to_string).collect()
            },
        })
    }
}

impl PathIndex {
    /// The rows of the `path_index` table this value stands for.
    pub fn rows<'a>(&'a self, oid: &'a Oid) -> impl Iterator<Item = WriteOp<'a>> {
        self.paths.iter().map(move |p| WriteOp::PathIndex {
            path: p,
            generation: self.generation,
            oid,
        })
    }
}

/// Kind of the path-search rows.
pub const TRIGRAMS: &str = "path3";

fn trigrams(s: &str) -> Vec<[u8; 3]> {
    let b = s.as_bytes();
    let mut v: Vec<[u8; 3]> = b.windows(3).map(|w| [w[0], w[1], w[2]]).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Path-search rows for paths added by `changes` (and their directories).
pub fn trigram_keys(changes: &[Change]) -> Vec<Vec<u8>> {
    let mut paths = BTreeSet::new();
    for c in changes
        .iter()
        .filter(|c| c.old.is_none() && c.new.is_some())
    {
        let mut p = c.path.as_str();
        paths.insert(p);
        while let Some(i) = p.rfind('/') {
            p = &p[..i];
            paths.insert(p);
        }
    }
    let mut keys = Vec::new();
    for p in paths {
        let lower = p.to_lowercase();
        for t in trigrams(&lower) {
            let mut k = t.to_vec();
            k.extend(p.as_bytes());
            keys.push(k);
        }
    }
    keys
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FoundPath {
    pub path: String,
    /// `file`, `dir`, `symlink` or `submodule`.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

impl Repo {
    /// Paths at `rev` whose path contains `q` (case-insensitive), sorted,
    /// at most `limit`.
    pub fn find_paths(&self, rev: &Oid, q: &str, limit: usize) -> Result<Vec<FoundPath>> {
        let ql = q.to_lowercase();
        let kind = |e: &crate::object::TreeEntry| {
            if e.is_tree() {
                "dir"
            } else if e.is_gitlink() {
                "submodule"
            } else if e.mode == crate::object::MODE_SYMLINK {
                "symlink"
            } else {
                "file"
            }
        };
        let mut out = Vec::new();
        if ql.len() < 3 {
            // Too short for trigrams: walk the tree.
            let mut stack = vec![(String::new(), self.tree_of(rev)?)];
            let mut all = Vec::new();
            while let Some((dir, t)) = stack.pop() {
                for e in self.store.read_tree(&t)? {
                    let p = changes::join(&dir, &String::from_utf8_lossy(&e.name));
                    if p.to_lowercase().contains(&ql) {
                        all.push(FoundPath {
                            path: p.clone(),
                            kind: kind(&e),
                        });
                    }
                    if e.is_tree() {
                        stack.push((p, e.oid));
                    }
                }
            }
            all.sort_by(|a, b| a.path.cmp(&b.path));
            all.truncate(limit);
            return Ok(all);
        }
        self.backfill_trigrams()?;
        let mut candidates: Option<HashSet<String>> = None;
        for t in trigrams(&ql) {
            let keys = self.meta.derived_keys_with_prefix(TRIGRAMS, &t)?;
            let set: HashSet<String> = keys
                .iter()
                .map(|k| String::from_utf8_lossy(&k[3..]).into_owned())
                .collect();
            candidates = Some(match candidates {
                None => set,
                Some(c) => c.intersection(&set).cloned().collect(),
            });
        }
        let mut cands: Vec<String> = candidates
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.to_lowercase().contains(&ql))
            .collect();
        cands.sort();
        let tree = self.tree_of(rev)?;
        for p in cands {
            if out.len() >= limit {
                break;
            }
            if let Some(e) = changes::entry_at(&self.store, tree, &p)? {
                out.push(FoundPath {
                    kind: kind(&e),
                    path: p,
                });
            }
        }
        Ok(out)
    }

    /// Metadata written before path search existed has no trigram rows:
    /// add them, once, for every path in the path index.
    fn backfill_trigrams(&self) -> Result<()> {
        if self.meta.get_kv("path3")?.as_deref() == Some("1") {
            return Ok(());
        }
        let mut after = String::new();
        loop {
            let paths = self.meta.paths_after(&after, 2000)?;
            let Some(last) = paths.last().cloned() else {
                break;
            };
            let mut keys = Vec::new();
            for p in &paths {
                for t in trigrams(&p.to_lowercase()) {
                    let mut k = t.to_vec();
                    k.extend(p.as_bytes());
                    keys.push(k);
                }
            }
            let ops: Vec<WriteOp> = keys
                .iter()
                .map(|k| WriteOp::Derived {
                    kind: TRIGRAMS,
                    key: k,
                    value: b"",
                })
                .collect();
            self.meta.write_batch(&ops)?;
            after = last;
        }
        self.meta.set_kv("path3", "1")
    }
}
