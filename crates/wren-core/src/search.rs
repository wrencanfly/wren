//! Indexed content search: a trigram index of file contents, so that `grep`
//! reads only the files that can match.
//!
//! The index is over blobs, not revisions. A blob's content never changes,
//! so an indexed blob stays correctly indexed at every revision that has
//! it, and keeping the index current is just indexing the blobs the
//! default branch gains (`index_content`, run in the background).
//!
//! * Every indexed blob gets a number (`c3.blob`: oid -> number; `0` marks
//!   a text file over 1 MiB, which is not indexed, and `u32::MAX` a binary
//!   file, which grep skips anyway).
//! * For each trigram of a blob's content (ASCII-lowercased, never across
//!   a newline), posting rows `c3: <trigram><first number of the batch>`
//!   hold the numbers of the blobs that contain it, delta-encoded. Each
//!   indexing batch adds rows; every 32 batches, trigrams with many rows
//!   are merged into one.
//!
//! A search (`grep`) parses the pattern and takes the literal strings any
//! match must contain (`required_trigrams`): runs of literals and of
//! character classes that are a single ASCII byte up to case, through
//! concatenation, groups and repetitions of at least one. Alternations and
//! other classes contribute nothing. With `ignore_case`, letters such as
//! `k`, whose case folding includes non-ASCII characters (the Kelvin sign),
//! end a literal, so the trigrams stay sound. The candidates are the blobs
//! holding every such trigram; files at the revision whose blob is
//! indexed but not a candidate cannot match and are not read. Files whose
//! blob is not indexed are read as before. The answer is therefore exactly
//! the unindexed answer, only cheaper.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rayon::prelude::*;
use regex_syntax::hir::{Class, Hir, HirKind};
use serde::Serialize;

use crate::changes;
use crate::meta::WriteOp;
use crate::oid::Oid;
use crate::repo::Repo;

const KIND_POST: &str = "c3";
const KIND_BLOB: &str = "c3.blob";
/// The default-branch commit whose files are all indexed.
const KV_HEAD: &str = "c3.head";
/// The next blob number to hand out (numbers start at 1).
const KV_NEXT: &str = "c3.next";
/// Bumped after every batch: in-memory views of the blob numbers reload.
const KV_VERSION: &str = "c3.version";
/// Larger blobs are not indexed (grep reads them).
const MAX_FILE: usize = 1 << 20;
/// Blobs per batch (one metadata transaction).
const BATCH: usize = 2000;
const COMPACT_EVERY: u64 = 32;
/// A trigram with more posting rows than this is merged at compaction.
const MAX_ROWS: usize = 4;
/// Trigrams of a pattern looked up at most (any subset is still sound).
const MAX_QUERY_TRIGRAMS: usize = 16;

/// Blob -> number (see `Repo::indexed_blobs`).
type BlobNumbers = Arc<HashMap<Oid, u32>>;

/// Which blobs are indexed, cached per repository with the index version.
#[derive(Default)]
pub struct ContentCache(Mutex<Option<(String, BlobNumbers)>>);

#[derive(Clone, Debug, Default, Serialize)]
pub struct IndexReport {
    /// Blobs added to the index (excluding those not indexable).
    pub indexed: usize,
    /// Binary or too large.
    pub skipped: usize,
    /// The default branch commit now fully indexed.
    pub head: Option<Oid>,
}

/// `c3.blob` values that are not blob numbers.
const NOT_INDEXED: u32 = 0;
const BINARY: u32 = u32::MAX;

/// As in `grep` (and git): a NUL in the first 8000 bytes.
fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

