//! JSON types of the Wren query API, shared by server and client.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use similar::TextDiff;

use crate::changes::{Change, Status};
use crate::object::{Kind, Signature, MODE_GITLINK};
use crate::oid::Oid;
use crate::repo::Repo;
use crate::store::ObjectStore;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoInfo {
    pub name: String,
    pub upstream: Option<String>,
    pub head: Option<String>,
    pub changesets: i64,
    /// Unix time of the last successful sync with upstream. Answers (e.g.
    /// "is X contained in main?") reflect upstream as of this moment.
    pub last_sync: Option<i64>,
    /// Error from the most recent sync attempt, if it failed.
    pub last_sync_error: Option<String>,
}

/// `GET /v1/capabilities`: what this tier speaks. Identical shape on every
/// tier; `tier` says which one answered.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capabilities {
    /// Protocol major version. Breaking changes bump it and move the routes
    /// to a new prefix (`/v2/...`, `/api/v2/...`).
    pub protocol: u32,
    pub object_format: String,
    pub tier: String,
    pub features: Vec<String>,
    /// Version of the query answers' semantics (see [`QUERY_SEMANTICS`]).
    #[serde(default)]
    pub semantics: String,
}

pub const PROTOCOL_VERSION: u32 = 1;

/// Version of the *answers*, separate from the wire protocol. A pinned query
/// is a pure function of immutable inputs and this version: when an algorithm
/// changes what it returns (e.g. a different line-diff heuristic in blame),
/// this is bumped, and caches key stored answers by it so old and new answers
/// never mix. Sent on every response as the `wren-semantics` header.
pub const QUERY_SEMANTICS: &str = "1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefInfo {
    pub name: String,
    pub target: Oid,
    pub commit: Oid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Resolved {
    pub oid: Oid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitInfo {
    pub oid: Oid,
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Signature,
    pub committer: Signature,
    pub message: String,
    pub generation: u32,
    /// Number of first-parent steps to a root commit. Two commits on the
    /// same first-parent history are exactly `depth` apart, which is what
    /// bisection needs.
    pub depth: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<Vec<Change>>,
}

/// `GET .../bisect?good=&bad=`: one step of first-parent bisection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BisectStep {
    /// Commit to test next, or null when the search is finished.
    pub next: Option<Oid>,
    /// When finished: the first bad commit on `bad`'s first-parent history
    /// (on main, typically the merge that introduced the problem).
    pub first_bad: Option<Oid>,
    /// Untested commits strictly between the good and bad bounds.
    pub candidates: u32,
    /// Upper bound on remaining test runs.
    pub steps_left: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogResponse {
    pub commits: Vec<CommitInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeBaseResponse {
    pub bases: Vec<Oid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IsAncestorResponse {
    pub result: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LandedResponse {
    /// The first commit on the ref's first-parent history containing the
    /// commit, or null if the ref does not contain it.
    pub landed: Option<Oid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffResponse {
    pub base: Oid,
    pub head: Oid,
    pub changes: Vec<Change>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlameCommit {
    pub author: String,
    pub time: i64,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlameResponse {
    pub rev: Oid,
    pub path: String,
    pub lines: Vec<crate::blame::BlameLine>,
    pub commits: BTreeMap<Oid, BlameCommit>,
    /// Always true. Earlier servers bounded each line diff by time and sent
    /// false (with `Cache-Control: no-store`) when one was cut short; the
    /// field stays so that their clients keep working.
    pub exact: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TreeEntryInfo {
    pub name: String,
    pub mode: String,
    pub kind: Kind,
    pub oid: Oid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TreeResponse {
    pub oid: Oid,
    pub entries: Vec<TreeEntryInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Human-readable message.
    pub error: String,
    /// Machine-readable code: `not_found`, `bad_request`, `unauthorized`,
    /// `too_large`, `upstream_unavailable` or `internal`.
    #[serde(default)]
    pub code: String,
}

impl Repo {
    pub fn commit_info(&self, oid: &Oid, with_changes: bool) -> Result<CommitInfo> {
        let c = self.commit(oid)?;
        let node = self.node(oid)?;
        Ok(CommitInfo {
            oid: *oid,
            tree: c.tree,
            parents: c.parents,
            author: c.author,
            committer: c.committer,
            message: c.message,
            generation: node.generation,
            depth: node.depth,
            changes: if with_changes {
                Some(self.changeset_changes(oid)?)
            } else {
                None
            },
        })
    }

    pub fn tree_listing(&self, rev: &Oid, path: &str) -> Result<Option<TreeResponse>> {
        let Some(e) = self.entry(rev, path)? else {
            return Ok(None);
        };
        if !e.is_tree() {
            return Ok(None);
        }
        let entries = self
            .store
            .read_tree(&e.oid)?
            .into_iter()
            .map(|t| TreeEntryInfo {
                name: t.name_str().into_owned(),
                mode: format!("{:06o}", t.mode),
                kind: if t.is_tree() {
                    Kind::Tree
                } else if t.mode == MODE_GITLINK {
                    Kind::Commit
                } else {
                    Kind::Blob
                },
                oid: t.oid,
            })
            .collect();
        Ok(Some(TreeResponse {
            oid: e.oid,
            entries,
        }))
    }

    pub fn blame_response(&self, rev: &Oid, path: &str) -> Result<Option<BlameResponse>> {
        let Some(lines) = self.blame(rev, path)? else {
            return Ok(None);
        };
        let mut commits = BTreeMap::new();
        for l in &lines {
            if let std::collections::btree_map::Entry::Vacant(e) = commits.entry(l.commit) {
                let c = self.commit(&l.commit)?;
                e.insert(BlameCommit {
                    author: c.author.name.clone(),
                    time: c.author.time,
                    summary: c.summary().to_string(),
                });
            }
        }
        Ok(Some(BlameResponse {
            rev: *rev,
            path: crate::changes::normalize_path(path),
            lines,
            commits,
            exact: true,
        }))
    }
}

/// Render changes as a unified diff.
pub fn unified_patch(store: &ObjectStore, changes: &[Change]) -> Result<String> {
    let mut out = String::new();
    for c in changes {
        let read = |v: &Option<crate::changes::FileVersion>| -> Result<Vec<u8>> {
            match v {
                Some(v) if v.mode != MODE_GITLINK => store.read_blob(&v.oid),
                _ => Ok(Vec::new()),
            }
        };
        let (old, new) = (read(&c.old)?, read(&c.new)?);
        out.push_str(&format!("diff --git a/{0} b/{0}\n", c.path));
        match c.status {
            Status::Added => out.push_str(&format!("new file mode {:06o}\n", c.new.unwrap().mode)),
            Status::Deleted => {
                out.push_str(&format!("deleted file mode {:06o}\n", c.old.unwrap().mode))
            }
            _ => {
                let (o, n) = (c.old.unwrap(), c.new.unwrap());
                if o.mode != n.mode {
                    out.push_str(&format!(
                        "old mode {:06o}\nnew mode {:06o}\n",
                        o.mode, n.mode
                    ));
                }
            }
        }
        if old.contains(&0) || new.contains(&0) {
            out.push_str(&format!("Binary files a/{0} and b/{0} differ\n", c.path));
            continue;
        }
        let (old, new) = (String::from_utf8_lossy(&old), String::from_utf8_lossy(&new));
        let a = if c.status == Status::Added {
            "/dev/null".to_string()
        } else {
            format!("a/{}", c.path)
        };
        let b = if c.status == Status::Deleted {
            "/dev/null".to_string()
        } else {
            format!("b/{}", c.path)
        };
        let diff = TextDiff::from_lines(old.as_ref(), new.as_ref());
        let text = diff
            .unified_diff()
            .context_radius(3)
            .header(&a, &b)
            .to_string();
        out.push_str(&text);
    }
    Ok(out)
}
