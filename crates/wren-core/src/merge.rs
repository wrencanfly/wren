//! Server-side merges: `git merge` of one commit into a branch, with no
//! work tree.
//!
//! The tree merge is three-way over the merge base, path by path, with the
//! same rules as rebase (`Repo::merge_path`: git's own `git merge-file` for
//! files changed on both sides). Only the paths the incoming side changed
//! are looked at, so the cost is the size of that side's change.
//!
//! Where git's merge (`ort`) would do something path-by-path merging cannot,
//! the merge is refused as a conflict instead of giving a different answer:
//!
//! * **renames** (detected as git does, `renames::detect`): a file renamed on
//!   one side and changed on the other (git merges the change into the new
//!   name), a rename onto a path the other side added, and a rename out of a
//!   directory that is gone on that side while the other side added files
//!   to it (git's directory rename detection);
//! * **criss-cross histories**: two merge bases are merged first into a
//!   virtual base, as git does; if that merge has conflicts (git would keep
//!   conflict markers in the virtual base), or there are more than two
//!   merge bases, the merge is refused.
//!
//! Conflicts are reported, never written: nothing changes until the merge
//! is clean. The differential test (`tests/merge.rs`) checks the trees
//! against `git merge-tree --write-tree`.

use std::collections::HashSet;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::changes::{self, Change, FileVersion, Status};
use crate::oid::Oid;
use crate::renames::{self, CopyKind};
use crate::repo::Repo;
use crate::write::{Conflict, FileEdit, NewChangeset, PathMerge, RefUpdate, LAND_ATTEMPTS};
use crate::{bad_request, BadRequest};

