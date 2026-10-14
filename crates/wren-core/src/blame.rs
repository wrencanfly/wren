//! Incremental, lazily derived blame.
//!
//! Blame for (commit C, path P) is derived from blame for P at the previous
//! first-parent commit that changed P, plus a line diff. Results are persisted,
//! so blaming a file after one more change costs one diff, not a history walk.
//! Following trunk-based practice (and Scion), history is first-parent only.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use similar::{capture_diff_slices_deadline, Algorithm, DiffOp};

use crate::changes::{normalize_path, Reader};
use crate::oid::Oid;
use crate::repo::Repo;

const KIND: &str = "blame";

/// Per line: (index into `commits`, 1-based line number in that commit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameData {
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
        let mut b = Vec::new();
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
        let nc = r.u32()? as usize;
        let commits = (0..nc).map(|_| r.oid()).collect::<Result<_>>()?;
        let nl = r.u32()? as usize;
        let lines = (0..nl)
            .map(|_| Ok((r.u32()?, r.u32()?)))
            .collect::<Result<_>>()?;
        Ok(BlameData { commits, lines })
    }

    fn attribution(&self, i: usize) -> (Oid, u32) {
        let (c, l) = self.lines[i];
        (self.commits[c as usize], l)
    }
}

fn key(commit: &Oid, path: &str) -> Vec<u8> {
    let mut k = commit.0.to_vec();
    k.extend(path.as_bytes());
    k
}

pub fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    data.split_inclusive(|&b| b == b'\n').collect()
}

impl Repo {
    /// The last first-parent commit at or before `tip` that changed `path`.
    fn last_change(&self, tip: &Oid, path: &str) -> Result<Option<Oid>> {
        Ok(self.log(tip, Some(path), 1)?.into_iter().next())
    }

    fn file_blob(&self, commit: &Oid, path: &str) -> Result<Option<Oid>> {
        Ok(self
            .entry(commit, path)?
            .filter(|e| !e.is_tree() && !e.is_gitlink())
            .map(|e| e.oid))
    }

    /// Blame `path` as of `rev`. Returns None if the path is not a file there.
    pub fn blame(&self, rev: &Oid, path: &str) -> Result<Option<Vec<BlameLine>>> {
        Ok(self.blame_exact(rev, path)?.map(|(lines, _)| lines))
    }

    /// Like `blame`, plus whether the result is exact. It is not exact when
    /// a line diff hit its time budget; such results are neither persisted
    /// nor may they be served as immutable.
    pub fn blame_exact(&self, rev: &Oid, path: &str) -> Result<Option<(Vec<BlameLine>, bool)>> {
        let path = normalize_path(path);
        let Some(blob) = self.file_blob(rev, &path)? else {
            return Ok(None);
        };
        let last = self
            .last_change(rev, &path)?
            .context("file exists but no change introduced it")?;
        let (data, exact) = self.blame_at_change(&last, &path)?;
        let content = self.store.read_blob(&blob)?;
        let lines = split_lines(&content);
        if lines.len() != data.lines.len() {
            bail!("blame data for {path}@{last} is inconsistent with file contents");
        }
        Ok(Some((
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
            exact,
        )))
    }

    /// Blame data for `path` at `change`, a commit that changed `path`.
    fn blame_at_change(&self, change: &Oid, path: &str) -> Result<(Arc<BlameData>, bool)> {
        // Walk back through earlier changes to the path until we find a
        // persisted value or the commit that created the file.
        let mut chain = Vec::new();
        let mut base: Option<Arc<BlameData>> = None;
        let mut cur = *change;
        loop {
            if let Some(b) = self.meta.get_derived(KIND, &key(&cur, path))? {
                base = Some(Arc::new(BlameData::decode(&b)?));
                break;
            }
            chain.push(cur);
            let Some(&parent) = self.node(&cur)?.parents.first() else {
                break;
            };
            if self.file_blob(&parent, path)?.is_none() {
                break;
            }
            cur = self
                .last_change(&parent, path)?
                .context("blame: missing earlier change")?;
        }
        // `base` (if any) is the blame at the newest persisted change, which
        // is the first-parent predecessor change of chain.last().
        let mut prev: Option<(Oid, Arc<BlameData>)> = base.map(|b| (cur, b));
        // Persisted values must be a pure function of the history. A diff that
        // hit its time budget may be non-minimal, so neither it nor anything
        // derived from it is stored.
        let mut persist = true;
        for c in chain.into_iter().rev() {
            let new_blob = self
                .file_blob(&c, path)?
                .context("blame: file missing at change")?;
            let new_content = self.store.read_blob(&new_blob)?;
            let new_lines = split_lines(&new_content);
            let (data, exact) = match &prev {
                None => (
                    BlameData {
                        commits: vec![c],
                        lines: (1..=new_lines.len() as u32).map(|l| (0, l)).collect(),
                    },
                    true,
                ),
                Some((prev_commit, prev_data)) => {
                    let old_blob = self
                        .file_blob(prev_commit, path)?
                        .context("blame: old file missing")?;
                    let old_content = self.store.read_blob(&old_blob)?;
                    derive_step(c, prev_data, &split_lines(&old_content), &new_lines)
                }
            };
            persist &= exact;
            let data = Arc::new(data);
            if persist {
                self.meta
                    .put_derived(KIND, &key(&c, path), &data.encode())?;
            }
            prev = Some((c, data));
        }
        Ok((prev.context("blame: nothing derived")?.1, persist))
    }
}

/// Carry attributions across one change: unchanged lines keep their origin,
/// inserted or replaced lines are attributed to `commit`.
fn derive_step(commit: Oid, prev: &BlameData, old: &[&[u8]], new: &[&[u8]]) -> (BlameData, bool) {
    let mut commits = vec![commit];
    let mut index = std::collections::HashMap::new();
    index.insert(commit, 0u32);
    let mut lines = vec![(0u32, 0u32); new.len()];
    // Minimal (raw) Myers like git's xdiff: the heuristic variant gives up on
    // large edits and misattributes lines that survived a big refactor
    // (measured on git.git: 78% -> 99% agreement on builtin/blame.c). The
    // deadline bounds pathological inputs.
    let start = std::time::Instant::now();
    let deadline = Some(start + DIFF_BUDGET);
    for op in capture_diff_slices_deadline(Algorithm::RawMyers, old, new, deadline) {
        match op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                for i in 0..len {
                    let (c, l) = prev.attribution(old_index + i);
                    let ci = *index.entry(c).or_insert_with(|| {
                        commits.push(c);
                        commits.len() as u32 - 1
                    });
                    lines[new_index + i] = (ci, l);
                }
            }
            DiffOp::Insert {
                new_index, new_len, ..
            }
            | DiffOp::Replace {
                new_index, new_len, ..
            } => {
                for i in 0..new_len {
                    lines[new_index + i] = (0, (new_index + i + 1) as u32);
                }
            }
            DiffOp::Delete { .. } => {}
        }
    }
    // Finishing at (or past) the budget means the diff may have been cut short.
    (BlameData { commits, lines }, start.elapsed() < DIFF_BUDGET)
}

const DIFF_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
