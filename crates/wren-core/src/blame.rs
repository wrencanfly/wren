//! Incremental, lazily derived blame.
//!
//! Blame for (commit C, path P) is derived from blame for P at the previous
//! first-parent commit that changed P, plus a line diff. Following
//! trunk-based practice (and Scion), history is first-parent only.
//!
//! A query finds the file's whole first-parent change chain in one pass over
//! the path index, back to the newest change whose blame is already persisted
//! (or to the change that created the file). Each change's old and new blob
//! ids come from the changeset's change record, so no tree is read; each
//! blob is read once, in parallel batches. Persisting every step would cost
//! a write per change, so only the requested result and every
//! `CHECKPOINT`-th change of the file are stored, in one transaction: a later
//! query for a newer change starts from the newest stored value, and one for
//! an older revision re-derives at most `CHECKPOINT - 1` steps. Every stored
//! value is a pure function of the (immutable) history.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use imara_diff::{Algorithm, Diff, InternedInput};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::changes::{find_change, normalize_path, FileVersion, Reader};
use crate::meta::MetaDb;
use crate::object::{MODE_GITLINK, MODE_TREE};
use crate::oid::Oid;
use crate::repo::Repo;

/// Storage kind of persisted blame. v2 changed the key to (path, commit),
/// added the step number, and switched the line diff from a minimal Myers
/// diff to git's (Myers with git's slider/indent heuristic), so rows of v1
/// must not be mixed in.
const KIND: &str = "blame.v2";

/// Every change of a file whose (non-zero) step number is a multiple of this
/// is persisted as a checkpoint.
const CHECKPOINT: u32 = 32;

/// Blobs read in parallel (and held in memory) at a time.
const BLOB_BATCH: usize = 64;

/// The largest number of path-index rows examined per batch.
const MAX_BATCH: usize = 512;

/// Per line: (index into `commits`, 1-based line number in that commit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameData {
    /// Number of earlier first-parent changes of the file since it was
    /// (last) created: 0 for the change that created it.
    pub step: u32,
    pub commits: Vec<Oid>,
    pub lines: Vec<(u32, u32)>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlameLine {
    pub line: usize,
    pub commit: Oid,
    pub orig_line: u32,
    pub content: String,
}

impl BlameData {
    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(12 + 20 * self.commits.len() + 8 * self.lines.len());
        b.extend(self.step.to_le_bytes());
        b.extend((self.commits.len() as u32).to_le_bytes());
        self.commits.iter().for_each(|c| b.extend(c.0));
        b.extend((self.lines.len() as u32).to_le_bytes());
        for (c, l) in &self.lines {
            b.extend(c.to_le_bytes());
            b.extend(l.to_le_bytes());
        }
        b
    }

    fn decode(b: &[u8]) -> Result<BlameData> {
        let mut r = Reader { b, i: 0 };
        let step = r.u32()?;
        let nc = r.u32()? as usize;
        let commits: Vec<Oid> = (0..nc).map(|_| r.oid()).collect::<Result<_>>()?;
        let nl = r.u32()? as usize;
        let lines: Vec<(u32, u32)> = (0..nl)
            .map(|_| Ok((r.u32()?, r.u32()?)))
            .collect::<Result<_>>()?;
        if lines.iter().any(|&(c, _)| c as usize >= nc) {
            bail!("corrupt blame data");
        }
        Ok(BlameData {
            step,
            commits,
            lines,
        })
    }

    fn attribution(&self, i: usize) -> (Oid, u32) {
        let (c, l) = self.lines[i];
        (self.commits[c as usize], l)
    }
}

/// Keys are `path NUL commit`, so one range scan finds every persisted
/// value for a path (git paths never contain NUL).
fn key_prefix(path: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(path.len() + 21);
    k.extend(path.as_bytes());
    k.push(0);
    k
}

fn key(path: &str, commit: &Oid) -> Vec<u8> {
    let mut k = key_prefix(path);
    k.extend(commit.0);
    k
}

pub fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    data.split_inclusive(|&b| b == b'\n').collect()
}

/// A first-parent change to the file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Step {
    commit: Oid,
    /// The file's blob at `commit`.
    blob: Oid,
    /// Its blob at the first parent; None if `commit` created the file.
    prev: Option<Oid>,
}

struct Chain {
    /// Newest first. Empty if the requested change itself is persisted.
    steps: Vec<Step>,
    /// The persisted change just before the oldest step (or the requested
    /// change, if `steps` is empty). None if the oldest step created the file.
    base: Option<Oid>,
}

/// A blob id and its contents.
type Blob = (Oid, Arc<Vec<u8>>);

/// A version as `file_blob` sees it: directories and submodules are not files.
fn file_oid(v: Option<FileVersion>) -> Option<Oid> {
    v.filter(|v| v.mode != MODE_TREE && v.mode != MODE_GITLINK)
        .map(|v| v.oid)
}

impl Repo {
    /// The blob at `path`, if it is a file (a regular file or a symlink).
    fn file_blob(&self, commit: &Oid, path: &str) -> Result<Option<Oid>> {
        Ok(self
            .entry(commit, path)?
            .filter(|e| !e.is_tree() && !e.is_gitlink())
            .map(|e| e.oid))
    }

