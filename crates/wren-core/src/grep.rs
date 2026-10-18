//! Server-side `git grep` at a revision, so a job or an agent can search
//! code without a checkout. Results are in git's path order and are a pure
//! function of (revision, path, pattern, flags), so pinned queries cache
//! forever like every other query.

use anyhow::Result;
use rayon::prelude::*;
use regex::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::object::MODE_TREE;
use crate::oid::Oid;
use crate::repo::Repo;
use crate::{bad_request, not_found};

/// Longest line text returned per match; longer lines are cut.
const MAX_LINE: usize = 400;
/// Files searched in parallel between checks of the match limit.
const CHUNK: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrepMatch {
    pub path: String,
    /// 1-based line number.
    pub line: usize,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrepResponse {
    pub rev: Oid,
    pub matches: Vec<GrepMatch>,
    /// True when the match limit cut the result short.
    pub truncated: bool,
}

impl Repo {
    /// Lines matching `pattern` (a Rust regex) in files under `path` at
    /// `rev`. Binary files (a NUL in the first 8000 bytes, as in git) and
    /// submodules are skipped.
    pub fn grep(
        &self,
        rev: &Oid,
        pattern: &str,
        path: &str,
        ignore_case: bool,
        limit: usize,
    ) -> Result<GrepResponse> {
        let re = RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
            .map_err(|e| bad_request(format!("invalid pattern: {e}")))?;
        let root = self
            .entry(rev, path)?
            .ok_or_else(|| not_found(format!("no path {path:?} at {rev}")))?;
        let base = crate::changes::normalize_path(path);
        let mut files = Vec::new();
        if root.mode == MODE_TREE {
            self.collect_files(root.oid, &base, &mut files)?;
        } else if !root.is_gitlink() {
            files.push((base, root.oid));
        }

        // Files that cannot match, by the content index, are not read.
        if let Some(tris) = crate::search::required_trigrams(pattern, ignore_case) {
            if std::env::var_os("WREN_NO_CONTENT_INDEX").is_none() {
                self.prune_by_index(&mut files, &tris)?;
            }
        }

        let mut matches = Vec::new();
        let mut truncated = false;
        for chunk in files.chunks(CHUNK) {
            let found: Vec<Vec<GrepMatch>> = chunk
                .par_iter()
                .map(|(p, oid)| Ok(search(&re, p, &self.store.read_blob(oid)?)))
                .collect::<Result<_>>()?;
            for m in found.into_iter().flatten() {
                if matches.len() == limit {
                    truncated = true;
                    break;
                }
                matches.push(m);
            }
            if truncated {
                break;
            }
        }
        Ok(GrepResponse {
            rev: *rev,
            matches,
            truncated,
        })
    }

    /// Depth-first in tree order, which is git's index (path) order.
    pub fn collect_files(
        &self,
        tree: Oid,
        prefix: &str,
        out: &mut Vec<(String, Oid)>,
    ) -> Result<()> {
        for e in self.store.read_tree(&tree)? {
            let p = if prefix.is_empty() {
                e.name_str().into_owned()
            } else {
                format!("{prefix}/{}", e.name_str())
            };
            if e.is_tree() {
                self.collect_files(e.oid, &p, out)?;
            } else if !e.is_gitlink() {
                out.push((p, e.oid));
            }
        }
        Ok(())
    }
}

fn search(re: &Regex, path: &str, data: &[u8]) -> Vec<GrepMatch> {
    if data[..data.len().min(8000)].contains(&0) {
        return Vec::new();
    }
    let mut out = Vec::new();
    // A trailing newline ends the last line; it does not start another.
    let body = data.strip_suffix(b"\n").unwrap_or(data);
    if body.is_empty() {
        return out;
    }
    for (i, line) in body.split(|&b| b == b'\n').enumerate() {
        if re.is_match(line) {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let mut text = String::from_utf8_lossy(line).into_owned();
            if text.len() > MAX_LINE {
                let mut cut = MAX_LINE;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
                text.push('…');
            }
            out.push(GrepMatch {
                path: path.to_string(),
                line: i + 1,
                text,
            });
        }
    }
    out
}