/// Distinct trigrams of `data` (lowercased ASCII), as 24-bit codes, sorted.
fn trigrams(data: &[u8]) -> Vec<u32> {
    let mut out: Vec<u32> = data
        .windows(3)
        .filter(|w| !w.contains(&b'\n'))
        .map(|w| {
            (w[0].to_ascii_lowercase() as u32) << 16
                | (w[1].to_ascii_lowercase() as u32) << 8
                | w[2].to_ascii_lowercase() as u32
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn tri_key(t: u32) -> [u8; 3] {
    [(t >> 16) as u8, (t >> 8) as u8, t as u8]
}

fn encode_postings(nums: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nums.len() * 2);
    let mut prev = 0u32;
    for &n in nums {
        let mut d = n - prev;
        prev = n;
        loop {
            let b = (d & 0x7f) as u8;
            d >>= 7;
            if d == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }
    out
}

fn decode_postings(b: &[u8], out: &mut Vec<u32>) {
    let (mut prev, mut cur, mut shift) = (0u32, 0u32, 0);
    for &x in b {
        cur |= ((x & 0x7f) as u32) << shift;
        if x & 0x80 != 0 {
            shift += 7;
            continue;
        }
        prev += cur;
        out.push(prev);
        cur = 0;
        shift = 0;
    }
}

/// The trigrams every line matching `pattern` contains (lowercased), or
/// `None` when the pattern requires none (or cannot be analysed).
pub fn required_trigrams(pattern: &str, ignore_case: bool) -> Option<Vec<u32>> {
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(ignore_case)
        .utf8(false)
        .build()
        .parse(pattern)
        .ok()?;
    let (exact, mut musts) = literals(&hir);
    musts.extend(exact);
    let mut tris: Vec<u32> = musts.iter().flat_map(|m| trigrams(m)).collect();
    tris.sort_unstable();
    tris.dedup();
    if tris.is_empty() {
        return None;
    }
    if tris.len() > MAX_QUERY_TRIGRAMS {
        let step = tris.len() as f64 / MAX_QUERY_TRIGRAMS as f64;
        tris = (0..MAX_QUERY_TRIGRAMS)
            .map(|i| tris[(i as f64 * step) as usize])
            .collect();
    }
    Some(tris)
}

/// For a pattern node: the one lowercased string it always matches, if it
/// is that simple, and strings every match of it contains.
fn literals(h: &Hir) -> (Option<Vec<u8>>, Vec<Vec<u8>>) {
    match h.kind() {
        HirKind::Empty | HirKind::Look(_) => (Some(Vec::new()), Vec::new()),
        HirKind::Literal(l) => (Some(l.0.to_ascii_lowercase()), Vec::new()),
        HirKind::Class(c) => (class_byte(c).map(|b| vec![b]), Vec::new()),
        HirKind::Capture(c) => literals(&c.sub),
        HirKind::Repetition(r) => {
            if r.min == 0 {
                return (None, Vec::new());
            }
            let (exact, mut musts) = literals(&r.sub);
            if r.min == 1 && r.max == Some(1) {
                return (exact, musts);
            }
            musts.extend(exact);
            (None, musts)
        }
        HirKind::Concat(subs) => {
            let mut run: Vec<u8> = Vec::new();
            let mut musts = Vec::new();
            let mut all_exact = true;
            for s in subs {
                let (exact, m) = literals(s);
                musts.extend(m);
                match exact {
                    Some(e) => run.extend(e),
                    None => {
                        all_exact = false;
                        musts.push(std::mem::take(&mut run));
                    }
                }
            }
            if all_exact {
                (Some(run), musts)
            } else {
                musts.push(run);
                (None, musts)
            }
        }
        HirKind::Alternation(_) => (None, Vec::new()),
    }
}

/// The byte a class always matches up to ASCII case: every member is ASCII
/// and lowercases to it.
fn class_byte(c: &Class) -> Option<u8> {
    let mut members: Vec<u32> = Vec::new();
    match c {
        Class::Unicode(u) => {
            for r in u.ranges() {
                let (a, b) = (r.start() as u32, r.end() as u32);
                if b > 0x7f || b - a > 64 {
                    return None;
                }
                members.extend(a..=b);
            }
        }
        Class::Bytes(by) => {
            for r in by.ranges() {
                let (a, b) = (r.start() as u32, r.end() as u32);
                if b > 0x7f || b - a > 64 {
                    return None;
                }
                members.extend(a..=b);
            }
        }
    }
    let first = (*members.first()? as u8).to_ascii_lowercase();
    members
        .iter()
        .all(|&m| (m as u8).to_ascii_lowercase() == first)
        .then_some(first)
}

impl Repo {
    /// Index the blobs of the default branch that are not indexed yet.
    pub fn index_content(&self) -> Result<IndexReport> {
        let mut report = IndexReport::default();
        let Some(head) = self.head_ref()? else {
            return Ok(report);
        };
        let Some(tip) = self.meta.get_ref(&head)?.map(|r| r.target) else {
            return Ok(report);
        };
        report.head = Some(tip);
        if self.meta.get_kv(KV_HEAD)? == Some(tip.to_hex()) {
            return Ok(report);
        }
        let _lock = self.meta.lock("content-index")?;
        let done = self
            .meta
            .get_kv(KV_HEAD)?
            .and_then(|s| s.parse::<Oid>().ok());
        if done == Some(tip) {
            return Ok(report);
        }
        let tree = self.tree_of(&tip)?;
        let mut wanted: Vec<Oid> = match done {
            Some(d) if self.meta.has_changeset(&d)? => {
                changes::diff_trees(&self.store, Some(self.tree_of(&d)?), Some(tree))?
                    .into_iter()
                    .filter_map(|c| c.new)
                    .filter(|v| v.mode != crate::object::MODE_GITLINK)
                    .map(|v| v.oid)
                    .collect()
            }
            _ => {
                let mut files = Vec::new();
                self.collect_files(tree, "", &mut files)?;
                files.into_iter().map(|(_, o)| o).collect()
            }
        };
        wanted.sort_unstable();
        wanted.dedup();
        let known = self.indexed_blobs()?;
        wanted.retain(|o| !known.contains_key(o));

        for chunk in wanted.chunks(BATCH) {
            // Trigrams, or why there are none.
            let grams: Vec<std::result::Result<Vec<u32>, u32>> = chunk
                .par_iter()
                .map(|o| {
                    let data = self.store.read_blob(o)?;
                    Ok(if is_binary(&data) {
                        Err(BINARY)
                    } else if data.len() > MAX_FILE {
                        Err(NOT_INDEXED)
                    } else {
                        Ok(trigrams(&data))
                    })
                })
                .collect::<Result<_>>()?;
            // Reserve the numbers first: a batch that fails after this
            // wastes them, and never reuses them.
            let first: u32 = self
                .meta
                .get_kv(KV_NEXT)?
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            let count = grams.iter().filter(|g| g.is_ok()).count() as u32;
            self.meta.set_kv(KV_NEXT, &(first + count).to_string())?;

            let mut numbers = Vec::with_capacity(chunk.len());
            let mut postings: HashMap<u32, Vec<u32>> = HashMap::new();
            let mut n = first;
            for g in &grams {
                match g {
                    Ok(tris) => {
                        for t in tris {
                            postings.entry(*t).or_default().push(n);
                        }
                        numbers.push(n);
                        n += 1;
                        report.indexed += 1;
                    }
                    Err(mark) => {
                        numbers.push(*mark);
                        report.skipped += 1;
                    }
                }
            }
            let blob_vals: Vec<[u8; 4]> = numbers.iter().map(|n| n.to_be_bytes()).collect();
            let post_rows: Vec<(Vec<u8>, Vec<u8>)> = postings
                .into_iter()
                .map(|(t, nums)| {
                    let mut key = tri_key(t).to_vec();
                    key.extend(first.to_be_bytes());
                    (key, encode_postings(&nums))
                })
                .collect();
            let mut ops: Vec<WriteOp> = chunk
                .iter()
                .zip(&blob_vals)
                .map(|(o, v)| WriteOp::Derived {
                    kind: KIND_BLOB,
                    key: o.as_bytes(),
                    value: v,
                })
                .collect();
            ops.extend(post_rows.iter().map(|(k, v)| WriteOp::Derived {
                kind: KIND_POST,
                key: k,
                value: v,
            }));
            self.meta.write_batch(&ops)?;
            let version: u64 = self
                .meta
                .get_kv(KV_VERSION)?
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
                + 1;
            self.meta.set_kv(KV_VERSION, &version.to_string())?;
            if version.is_multiple_of(COMPACT_EVERY) {
                self.compact_content_index()?;
            }
        }
        self.meta.set_kv(KV_HEAD, &tip.to_hex())?;
        Ok(report)
    }

    /// Merge the posting rows of trigrams that have many.
    fn compact_content_index(&self) -> Result<()> {
        for b in 0..=255u8 {
            let rows = self.meta.derived_with_prefix(KIND_POST, &[b])?;
            let mut i = 0;
            while i < rows.len() {
                let tri = &rows[i].0[..3];
                let j = i + rows[i..].iter().take_while(|r| &r.0[..3] == tri).count();
                if j - i > MAX_ROWS {
                    let mut nums = Vec::new();
                    for r in &rows[i..j] {
                        decode_postings(&r.1, &mut nums);
                    }
                    nums.sort_unstable();
                    nums.dedup();
                    let mut key = tri.to_vec();
                    key.extend(0u32.to_be_bytes());
                    let value = encode_postings(&nums);
                    let mut ops: Vec<WriteOp> = rows[i..j]
                        .iter()
                        .map(|r| WriteOp::DeleteDerived {
                            kind: KIND_POST,
                            key: &r.0,
                        })
                        .collect();
                    ops.push(WriteOp::Derived {
                        kind: KIND_POST,
                        key: &key,
                        value: &value,
                    });
                    self.meta.write_batch(&ops)?;
                }
                i = j;
            }
        }
        Ok(())
    }

    /// Blob -> number for every blob the index has seen (or `NOT_INDEXED`,
    /// `BINARY`). Reloaded when the index changed.
    pub(crate) fn indexed_blobs(&self) -> Result<BlobNumbers> {
        let version = self.meta.get_kv(KV_VERSION)?.unwrap_or_default();
        if let Some((v, m)) = &*self.content.0.lock().unwrap() {
            if *v == version {
                return Ok(m.clone());
            }
        }
        let map: HashMap<Oid, u32> = self
            .meta
            .derived_with_prefix(KIND_BLOB, &[])?
            .into_iter()
            .filter_map(|(k, v)| {
                let oid = Oid::from_bytes(&k).ok()?;
                let n = u32::from_be_bytes(v.get(..4)?.try_into().ok()?);
                Some((oid, n))
            })
            .collect();
        let map = Arc::new(map);
        *self.content.0.lock().unwrap() = Some((version, map.clone()));
        Ok(map)
    }

    /// Numbers of the blobs that contain every trigram in `tris`.
    pub(crate) fn content_candidates(&self, tris: &[u32]) -> Result<HashSet<u32>> {
        let mut lists: Vec<Vec<u32>> = Vec::with_capacity(tris.len());
        for t in tris {
            let mut nums = Vec::new();
            for (_, v) in self.meta.derived_with_prefix(KIND_POST, &tri_key(*t))? {
                decode_postings(&v, &mut nums);
            }
            if nums.is_empty() {
                return Ok(HashSet::new());
            }
            nums.sort_unstable();
            nums.dedup();
            lists.push(nums);
        }
        lists.sort_by_key(|l| l.len());
        let mut set: HashSet<u32> = lists.first().into_iter().flatten().copied().collect();
        for l in &lists[1.min(lists.len())..] {
            let other: HashSet<u32> = l.iter().copied().collect();
            set.retain(|n| other.contains(n));
            if set.is_empty() {
                break;
            }
        }
        Ok(set)
    }

    /// The files of `files` that can match a pattern requiring `tris`:
    /// those not indexed, and the indexed candidates.
    pub fn prune_by_index(&self, files: &mut Vec<(String, Oid)>, tris: &[u32]) -> Result<()> {
        let known = self.indexed_blobs()?;
        if known.is_empty() {
            return Ok(());
        }
        let candidates = self.content_candidates(tris)?;
        files.retain(|(_, oid)| match known.get(oid) {
            None | Some(&NOT_INDEXED) => true,
            Some(&BINARY) => false,
            Some(n) => candidates.contains(n),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postings_round_trip() {
        let nums = vec![1, 2, 300, 301, 70_000, 4_000_000_000];
        let mut out = Vec::new();
        decode_postings(&encode_postings(&nums), &mut out);
        assert_eq!(out, nums);
    }

    fn lits(p: &str, ic: bool) -> Vec<String> {
        let mut v: Vec<String> = required_trigrams(p, ic)
            .unwrap_or_default()
            .into_iter()
            .map(|t| String::from_utf8(tri_key(t).to_vec()).unwrap())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn required_literals() {
        assert_eq!(lits("Hello", false), ["ell", "hel", "llo"]);
        assert_eq!(lits("foo.*bar", false), ["bar", "foo"]);
        assert_eq!(lits("(abc)+x", false), ["abc"]);
        assert!(lits("abc|def", false).is_empty());
        assert!(lits("a?bcd?", false).is_empty() || lits("a?bcd?", false) == ["bc"]);
        assert!(lits("\\w+", false).is_empty());
        // Case-insensitive: ASCII letters are fine, except those whose
        // folding leaves ASCII (k: the Kelvin sign; s: the long s).
        assert_eq!(lits("HELLO", true), ["ell", "hel", "llo"]);
        assert!(lits("kkk", true).is_empty());
        assert_eq!(lits("mask_bits", true), ["_bi", "bit"]);
    }
}
