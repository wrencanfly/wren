//! Rename and copy detection, as git does it (`-M` / `-C`, 50%).
//!
//! A changeset records files added and deleted, not renames. Renames are
//! derived from the change list (`Copies`, a derived type, computed lazily
//! per changeset and persisted):
//!
//! 1. exact: a deleted and an added file with the same blob (a same
//!    basename is preferred among several);
//! 2. inexact: every remaining (deleted, added) pair of regular files is
//!    scored with git's similarity estimate (`diffcore-delta`: content
//!    split into chunks at newlines or every 64 bytes, hashed; the score is
//!    the bytes in common over the larger size), and pairs at or above 50%
//!    are taken best first, each source at most once;
//! 3. copies (only when asked): sources also include the old versions of
//!    files modified in the same changeset, and may be used more than once.
//!
//! When several sources are equally good the choice can differ from git's
//! (git also weighs basename heuristics); a single candidate gives git's
//! answer.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::changes::{self, Change, FileVersion};
use crate::derived::Derivable;
use crate::meta::ChangesetRow;
use crate::object::{MODE_GITLINK, MODE_SYMLINK};
use crate::oid::Oid;
use crate::repo::Repo;
use crate::store::ObjectStore;

/// git's scores are out of 60000; 50% is the default threshold.
const MAX_SCORE: u64 = 60_000;
const MIN_SCORE: u64 = 30_000;
const HASHBASE: u32 = 107_927;
/// Inexact detection is skipped when there are more candidate pairs than
/// this (git's `diff.renameLimit` is about files; this bounds work).
const MAX_PAIRS: usize = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CopyKind {
    Rename,
    Copy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyInfo {
    pub from: String,
    pub to: String,
    /// Similarity, 0–100 (100: identical content).
    pub score: u32,
    pub kind: CopyKind,
}

fn kind_of_mode(m: u32) -> u32 {
    m & 0o170000
}

fn is_regular(m: u32) -> bool {
    m != MODE_SYMLINK && m != MODE_GITLINK && kind_of_mode(m) == 0o100000
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// git's span hashes of `data`: hash value -> bytes, sorted.
fn spans(data: &[u8]) -> Vec<(u32, u64)> {
    let is_text = !data[..data.len().min(8000)].contains(&0);
    let mut map: HashMap<u32, u64> = HashMap::new();
    let (mut accum1, mut accum2, mut n): (u32, u32, u64) = (0, 0, 0);
    let mut i = 0;
    while i < data.len() {
        let c = data[i] as u32;
        i += 1;
        let old_1 = accum1;
        // CR in a CRLF pair is ignored in text.
        if is_text && c == b'\r' as u32 && i < data.len() && data[i] == b'\n' {
            continue;
        }
        accum1 = (accum1 << 7) ^ (accum2 >> 25);
        accum2 = (accum2 << 7) ^ (old_1 >> 25);
        accum1 = accum1.wrapping_add(c);
        n += 1;
        if n < 64 && c != b'\n' as u32 {
            continue;
        }
        let h = accum1.wrapping_add(accum2.wrapping_mul(0x61)) % HASHBASE;
        *map.entry(h).or_default() += n;
        n = 0;
        accum1 = 0;
        accum2 = 0;
    }
    if n > 0 {
        let h = accum1.wrapping_add(accum2.wrapping_mul(0x61)) % HASHBASE;
        *map.entry(h).or_default() += n;
    }
    let mut v: Vec<(u32, u64)> = map.into_iter().collect();
    v.sort_unstable();
    v
}

/// Bytes of `src` that also occur in `dst`, per git.
fn copied(src: &[(u32, u64)], dst: &[(u32, u64)]) -> u64 {
    let (mut i, mut j, mut sc) = (0, 0, 0);
    while i < src.len() && j < dst.len() {
        match src[i].0.cmp(&dst[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                sc += src[i].1.min(dst[j].1);
                i += 1;
                j += 1;
            }
        }
    }
    sc
}

/// git's similarity score (out of `MAX_SCORE`) of two regular files.
pub fn similarity(src: &[u8], dst: &[u8]) -> u64 {
    let max = src.len().max(dst.len()) as u64;
    let base = src.len().min(dst.len()) as u64;
    if max * (MAX_SCORE - MIN_SCORE) < (max - base) * MAX_SCORE {
        return 0;
    }
    if dst.is_empty() {
        return 0;
    }
    copied(&spans(src), &spans(dst)) * MAX_SCORE / max
}

/// Renames (and, with `copies`, copies) among `changes`, a change list
/// between two trees.
pub fn detect(store: &ObjectStore, changes: &[Change], copies: bool) -> Result<Vec<CopyInfo>> {
    // Sources: deleted files; with copies, also modified files' old side.
    let mut sources: Vec<(&str, FileVersion, bool)> = Vec::new(); // (path, version, deleted)
    let mut dsts: Vec<(&str, FileVersion)> = Vec::new();
    for c in changes {
        match (c.old, c.new) {
            (Some(o), None) if o.mode != MODE_GITLINK => sources.push((&c.path, o, true)),
            (Some(o), Some(_)) if copies && o.mode != MODE_GITLINK => {
                sources.push((&c.path, o, false))
            }
            (None, Some(n)) if n.mode != MODE_GITLINK => dsts.push((&c.path, n)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    let mut matched: HashSet<usize> = HashSet::new(); // dst index
    let mut used: HashSet<usize> = HashSet::new(); // src index used by a rename
    let kind = |src_deleted: bool, renamed_already: bool| {
        if src_deleted && !renamed_already {
            CopyKind::Rename
        } else {
            CopyKind::Copy
        }
    };

    // 1. Exact: same blob (and same file type).
    let mut by_blob: HashMap<Oid, Vec<usize>> = HashMap::new();
    for (i, s) in sources.iter().enumerate() {
        by_blob.entry(s.1.oid).or_default().push(i);
    }
    for (di, d) in dsts.iter().enumerate() {
        let Some(cands) = by_blob.get(&d.1.oid) else {
            continue;
        };
        let pick = cands
            .iter()
            .filter(|&&si| kind_of_mode(sources[si].1.mode) == kind_of_mode(d.1.mode))
            .filter(|&&si| copies || (sources[si].2 && !used.contains(&si)))
            .max_by_key(|&&si| {
                (
                    sources[si].2 && !used.contains(&si),
                    basename(sources[si].0) == basename(d.0),
                    std::cmp::Reverse(si),
                )
            });
        if let Some(&si) = pick {
            let rename = sources[si].2 && !used.contains(&si);
            if !rename && !copies {
                continue;
            }
            out.push(CopyInfo {
                from: sources[si].0.to_string(),
                to: d.0.to_string(),
                score: 100,
                kind: kind(sources[si].2, used.contains(&si)),
            });
            if rename {
                used.insert(si);
            }
            matched.insert(di);
        }
    }

    // 2. Inexact, regular files only.
    let srcs: Vec<usize> = (0..sources.len())
        .filter(|&i| is_regular(sources[i].1.mode))
        .filter(|&i| copies || !used.contains(&i))
        .collect();
    let left: Vec<usize> = (0..dsts.len())
        .filter(|i| !matched.contains(i) && is_regular(dsts[*i].1.mode))
        .collect();
    if !srcs.is_empty() && !left.is_empty() && srcs.len() * left.len() <= MAX_PAIRS {
        let read = |o: &Oid| -> Result<Arc<Vec<u8>>> { Ok(Arc::new(store.read_blob(o)?)) };
        let src_data: Vec<Arc<Vec<u8>>> = srcs
            .iter()
            .map(|&i| read(&sources[i].1.oid))
            .collect::<Result<_>>()?;
        let src_spans: Vec<Vec<(u32, u64)>> = src_data.iter().map(|d| spans(d)).collect();
        // (score, same basename, src, dst)
        let mut cands: Vec<(u64, bool, usize, usize)> = Vec::new();
        for &di in &left {
            let dd = read(&dsts[di].1.oid)?;
            let ds = spans(&dd);
            for (k, &si) in srcs.iter().enumerate() {
                let s = &src_data[k];
                let max = s.len().max(dd.len()) as u64;
                let base = s.len().min(dd.len()) as u64;
                if max * (MAX_SCORE - MIN_SCORE) < (max - base) * MAX_SCORE || dd.is_empty() {
                    continue;
                }
                let score = copied(&src_spans[k], &ds) * MAX_SCORE / max;
                if score >= MIN_SCORE {
                    cands.push((
                        score,
                        basename(sources[si].0) == basename(dsts[di].0),
                        si,
                        di,
                    ));
                }
            }
        }
        cands.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(b.1.cmp(&a.1))
                .then(a.3.cmp(&b.3))
                .then(a.2.cmp(&b.2))
        });
        // Renames first, then (with copies) copies.
        for pass_copies in [false, true] {
            if pass_copies && !copies {
                break;
            }
            for &(score, _, si, di) in &cands {
                if matched.contains(&di) {
                    continue;
                }
                let rename = sources[si].2 && !used.contains(&si);
                if !pass_copies && !rename {
                    continue;
                }
                out.push(CopyInfo {
                    from: sources[si].0.to_string(),
                    to: dsts[di].0.to_string(),
                    score: (score * 100 / MAX_SCORE) as u32,
                    kind: if rename {
                        CopyKind::Rename
                    } else {
                        CopyKind::Copy
                    },
                });
                if rename {
                    used.insert(si);
                }
                matched.insert(di);
            }
        }
    }
    out.sort_by(|a, b| a.to.cmp(&b.to));
    Ok(out)
}

/// Derived type: renames and copies in a changeset, relative to its first
/// parent. Depends on the changeset alone, so it is derived on its own
/// (not recursively) when first asked for.
pub struct Copies(pub Vec<CopyInfo>);

impl Derivable for Copies {
    const KIND: &'static str = "copies.v1";
    const RECURSIVE: bool = false;

    fn derive(repo: &Repo, cs: &ChangesetRow, _parents: &[Arc<Self>]) -> Result<Self> {
        let changes = changes::decode_changes(&cs.changes)?;
        Ok(Copies(detect(&repo.store, &changes, true)?))
    }

    fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(&self.0).unwrap()
    }

    fn decode(b: &[u8]) -> Result<Self> {
        Ok(Copies(serde_json::from_slice(b)?))
    }
}

impl Repo {
    /// Where the file `path` added in changeset `cs` was renamed from (git
    /// `-M`), if it was.
    pub fn renamed_from(&self, cs: &Oid, path: &str) -> Result<Option<String>> {
        let copies = self.derived::<Copies>(cs)?;
        Ok(copies
            .0
            .iter()
            .find(|c| c.to == path && c.kind == CopyKind::Rename)
            .map(|c| c.from.clone()))
    }

    /// `log --follow`: the first-parent history of the file `path` as of
    /// `tip`, continuing under its old name across renames. Returns
    /// (commit, the file's name in it), newest first.
    pub fn log_follow(&self, tip: &Oid, path: &str, limit: usize) -> Result<Vec<(Oid, String)>> {
        let mut path = changes::normalize_path(path);
        let mut out: Vec<(Oid, String)> = Vec::new();
        let mut from = *tip;
        'paths: loop {
            let mut cursor = (self.node(&from)?.generation, None);
            loop {
                let page = self.meta.path_changes(&path, cursor, 512)?;
                for (_, c) in &page {
                    if !self.on_first_parent_chain(&from, c)? {
                        continue;
                    }
                    out.push((*c, path.clone()));
                    if out.len() >= limit {
                        break 'paths;
                    }
                    // Did this commit create the file? Then follow a rename.
                    let raw = self
                        .meta
                        .changeset_changes(c)?
                        .ok_or_else(|| crate::not_found(format!("unknown changeset {c}")))?;
                    let created = changes::find_change(&raw, &path)?
                        .is_some_and(|ch| ch.old.is_none() && ch.new.is_some());
                    if created {
                        let Some(old) = self.renamed_from(c, &path)? else {
                            break 'paths;
                        };
                        let Some(parent) = self.node(c)?.parents.first().copied() else {
                            break 'paths;
                        };
                        path = old;
                        from = parent;
                        continue 'paths;
                    }
                }
                match page.last() {
                    Some(&(g, o)) if page.len() == 512 => cursor = (g, Some(o)),
                    _ => break 'paths,
                }
            }
        }
        Ok(out)
    }

    /// Renames and copies between two trees (git `-M -C`).
    pub fn diff_copies(&self, a: &Oid, b: &Oid) -> Result<Vec<CopyInfo>> {
        let changes = self.diff(a, b)?;
        detect(&self.store, &changes, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_bounds() {
        let a = b"one\ntwo\nthree\nfour\n".repeat(10);
        assert_eq!(similarity(&a, &a), MAX_SCORE);
        let mut b = a.clone();
        b.extend(b"five\n");
        let s = similarity(&a, &b);
        assert!(s > MIN_SCORE && s < MAX_SCORE, "{s}");
        // Size alone rules it out.
        assert_eq!(similarity(b"x\n", &b"y\n".repeat(100)), 0);
    }
}