    /// (file at the first parent, file at `c`) per `file_blob`.
    ///
    /// The change record for `path` says the same without reading a tree.
    /// A commit that changed `path` has a record for it exactly when `path`
    /// is a non-directory on at least one side, and the record holds each
    /// such side's entry: a file that replaces a directory (or the reverse)
    /// is recorded as added (deleted) at `path` itself, and a submodule keeps
    /// its gitlink mode. Without a record, `path` was a directory or absent
    /// on both sides. Records name paths decoded lossily from UTF-8, so for
    /// a path that could be such a decoding the trees are read instead.
    fn file_versions(
        &self,
        c: &Oid,
        path: &str,
        records: bool,
    ) -> Result<(Option<Oid>, Option<Oid>)> {
        if records {
            let raw = self
                .meta
                .changeset_changes(c)?
                .ok_or_else(|| crate::not_found(format!("unknown changeset {c}")))?;
            return Ok(match find_change(&raw, path)? {
                Some(ch) => (file_oid(ch.old), file_oid(ch.new)),
                None => (None, None),
            });
        }
        let new = self.file_blob(c, path)?;
        let old = match self.node(c)?.parents.first() {
            Some(p) => self.file_blob(p, path)?,
            None => None,
        };
        Ok((old, new))
    }

    /// The first-parent changes of `path` up to `rev`, newest first, back to
    /// the newest one with persisted blame or the one that created the file.
    /// `path` must be a file at `rev`.
    fn change_chain(&self, rev: &Oid, path: &str) -> Result<Chain> {
        let persisted: HashSet<Oid> = self
            .meta
            .derived_keys_with_prefix(KIND, &key_prefix(path))?
            .iter()
            .filter_map(|k| Oid::from_bytes(&k[k.len().saturating_sub(20)..]).ok())
            .collect();
        let records = !path.contains(char::REPLACEMENT_CHARACTER);
        // The path index lists every commit that changed `path`, on any
        // branch, by decreasing generation; the changes are those on the
        // first-parent chain. Below a change C, rev's chain *is* C's chain,
        // so a batch of candidates can be checked in parallel against the
        // newest change found before it: the chain only shrinks, so a
        // rejected candidate never qualifies later.
        // The first batch is small and sequential: when the newest change is
        // already persisted, that is all the work there is.
        let mut tip = *rev;
        let mut cursor = (self.node(rev)?.generation, None);
        let mut batch = 4;
        let mut steps: Vec<Step> = Vec::new();
        loop {
            let page = self.meta.path_changes(path, cursor, batch)?;
            let on_chain = map_maybe_par(&page, |(_, c)| self.on_first_parent_chain(&tip, c))?;
            let found: Vec<Oid> = page
                .iter()
                .zip(&on_chain)
                .filter(|(_, &on)| on)
                .map(|((_, c), _)| *c)
                .collect();
            let versions = map_maybe_par(&found, |c| match persisted.contains(c) {
                true => Ok((None, None)),
                false => self.file_versions(c, path, records),
            })?;
            for (c, (prev, blob)) in found.into_iter().zip(versions) {
                if persisted.contains(&c) {
                    return Ok(Chain {
                        steps,
                        base: Some(c),
                    });
                }
                let blob = blob.with_context(|| format!("blame: {path} missing at change {c}"))?;
                if let Some(newer) = steps.last() {
                    // No first-parent commit in between changed the file.
                    if newer.prev != Some(blob) {
                        bail!("blame: history of {path} is inconsistent at {c}");
                    }
                }
                steps.push(Step {
                    commit: c,
                    blob,
                    prev,
                });
                if prev.is_none() {
                    return Ok(Chain { steps, base: None });
                }
                tip = c;
            }
            match page.last() {
                Some(&(g, o)) if page.len() == batch => cursor = (g, Some(o)),
                _ => break,
            }
            batch = (batch * 4).min(MAX_BATCH);
        }
        match steps.is_empty() {
            true => bail!("blame: {path} exists but no change introduced it"),
            false => bail!("blame: missing earlier change to {path}"),
        }
    }