/// How a tree merge came out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeMerge {
    /// The edits that turn the first side's tree into the merge.
    Clean(Vec<FileEdit>),
    Conflicts(Vec<Conflict>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Merged {
    Merged {
        #[serde(rename = "ref")]
        name: String,
        old: Oid,
        new: Oid,
        /// The branch moved to the merged commit itself (it already
        /// contained the branch tip, and fast-forwards were allowed).
        fast_forward: bool,
        attempts: u32,
    },
    /// The branch already contains the commit; nothing changed.
    UpToDate {
        #[serde(rename = "ref")]
        name: String,
        tip: Oid,
    },
    Conflicts {
        onto: Oid,
        conflicts: Vec<Conflict>,
    },
    Rejected {
        reason: String,
    },
}

/// A merge into a branch.
#[derive(Clone, Debug)]
pub struct MergeRequest {
    /// The branch (`main` or `refs/heads/main`).
    pub into: String,
    /// The commit to merge.
    pub commit: Oid,
    pub author: Vec<u8>,
    /// Defaults to the author.
    pub committer: Option<Vec<u8>>,
    /// Defaults to `Merge commit '<id>' into <branch>`.
    pub message: Option<Vec<u8>>,
    /// Move the branch to `commit` when it already contains the branch tip,
    /// instead of creating a merge commit.
    pub fast_forward: bool,
}

/// Depth of virtual merge bases (criss-cross within criss-cross).
const MAX_VIRTUAL_DEPTH: u32 = 4;

/// The directories above `p`, nearest first, without the root.
fn ancestors(p: &str) -> impl Iterator<Item = &str> {
    let mut cur = p;
    std::iter::from_fn(move || {
        let i = cur.rfind('/')?;
        cur = &cur[..i];
        Some(cur)
    })
}

impl Repo {
    /// Merge commit `theirs` into commit `ours` (trees only; no commit is
    /// made).
    pub fn merge_commits(&self, ours: &Oid, theirs: &Oid) -> Result<TreeMerge> {
        self.merge_commits_at(ours, theirs, 0)
    }

    fn merge_commits_at(&self, ours: &Oid, theirs: &Oid, depth: u32) -> Result<TreeMerge> {
        let base = match self.base_tree(ours, theirs, depth)? {
            Ok(t) => t,
            Err(conflicts) => return Ok(TreeMerge::Conflicts(conflicts)),
        };
        self.merge_trees(base, self.tree_of(ours)?, self.tree_of(theirs)?, theirs)
    }

    /// The tree to merge `a` and `b` over: their merge base's, or for two
    /// merge bases, the (clean) merge of those.
    fn base_tree(&self, a: &Oid, b: &Oid, depth: u32) -> Result<Result<Oid, Vec<Conflict>>> {
        let bases = self.merge_bases(a, b)?;
        let refuse = |detail: String| {
            Ok(Err(vec![Conflict {
                changeset: *b,
                path: String::new(),
                kind: "criss-cross".into(),
                base: None,
                ours: None,
                theirs: None,
                detail: Some(detail),
            }]))
        };
        match bases.as_slice() {
            [] => Err(bad_request(format!(
                "{a} and {b} have no common history; refusing to merge unrelated histories"
            ))),
            [one] => Ok(Ok(self.tree_of(one)?)),
            [x, y] if depth < MAX_VIRTUAL_DEPTH => match self.merge_commits_at(x, y, depth + 1)? {
                TreeMerge::Clean(edits) => {
                    let tree = match self.apply_edits(self.tree_of(x)?, &edits) {
                        Ok((_, tree)) => tree,
                        Err(e) if e.downcast_ref::<BadRequest>().is_some() => {
                            return refuse(format!(
                                "the merge bases {x} and {y} do not merge: {e:#}"
                            ))
                        }
                        Err(e) => return Err(e),
                    };
                    Ok(Ok(tree))
                }
                TreeMerge::Conflicts(c) => refuse(format!(
                    "the merge bases {x} and {y} conflict (at {:?}); git would merge over a \
                     virtual base with conflict markers",
                    c[0].path
                )),
            },
            _ => refuse(format!(
                "{} merge bases; only two are merged into a virtual base",
                bases.len()
            )),
        }
    }

    /// Three-way merge of tree `theirs` into tree `ours` over `base`.
    /// `theirs_commit` labels conflicts.
    pub fn merge_trees(
        &self,
        base: Oid,
        ours: Oid,
        theirs: Oid,
        theirs_commit: &Oid,
    ) -> Result<TreeMerge> {
        let ch_ours = changes::diff_trees(&self.store, Some(base), Some(ours))?;
        let ch_theirs = changes::diff_trees(&self.store, Some(base), Some(theirs))?;
        let conflict = |path: &str, kind: &str, detail: Option<String>| Conflict {
            changeset: *theirs_commit,
            path: path.to_string(),
            kind: kind.to_string(),
            base: None,
            ours: None,
            theirs: None,
            detail,
        };
        let mut conflicts = Vec::new();
        for (side, other, tip) in [(&ch_ours, &ch_theirs, ours), (&ch_theirs, &ch_ours, theirs)] {
            conflicts.extend(
                self.rename_conflicts(side, other, tip)?
                    .into_iter()
                    .map(|(path, detail)| conflict(&path, "rename", Some(detail))),
            );
        }
        if !conflicts.is_empty() {
            return Ok(TreeMerge::Conflicts(conflicts));
        }

        let mut edits = Vec::new();
        for c in &ch_theirs {
            let cur = changes::entry_at(&self.store, ours, &c.path)?;
            let outcome = self.merge_path(base, c, cur.as_ref())?;
            match outcome {
                PathMerge::Take(new) => edits.push(FileEdit {
                    path: c.path.clone(),
                    new,
                }),
                PathMerge::Keep => {}
                PathMerge::Conflict(kind) => conflicts.push(Conflict {
                    base: c.old,
                    ours: cur.filter(|e| !e.is_tree()).map(|e| FileVersion {
                        mode: e.mode,
                        oid: e.oid,
                    }),
                    theirs: c.new,
                    ..conflict(&c.path, kind, None)
                }),
            }
        }
        if !conflicts.is_empty() {
            return Ok(TreeMerge::Conflicts(conflicts));
        }
        // Edits that clash only as a whole (a file where the other side
        // made a directory) show up when the tree is built.
        if let Err(e) = self.apply_edits(ours, &edits) {
            if e.downcast_ref::<BadRequest>().is_none() {
                return Err(e);
            }
            return Ok(TreeMerge::Conflicts(vec![conflict(
                "",
                "file/directory",
                Some(format!("{e:#}")),
            )]));
        }
        Ok(TreeMerge::Clean(edits))
    }

    /// Renames on one side (`side`, ending at tree `tip`) that git's merge
    /// would carry across to the other side's changes (`other`): (path,
    /// why).
    fn rename_conflicts(
        &self,
        side: &[Change],
        other: &[Change],
        tip: Oid,
    ) -> Result<Vec<(String, String)>> {
        if !side.iter().any(|c| c.status == Status::Deleted) {
            return Ok(Vec::new());
        }
        let touched: HashSet<&str> = other.iter().map(|c| c.path.as_str()).collect();
        let mut out = Vec::new();
        for r in renames::detect(&self.store, side, false)? {
            if r.kind != CopyKind::Rename {
                continue;
            }
            if touched.contains(r.from.as_str()) {
                out.push((
                    r.from.clone(),
                    format!("renamed to {} on one side and changed on the other", r.to),
                ));
                continue;
            }
            if touched.contains(r.to.as_str()) {
                out.push((
                    r.to.clone(),
                    format!(
                        "{} was renamed here on one side and added on the other",
                        r.from
                    ),
                ));
                continue;
            }
            // Every directory the file left that is gone on this side
            // (git detects renames of any of them).
            for dir in ancestors(&r.from) {
                if r.to.starts_with(&format!("{dir}/")) {
                    break; // this one and those above it stay
                }
                if changes::entry_at(&self.store, tip, dir)?.is_some() {
                    continue;
                }
                let prefix = format!("{dir}/");
                if let Some(c) = other
                    .iter()
                    .find(|c| c.status == Status::Added && c.path.starts_with(&prefix))
                {
                    out.push((
                        c.path.clone(),
                        format!(
                            "{dir} was renamed (as {} was to {}) on one side and {} added on the other",
                            r.from, r.to, c.path
                        ),
                    ));
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// For a rebase (a cherry-pick of `changes`, made on `parent_tree` and
    /// giving `cs_tree`, onto `onto_tree`): the renames git would carry
    /// across, as for a merge. The change's own renames are always looked
    /// at (that costs the size of the change); the branch's, which need a
    /// diff from the change's parent to the branch, only where they could
    /// matter: a path both sides deleted (one may have renamed it), or a
    /// directory one side emptied while the other put files in it.
    pub(crate) fn rebase_rename_conflicts(
        &self,
        parent_tree: Oid,
        onto_tree: Oid,
        changes: &[Change],
        cs_tree: Oid,
    ) -> Result<Vec<(String, String)>> {
        let at = |t: Oid, p: &str| changes::entry_at(&self.store, t, p);
        let dir_of = |t: Oid, d: &str| -> Result<Option<Oid>> {
            Ok(at(t, d)?.filter(|e| e.is_tree()).map(|e| e.oid))
        };
        let mut out = Vec::new();
        // The change's renames: the branch changed or deleted the source.
        if changes.iter().any(|c| c.status == Status::Deleted) {
            for r in renames::detect(&self.store, changes, false)? {
                if r.kind != CopyKind::Rename {
                    continue;
                }
                let before = at(parent_tree, &r.from)?.map(|e| (e.mode, e.oid));
                let now = at(onto_tree, &r.from)?.map(|e| (e.mode, e.oid));
                if before != now {
                    out.push((
                        r.from.clone(),
                        format!("renamed to {} here and changed on the branch", r.to),
                    ));
                }
            }
        }
        let mut onto_side = false;
        let mut cs_side = false;
        for c in changes {
            if c.status == Status::Deleted && at(onto_tree, &c.path)?.is_none() {
                onto_side = true;
            }
            for d in ancestors(&c.path) {
                let before = dir_of(parent_tree, d)?;
                match c.status {
                    Status::Added if before.is_some() && dir_of(onto_tree, d)?.is_none() => {
                        onto_side = true
                    }
                    Status::Deleted if dir_of(cs_tree, d)?.is_none() => {
                        let now = dir_of(onto_tree, d)?;
                        if now.is_some() && now != before {
                            cs_side = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        if onto_side || cs_side {
            let onto_changes =
                changes::diff_trees(&self.store, Some(parent_tree), Some(onto_tree))?;
            if onto_side {
                out.extend(self.rename_conflicts(&onto_changes, changes, onto_tree)?);
            }
            if cs_side {
                out.extend(self.rename_conflicts(changes, &onto_changes, cs_tree)?);
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Merge `req.commit` into the branch `req.into` and move the branch,
    /// retrying when it moved in between (as `land` does).
    pub fn merge_into(&self, req: &MergeRequest) -> Result<Merged> {
        let name = if req.into.starts_with("refs/") {
            req.into.clone()
        } else {
            format!("refs/heads/{}", req.into)
        };
        if !self.meta.has_changeset(&req.commit)? {
            return Err(crate::not_found(format!("unknown commit {}", req.commit)));
        }
        for attempt in 1..=LAND_ATTEMPTS {
            let guard = self.lock_mirror()?;
            let tip = self
                .meta
                .get_ref(&name)?
                .ok_or_else(|| crate::not_found(format!("unknown branch {name}")))?
                .target;
            if self.is_ancestor(&req.commit, &tip)? {
                return Ok(Merged::UpToDate { name, tip });
            }
            let fast_forward = req.fast_forward && self.is_ancestor(&tip, &req.commit)?;
            let new = if fast_forward {
                req.commit
            } else {
                let edits = match self.merge_commits(&tip, &req.commit)? {
                    TreeMerge::Conflicts(conflicts) => {
                        return Ok(Merged::Conflicts {
                            onto: tip,
                            conflicts,
                        })
                    }
                    TreeMerge::Clean(edits) => edits,
                };
                let short = name.strip_prefix("refs/heads/").unwrap_or(&name);
                let message = req.message.clone().unwrap_or_else(|| {
                    format!("Merge commit '{}' into {short}\n", req.commit).into_bytes()
                });
                self.create_changeset(&NewChangeset {
                    parents: vec![tip, req.commit],
                    author: req.author.clone(),
                    committer: req.committer.clone(),
                    message,
                    edits,
                })?
            };
            let outcome = self.update_ref_locked(&name, Some(tip), Some(new), "merge")?;
            drop(guard);
            match outcome {
                RefUpdate::Updated => {
                    return Ok(Merged::Merged {
                        name,
                        old: tip,
                        new,
                        fast_forward,
                        attempts: attempt,
                    })
                }
                RefUpdate::Rejected { reason } => return Ok(Merged::Rejected { reason }),
                RefUpdate::Stale { .. } => {
                    if self.upstream.is_some() && !self.authoritative()? {
                        self.sync()?;
                    }
                }
            }
        }
        bail!("{name} kept moving; gave up after {LAND_ATTEMPTS} attempts")
    }
}