    /// Blame `path` as of `rev`. Returns None if the path is not a file there.
    pub fn blame(&self, rev: &Oid, path: &str) -> Result<Option<Vec<BlameLine>>> {
        let path = normalize_path(path);
        let Some(blob) = self.file_blob(rev, &path)? else {
            return Ok(None);
        };
        let chain = self.change_chain(rev, &path)?;
        let (data, newest) = self.derive_chain(&path, chain)?;
        let content = match newest {
            Some((b, content)) if b == blob => content,
            _ => Arc::new(self.store.read_blob(&blob)?),
        };
        let lines = split_lines(&content);
        if lines.len() != data.lines.len() {
            bail!("blame data for {path} at {rev} is inconsistent with file contents");
        }
        Ok(Some(
            lines
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let (commit, orig_line) = data.attribution(i);
                    BlameLine {
                        line: i + 1,
                        commit,
                        orig_line,
                        content: String::from_utf8_lossy(l)
                            .trim_end_matches(['\n', '\r'])
                            .to_string(),
                    }
                })
                .collect(),
        ))
    }

    /// Blame data at the newest change of `chain`, plus that change's blob
    /// and contents if they were read. Persists the result and checkpoints.
    fn derive_chain(&self, path: &str, chain: Chain) -> Result<(BlameData, Option<Blob>)> {
        let mut prev: Option<BlameData> = match chain.base {
            Some(b) => Some(BlameData::decode(
                &self
                    .meta
                    .get_derived(KIND, &key(path, &b))?
                    .context("blame: persisted value vanished")?,
            )?),
            None => None,
        };
        let mut steps = chain.steps;
        if steps.is_empty() {
            return Ok((prev.context("blame: nothing to derive")?, None));
        }
        steps.reverse();
        let n = steps.len();
        let mut writes = Vec::new();
        // The file's contents before the step being derived.
        let mut prev_content: Option<Arc<Vec<u8>>> = None;
        let mut done = 0;
        for chunk in steps.chunks(BLOB_BATCH) {
            let mut ids: Vec<Oid> = chunk.iter().map(|s| s.blob).collect();
            let need_base = done == 0 && prev.is_some();
            if need_base {
                ids.insert(0, chunk[0].prev.context("blame: base version missing")?);
            }
            let mut contents = map_maybe_par(&ids, |b| self.store.read_blob(b).map(Arc::new))?;
            if need_base {
                prev_content = Some(contents.remove(0));
            }
            // One interner per batch: each step only interns its new lines.
            let carried = prev_content.clone();
            let mut input: InternedInput<&[u8]> = InternedInput::default();
            if let Some(c) = &carried {
                input.update_after(c.split_inclusive(|&b| b == b'\n'));
            }
            for (s, content) in chunk.iter().zip(&contents) {
                std::mem::swap(&mut input.before, &mut input.after);
                input.update_after(content.split_inclusive(|&b| b == b'\n'));
                let data = match &prev {
                    None => BlameData {
                        step: 0,
                        commits: vec![s.commit],
                        lines: (1..=input.after.len() as u32).map(|l| (0, l)).collect(),
                    },
                    Some(p) => derive_step(s.commit, p, &input)?,
                };
                done += 1;
                if done == n || (data.step > 0 && data.step % CHECKPOINT == 0) {
                    writes.push((key(path, &s.commit), data.encode()));
                }
                prev = Some(data);
            }
            prev_content = contents.last().cloned();
        }
        self.meta.write(|tx| {
            for (k, v) in &writes {
                MetaDb::put_derived_tx(tx, KIND, k, v)?;
            }
            Ok(())
        })?;
        let newest = steps.last().map(|s| s.blob).zip(prev_content);
        Ok((prev.context("blame: nothing derived")?, newest))
    }
}

/// `items.iter().map(f).collect()`, in parallel unless there are only a few
/// (spreading a handful of quick lookups over threads costs more than it saves).
fn map_maybe_par<T: Sync, R: Send>(
    items: &[T],
    f: impl Fn(&T) -> Result<R> + Sync + Send,
) -> Result<Vec<R>> {
    match items.len() {
        0..=4 => items.iter().map(f).collect(),
        _ => items.par_iter().map(f).collect(),
    }
}

#[cfg(all(test, unix))]
mod tests;

/// Carry attributions across one change (`input.before` -> `input.after`):
/// unchanged lines keep their origin, inserted or replaced lines are
/// attributed to `commit`.
///
/// The diff is git's default: Myers (with xdiff's cost heuristic, so it is
/// fast and deterministic but not always minimal) followed by git's slider
/// post-processing with the indent heuristic. Which of several equally good
/// alignments is chosen decides which commit a line is blamed on.
fn derive_step(commit: Oid, prev: &BlameData, input: &InternedInput<&[u8]>) -> Result<BlameData> {
    if prev.lines.len() != input.before.len() {
        bail!("blame data is inconsistent with file contents");
    }
    let mut diff = Diff::compute(Algorithm::Myers, input);
    diff.postprocess_lines(input);
    let mut commits = vec![commit];
    // Index in `prev.commits` -> index in `commits`.
    let mut remap = vec![u32::MAX; prev.commits.len()];
    let new_len = input.after.len();
    let mut lines = Vec::with_capacity(new_len);
    let mut keep = |lines: &mut Vec<(u32, u32)>, old: usize| {
        let (c, l) = prev.lines[old];
        let r = &mut remap[c as usize];
        if *r == u32::MAX {
            *r = commits.len() as u32;
            commits.push(prev.commits[c as usize]);
        }
        lines.push((*r, l));
    };
    let mut old = 0usize;
    for h in diff.hunks() {
        while lines.len() < h.after.start as usize {
            keep(&mut lines, old);
            old += 1;
        }
        lines.extend(h.after.map(|i| (0, i + 1)));
        old = h.before.end as usize;
    }
    while lines.len() < new_len {
        keep(&mut lines, old);
        old += 1;
    }
    Ok(BlameData {
        step: prev.step + 1,
        commits,
        lines,
    })
}
