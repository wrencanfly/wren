//! Server-side writes: every commit, rebase and revert happens on the server.
//!
//! * `create_changeset`: parents plus the files that changed. No tree, no
//!   checkout: the tree is derived (`treegen`), and the cost is proportional
//!   to the change, not the repository.
//! * `rebase`: replays a linear stack of changesets onto a new base, looking
//!   only at the paths each changeset touched. Files changed on both sides
//!   get git's own three-way content merge (`git merge-file`).
//! * `update_ref`: compare-and-swap. While a git host is the source of truth
//!   (write-proxy mode), the host is updated first with `--force-with-lease`,
//!   which is a compare-and-swap inside receive-pack, and Wren's ref follows.
//! * `land` = rebase onto the branch tip + `update_ref`, retried when the
//!   branch moved. There is no merge queue and no global lock: a lost race
//!   costs one more rebase of the stack. (Within one process, attempts on a
//!   repo take turns; see `land`.)
//! * `revert`: the inverse changeset, optionally landed.
//!
//! Once Wren is the source of truth (M5), `update_ref` moves Wren's ref
//! first and the host follows asynchronously (`export`). The host's branch
//! protections then no longer guard writes, so Wren has its own: protected
//! refs (no deletion, fast-forward only), a policy hook consulted before any
//! ref moves, and an audit log of every move. They apply in both modes and
//! to every path: the write API, `land`, and `git push`.
//!
//! Commits created here are ordinary git commits with git ids, so when the
//! host's history comes back through sync they are already known.

use std::io::Write as _;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::changes::{self, Change, FileVersion, Status};
use crate::git_source::{GitMirror, Push};
use crate::meta::ChangesetRow;
use crate::object::{Kind, RawCommit, MODE_BLOB, MODE_EXEC, MODE_GITLINK, MODE_SYMLINK};
use crate::oid::Oid;
use crate::repo::Repo;
use crate::{bad_request, BadRequest};

/// Rules every ref update must pass, in both modes and on every path.
#[derive(Clone, Default)]
pub struct Policy {
    /// Glob patterns (`*` matches anything) of protected refs: they cannot
    /// be deleted and move only forward.
    pub protected: Vec<String>,
    /// Asked before any ref moves; `Err(reason)` refuses.
    #[allow(clippy::type_complexity)]
    pub hook: Option<std::sync::Arc<dyn Fn(&RefChange) -> Result<(), String> + Send + Sync>>,
}

impl Policy {
    pub fn is_protected(&self, name: &str) -> bool {
        self.protected.iter().any(|p| glob(p, name))
    }
}

fn glob(pattern: &str, s: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == s,
        Some((pre, rest)) => {
            let Some(tail) = s.strip_prefix(pre) else {
                return false;
            };
            (0..=tail.len()).any(|i| tail.is_char_boundary(i) && glob(rest, &tail[i..]))
        }
    }
}

/// A proposed ref move, as the policy hook sees it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefChange {
    pub repo: String,
    #[serde(rename = "ref")]
    pub name: String,
    pub old: Option<Oid>,
    pub new: Option<Oid>,
    /// `refs`, `land` or `push`.
    pub via: String,
}

/// An audit log entry: one ref move.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEntry {
    pub time_ms: u64,
    #[serde(rename = "ref")]
    pub name: String,
    pub old: Option<Oid>,
    pub new: Option<Oid>,
    pub via: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExportReport {
    pub pushed: usize,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExportStatus {
    pub authoritative: bool,
    /// Refs whose current value has not reached the host yet.
    pub pending: usize,
    pub lag_ms_p99: Option<u64>,
    pub samples: usize,
    pub error: Option<String>,
}

/// One file edit in a new changeset: the path's new version, or `None` to
/// delete it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEdit {
    pub path: String,
    pub new: Option<FileVersion>,
}

/// What a client sends to create a changeset.
#[derive(Clone, Debug)]
pub struct NewChangeset {
    pub parents: Vec<Oid>,
    /// `Name <email> <unix time> <tz>`, see [`signature`].
    pub author: Vec<u8>,
    /// Defaults to the author.
    pub committer: Option<Vec<u8>>,
    pub message: Vec<u8>,
    pub edits: Vec<FileEdit>,
}

/// A git signature line value, validated.
pub fn signature(name: &str, email: &str, time: i64, tz: &str) -> Result<Vec<u8>> {
    if name.contains(['<', '>', '\n']) || email.contains(['<', '>', '\n']) {
        return Err(bad_request(
            "names and emails cannot contain '<', '>' or newlines",
        ));
    }
    let tz_ok = tz.len() == 5
        && matches!(tz.as_bytes()[0], b'+' | b'-')
        && tz[1..].bytes().all(|b| b.is_ascii_digit());
    if !tz_ok {
        return Err(bad_request(format!(
            "bad timezone {tz:?}, expected e.g. +0100"
        )));
    }
    Ok(format!("{name} <{email}> {time} {tz}").into_bytes())
}

/// Why a stack could not be replayed onto a new base.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    /// The changeset (from the stack) that could not be applied.
    pub changeset: Oid,
    pub path: String,
    /// `content`, `add/add`, `modify/delete`, `delete/modify`, `mode`, or
    /// `file/directory`.
    pub kind: String,
    /// The path in the changeset's original parent, in the new base, and in
    /// the changeset.
    pub base: Option<FileVersion>,
    pub ours: Option<FileVersion>,
    pub theirs: Option<FileVersion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rebased {
    Done {
        /// The new changesets, oldest first (the stack itself when it
        /// already sits on the base).
        changesets: Vec<Oid>,
        /// Stack entries whose changes the base already has; like git
        /// rebase, they are dropped.
        dropped: Vec<Oid>,
    },
    Conflicts(Vec<Conflict>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RefUpdate {
    Updated,
    /// The ref no longer had the expected value; nothing changed.
    Stale {
        current: Option<Oid>,
    },
    /// The git host refused the update (hook, branch protection, ...).
    Rejected {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Landed {
    Landed {
        #[serde(rename = "ref")]
        name: String,
        old: Oid,
        new: Oid,
        changesets: Vec<Oid>,
        dropped: Vec<Oid>,
        attempts: u32,
    },
    Conflicts {
        onto: Oid,
        conflicts: Vec<Conflict>,
    },
    Rejected {
        reason: String,
    },
}

/// How many times `land` re-rebases after losing a race for the ref.
const LAND_ATTEMPTS: u32 = 8;

fn is_regular(mode: u32) -> bool {
    mode & 0o170000 == 0o100000
}

/// Check a path supplied by a client: relative, normalized, and not inside
/// `.git`.
fn check_path(p: &str) -> Result<()> {
    let bad = p.is_empty()
        || p.contains('\0')
        || changes::normalize_path(p) != p
        || p.split('/')
            .any(|c| c == ".." || c.eq_ignore_ascii_case(".git"));
    if bad {
        return Err(bad_request(format!("invalid path {p:?}")));
    }
    Ok(())
}

impl Repo {
    /// Create a changeset from its parents and the files it changes relative
    /// to the first parent. Moves no ref. Returns the new commit id; creating
    /// the same changeset twice returns the same id.
    pub fn create_changeset(&self, cs: &NewChangeset) -> Result<Oid> {
        let Some(&p1) = cs.parents.first() else {
            return Err(bad_request("a changeset needs a parent"));
        };
        for p in &cs.parents {
            if !self.meta.has_changeset(p)? {
                return Err(crate::not_found(format!("unknown parent {p}")));
            }
        }
        let base = self.tree_of(&p1)?;
        let mut seen = std::collections::BTreeSet::new();
        let mut changes = Vec::new();
        for e in &cs.edits {
            check_path(&e.path)?;
            if !seen.insert(e.path.as_str()) {
                return Err(bad_request(format!("{} is edited twice", e.path)));
            }
            let old = match changes::entry_at(&self.store, base, &e.path)? {
                // Replacing a directory by a file: the files in it must be
                // deleted by other edits (checked against the tree below).
                Some(t) if t.is_tree() && e.new.is_some() => None,
                Some(t) if t.is_tree() => {
                    return Err(bad_request(format!(
                        "{} is a directory; delete the files in it",
                        e.path
                    )))
                }
                Some(t) => Some(FileVersion {
                    mode: t.mode,
                    oid: t.oid,
                }),
                None => None,
            };
            if let Some(n) = &e.new {
                if ![MODE_BLOB, MODE_EXEC, MODE_SYMLINK, MODE_GITLINK].contains(&n.mode) {
                    return Err(bad_request(format!("{}: bad mode {:o}", e.path, n.mode)));
                }
                if n.mode != MODE_GITLINK && !self.store.has(&n.oid) {
                    return Err(bad_request(format!(
                        "{}: blob {} has not been uploaded",
                        e.path, n.oid
                    )));
                }
            }
            let status = match (&old, &e.new) {
                (None, None) => {
                    return Err(bad_request(format!(
                        "{} is deleted, but it does not exist",
                        e.path
                    )))
                }
                (o, n) if o == n => continue,
                (None, Some(_)) => Status::Added,
                (Some(_), None) => Status::Deleted,
                (Some(o), Some(n)) if o.mode & 0o170000 == n.mode & 0o170000 => Status::Modified,
                (Some(_), Some(_)) => Status::TypeChanged,
            };
            changes.push(Change {
                path: e.path.clone(),
                status,
                old,
                new: e.new,
            });
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        let tree = crate::treegen::apply(&self.store, Some(base), &changes)
            .map_err(|e| bad_request(format!("{e:#}")))?;
        // Change records must be exactly what diffing the trees gives
        // (blame, log and diff rely on it). Cheap: both sides are small.
        let check = changes::diff_trees(&self.store, Some(base), Some(tree))?;
        if check != changes {
            return Err(bad_request(
                "the edits are inconsistent (for example, a file is added under a path \
                 that stays a file); delete conflicting paths explicitly",
            ));
        }

        let mut message = cs.message.clone();
        if !message.ends_with(b"\n") {
            message.push(b'\n');
        }
        let committer = cs.committer.clone().unwrap_or_else(|| cs.author.clone());
        let raw = RawCommit {
            tree,
            parents: cs.parents.clone(),
            author: cs.author.clone(),
            committer: committer.clone(),
            extra_headers: Vec::new(),
            message: message.clone(),
        }
        .encode();
        let commit_time = crate::object::Commit::parse(&raw)?.committer.time;
        // The commit object is derived data, but stored: pushing to the git
        // host and serving checkouts read it from the object store.
        let oid = self.store.put(Kind::Commit, &raw)?;
        // Every server must be able to read the new objects before the
        // changeset is visible to any of them.
        self.store.publish()?;
        if self.meta.has_changeset(&oid)? {
            return Ok(oid);
        }
        let row = ChangesetRow {
            seq: 0,
            oid,
            parents: cs.parents.clone(),
            author: cs.author.clone(),
            committer: Some(committer),
            extra_headers: Vec::new(),
            message,
            commit_time,
            changes: changes::encode_changes(&changes),
            tree_override: None,
            raw_commit: None,
        };
        self.insert_changeset(&row, &changes, tree)?;
        Ok(oid)
    }

    /// Replay `stack` (a linear chain, oldest first, each the first-parent
    /// child of the previous) onto `onto`. Only the paths each changeset
    /// touched are examined. Authors and messages are kept; the committer is
    /// kept too unless `committer` is given, so replaying the same stack onto
    /// the same base twice yields the same ids.
    pub fn rebase(&self, stack: &[Oid], onto: Oid, committer: Option<&[u8]>) -> Result<Rebased> {
        let mut rows = Vec::with_capacity(stack.len());
        for (i, oid) in stack.iter().enumerate() {
            let row = self
                .meta
                .changeset(oid)?
                .ok_or_else(|| crate::not_found(format!("unknown changeset {oid}")))?;
            if row.parents.len() != 1 {
                return Err(bad_request(format!(
                    "{oid} is a merge or a root commit; only linear changesets can be rebased"
                )));
            }
            if i > 0 && row.parents[0] != stack[i - 1] {
                return Err(bad_request(format!(
                    "{oid} is not a child of {}: the stack must be a chain",
                    stack[i - 1]
                )));
            }
            rows.push(row);
        }
        if rows.is_empty() {
            return Err(bad_request("nothing to rebase"));
        }
        if rows[0].parents[0] == onto && committer.is_none() {
            return Ok(Rebased::Done {
                changesets: stack.to_vec(),
                dropped: Vec::new(),
            });
        }
        let mut base = onto;
        let mut out = Vec::new();
        let mut dropped = Vec::new();
        for row in rows {
            let base_tree = self.tree_of(&base)?;
            let mut edits = Vec::new();
            let mut conflicts = Vec::new();
            for c in changes::decode_changes(&row.changes)? {
                let cur = changes::entry_at(&self.store, base_tree, &c.path)?;
                let conflict =
                    |kind: &str, ours: Option<FileVersion>, detail: Option<String>| Conflict {
                        changeset: row.oid,
                        path: c.path.clone(),
                        kind: kind.to_string(),
                        base: c.old,
                        ours,
                        theirs: c.new,
                        detail,
                    };
                if cur.as_ref().is_some_and(|e| e.is_tree()) {
                    conflicts.push(conflict("file/directory", None, None));
                    continue;
                }
                let cur = cur.map(|e| FileVersion {
                    mode: e.mode,
                    oid: e.oid,
                });
                if cur == c.old {
                    edits.push(FileEdit {
                        path: c.path.clone(),
                        new: c.new,
                    });
                    continue;
                }
                if cur == c.new {
                    continue; // the base already has this version
                }
                match (c.old, cur, c.new) {
                    (Some(o), Some(t), Some(n))
                        if is_regular(o.mode) && is_regular(t.mode) && is_regular(n.mode) =>
                    {
                        let mode = if t.mode == o.mode {
                            n.mode
                        } else if n.mode == o.mode || n.mode == t.mode {
                            t.mode
                        } else {
                            conflicts.push(conflict("mode", cur, None));
                            continue;
                        };
                        let oid = if o.oid == t.oid {
                            n.oid
                        } else if o.oid == n.oid {
                            t.oid
                        } else {
                            match self.merge_file(&o.oid, &t.oid, &n.oid)? {
                                Some(merged) => self.store.put(Kind::Blob, &merged)?,
                                None => {
                                    conflicts.push(conflict("content", cur, None));
                                    continue;
                                }
                            }
                        };
                        edits.push(FileEdit {
                            path: c.path.clone(),
                            new: Some(FileVersion { mode, oid }),
                        });
                    }
                    (None, Some(_), Some(_)) => conflicts.push(conflict("add/add", cur, None)),
                    (Some(_), Some(_), None) => {
                        conflicts.push(conflict("modify/delete", cur, None))
                    }
                    (Some(_), None, Some(_)) => {
                        conflicts.push(conflict("delete/modify", cur, None))
                    }
                    _ => conflicts.push(conflict("content", cur, None)),
                }
            }
            if !conflicts.is_empty() {
                return Ok(Rebased::Conflicts(conflicts));
            }
            if edits.is_empty() {
                dropped.push(row.oid);
                continue;
            }
            let new = NewChangeset {
                parents: vec![base],
                author: row.author.clone(),
                committer: committer
                    .map(|c| c.to_vec())
                    .or_else(|| row.committer.clone()),
                message: row.message.clone(),
                edits,
            };
            match self.create_changeset(&new) {
                Ok(oid) => {
                    out.push(oid);
                    base = oid;
                }
                // A file where the base now has a directory (or the reverse),
                // found while building the tree.
                Err(e) if e.downcast_ref::<BadRequest>().is_some() => {
                    return Ok(Rebased::Conflicts(vec![Conflict {
                        changeset: row.oid,
                        path: String::new(),
                        kind: "file/directory".into(),
                        base: None,
                        ours: None,
                        theirs: None,
                        detail: Some(format!("{e:#}")),
                    }]));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(Rebased::Done {
            changesets: out,
            dropped,
        })
    }

    /// Three-way merge of blob contents with git's own algorithm. `None`
    /// means a conflict (or a binary file git will not merge).
    fn merge_file(&self, base: &Oid, ours: &Oid, theirs: &Oid) -> Result<Option<Vec<u8>>> {
        let dir = tempfile::tempdir()?;
        let mut paths = Vec::new();
        for (name, oid) in [("ours", ours), ("base", base), ("theirs", theirs)] {
            let p = dir.path().join(name);
            std::fs::File::create(&p)?.write_all(&self.store.read_blob(oid)?)?;
            paths.push(p);
        }
        let out = Command::new("git")
            .args(["merge-file", "-p"])
            .args(&paths)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("running git merge-file")?;
        // Exit status: 0 clean, 1..=127 the number of conflicts, 255 error
        // (e.g. binary files).
        Ok(match out.status.code() {
            Some(0) => Some(out.stdout),
            _ => None,
        })
    }

    /// Compare-and-swap a branch to `new` (or delete it). With an upstream,
    /// the git host is updated first, conditionally on it still having
    /// `expected`; Wren's ref follows only once the host accepted.
    pub fn update_ref(
        &self,
        name: &str,
        expected: Option<Oid>,
        new: Option<Oid>,
    ) -> Result<RefUpdate> {
        self.update_ref_as(name, expected, new, "refs")
    }

    /// `update_ref`, recording `via` (the path the write came by: `refs`,
    /// `land`, `push`) in the audit log.
    pub fn update_ref_as(
        &self,
        name: &str,
        expected: Option<Oid>,
        new: Option<Oid>,
        via: &str,
    ) -> Result<RefUpdate> {
        let _guard = self.lock_mirror()?;
        self.update_ref_locked(name, expected, new, via)
    }

    /// `update_ref` with the mirror lock held by the caller. A push reads packs
    /// in the mirror that a concurrent sync's absorb would delete, and
    /// holding the lock across push and CAS means Wren's ref has caught up
    /// before the next local writer reads it.
    fn update_ref_locked(
        &self,
        name: &str,
        expected: Option<Oid>,
        new: Option<Oid>,
        via: &str,
    ) -> Result<RefUpdate> {
        let branch = name.starts_with("refs/heads/") && name.len() > "refs/heads/".len();
        let tag = name.starts_with("refs/tags/") && name.len() > "refs/tags/".len();
        if !branch && !tag {
            return Err(bad_request(format!(
                "only branches and tags can be updated, not {name:?}"
            )));
        }
        // (target, the commit it names): a branch names a changeset; a tag
        // a changeset or an annotated tag of one.
        let new = match new {
            None => None,
            Some(n) => Some((n, self.peel_to_changeset(&n, tag)?)),
        };
        let current = || -> Result<Option<Oid>> { Ok(self.meta.get_ref(name)?.map(|r| r.target)) };
        if let Some(reason) = self.check_policy(name, expected, new, via)? {
            return Ok(RefUpdate::Rejected { reason });
        }
        let authoritative = self.authoritative()?;
        if let (Some(url), false) = (&self.upstream, authoritative) {
            let mirror = GitMirror::open(self.dir().join("mirror.git"), url, self.store.root())?;
            match mirror.push_lease(name, expected, new.map(|n| n.0))? {
                Push::Done => {}
                Push::Stale => {
                    return Ok(RefUpdate::Stale {
                        current: current()?,
                    })
                }
                Push::Rejected(reason) => return Ok(RefUpdate::Rejected { reason }),
            }
        }
        if self.meta.cas_ref(name, expected, new)? {
            self.record_move(name, expected, new.map(|n| n.0), via, authoritative)?;
            return Ok(RefUpdate::Updated);
        }
        // A sync that ran after the push may already have published it.
        let cur = current()?;
        if cur == new.map(|n| n.0) {
            return Ok(RefUpdate::Updated);
        }
        Ok(RefUpdate::Stale { current: cur })
    }

    /// The changeset `oid` names: itself, or (for a tag ref) what an
    /// annotated tag points at.
    fn peel_to_changeset(&self, oid: &Oid, allow_tag: bool) -> Result<Oid> {
        let mut cur = *oid;
        for _ in 0..16 {
            if self.meta.has_changeset(&cur)? {
                return Ok(cur);
            }
            match self.store.get(&cur)? {
                Some((Kind::Tag, data)) if allow_tag => {
                    cur = crate::object::Tag::parse(&data)?.object
                }
                _ => return Err(crate::not_found(format!("unknown changeset {oid}"))),
            }
        }
        Err(bad_request(format!("{oid}: tag chain too long")))
    }

    /// Protected refs and the policy hook: `Some(reason)` refuses the move.
    fn check_policy(
        &self,
        name: &str,
        old: Option<Oid>,
        new: Option<(Oid, Oid)>,
        via: &str,
    ) -> Result<Option<String>> {
        let policy = self.policy.read().unwrap().clone();
        if policy.is_protected(name) {
            match (old, new) {
                (_, None) => return Ok(Some(format!("{name} is protected: it cannot be deleted"))),
                (Some(o), Some((_, peeled))) => {
                    let old_commit = self.peel_to_changeset(&o, true).unwrap_or(o);
                    if !self.is_ancestor(&old_commit, &peeled)? {
                        return Ok(Some(format!(
                            "{name} is protected: only fast-forward updates are allowed"
                        )));
                    }
                }
                (None, Some(_)) => {}
            }
        }
        if let Some(hook) = &policy.hook {
            let change = RefChange {
                repo: self.name.clone(),
                name: name.to_string(),
                old,
                new: new.map(|n| n.0),
                via: via.to_string(),
            };
            if let Err(reason) = hook(&change) {
                return Ok(Some(format!("policy: {reason}")));
            }
        }
        Ok(None)
    }

    /// The audit log entry for a move, and (when Wren is the source of
    /// truth) when it happened, for the exporter's lag.
    fn record_move(
        &self,
        name: &str,
        old: Option<Oid>,
        new: Option<Oid>,
        via: &str,
        authoritative: bool,
    ) -> Result<()> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let entry = AuditEntry {
            time_ms: now_ms,
            name: name.to_string(),
            old,
            new,
            via: via.to_string(),
        };
        self.meta
            .append_log("audit", &serde_json::to_vec(&entry)?)?;
        if authoritative {
            self.meta
                .set_kv(&format!("changed:{name}"), &now_ms.to_string())?;
        }
        Ok(())
    }

    /// The audit log: every ref move, oldest first, after `after`.
    pub fn audit(&self, after: i64, limit: usize) -> Result<Vec<(i64, AuditEntry)>> {
        self.meta
            .read_log("audit", after, limit)?
            .into_iter()
            .map(|(seq, d)| Ok((seq, serde_json::from_slice(&d)?)))
            .collect()
    }

    /// When Wren is the source of truth: bring the git host up to date with
    /// Wren's refs. Each ref is pushed with a lease on the value last
    /// exported, so a push that bypassed Wren is never overwritten; it is
    /// reported instead. Reconciling (rather than replaying a queue) makes
    /// this idempotent and crash-safe: intermediate values are skipped.
    pub fn export(&self) -> Result<ExportReport> {
        let mut report = ExportReport::default();
        let Some(url) = self.upstream.clone() else {
            return Ok(report);
        };
        if !self.authoritative()? {
            return Ok(report);
        }
        let _guard = self.lock_mirror()?;
        let mirror = GitMirror::open(self.dir().join("mirror.git"), &url, self.store.root())?;
        let refs = self.meta.refs()?;
        let mut names: Vec<(String, Option<Oid>)> = refs
            .iter()
            .map(|r| (r.name.clone(), Some(r.target)))
            .collect();
        // Exported refs that Wren no longer has: deletions.
        for k in self.meta.exported_names()? {
            if !refs.iter().any(|r| r.name == k) {
                names.push((k, None));
            }
        }
        let mut errors = Vec::new();
        for (name, want) in names {
            let key = format!("exported:{name}");
            let have: Option<Oid> = match self.meta.get_kv(&key)? {
                Some(s) if !s.is_empty() => Some(s.parse()?),
                _ => None,
            };
            if have == want {
                continue;
            }
            match mirror.push_lease(&name, have, want)? {
                Push::Done => {
                    self.meta
                        .set_kv(&key, &want.map(|w| w.to_hex()).unwrap_or_default())?;
                    report.pushed += 1;
                    if let Some(t) = self.meta.get_kv(&format!("changed:{name}"))? {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        let lag = now.saturating_sub(t.parse().unwrap_or(now));
                        let mut lags = self.export_lags.lock().unwrap();
                        lags.push_back(lag);
                        if lags.len() > 1000 {
                            lags.pop_front();
                        }
                    }
                }
                Push::Stale => errors.push(format!(
                    "{name}: the host no longer has what Wren exported; \
                     someone pushed to it directly (not overwritten)"
                )),
                Push::Rejected(reason) => errors.push(format!("{name}: host refused: {reason}")),
            }
        }
        self.meta.set_kv("export_error", &errors.join("; "))?;
        report.errors = errors;
        Ok(report)
    }

    /// Refs whose current value has not reached the host yet, and the
    /// 99th percentile of recent export lags (ms).
    pub fn export_status(&self) -> Result<ExportStatus> {
        let mut pending = 0;
        if self.authoritative()? {
            for r in self.meta.refs()? {
                if self
                    .meta
                    .get_kv(&format!("exported:{}", r.name))?
                    .as_deref()
                    != Some(r.target.to_hex().as_str())
                {
                    pending += 1;
                }
            }
        }
        let mut lags: Vec<u64> = self.export_lags.lock().unwrap().iter().copied().collect();
        lags.sort_unstable();
        let p99 = (!lags.is_empty()).then(|| lags[(lags.len() * 99).div_ceil(100) - 1]);
        Ok(ExportStatus {
            authoritative: self.authoritative()?,
            pending,
            lag_ms_p99: p99,
            samples: lags.len(),
            error: self.meta.get_kv("export_error")?.filter(|e| !e.is_empty()),
        })
    }

    /// Rebase `stack` onto the tip of branch `onto` and move the branch to
    /// the result, retrying when the branch moved in between.
    pub fn land(&self, stack: &[Oid], onto: &str, committer: Option<&[u8]>) -> Result<Landed> {
        let name = if onto.starts_with("refs/") {
            onto.to_string()
        } else {
            format!("refs/heads/{onto}")
        };
        for attempt in 1..=LAND_ATTEMPTS {
            // One attempt (read the tip, rebase, push, CAS) runs under the
            // mirror lock, so lands onto a branch take turns (on Postgres,
            // across servers too)
            // instead of invalidating each other's rebases in a retry storm.
            // This is only an optimization within one process: correctness
            // rests on the compare-and-swaps (the host's lease and Wren's
            // ref), which also catch writers this lock cannot see.
            let guard = self.lock_mirror()?;
            let r = self
                .meta
                .get_ref(&name)?
                .ok_or_else(|| crate::not_found(format!("unknown branch {name}")))?;
            let tip = r.target;
            let (changesets, dropped) = match self.rebase(stack, tip, committer)? {
                Rebased::Conflicts(conflicts) => {
                    return Ok(Landed::Conflicts {
                        onto: tip,
                        conflicts,
                    })
                }
                Rebased::Done {
                    changesets,
                    dropped,
                } => (changesets, dropped),
            };
            let new = changesets.last().copied().unwrap_or(tip);
            if new == tip {
                return Ok(Landed::Landed {
                    name,
                    old: tip,
                    new,
                    changesets,
                    dropped,
                    attempts: attempt,
                });
            }
            let outcome = self.update_ref_locked(&name, Some(tip), Some(new), "land")?;
            drop(guard);
            match outcome {
                RefUpdate::Updated => {
                    return Ok(Landed::Landed {
                        name,
                        old: tip,
                        new,
                        changesets,
                        dropped,
                        attempts: attempt,
                    })
                }
                RefUpdate::Rejected { reason } => return Ok(Landed::Rejected { reason }),
                RefUpdate::Stale { .. } => {
                    // Someone else landed first. Learn the new tip (from the
                    // host, when it is the source of truth) and replay.
                    if self.upstream.is_some() && !self.authoritative()? {
                        self.sync()?;
                    }
                }
            }
        }
        bail!("{name} kept moving; gave up after {LAND_ATTEMPTS} attempts")
    }

    /// The changeset that undoes `oid` (a non-merge), on top of `oid` itself.
    /// Land it with [`Repo::land`] to apply it to a branch.
    pub fn revert(&self, oid: &Oid, author: &[u8], committer: Option<&[u8]>) -> Result<Oid> {
        let row = self
            .meta
            .changeset(oid)?
            .ok_or_else(|| crate::not_found(format!("unknown changeset {oid}")))?;
        if row.parents.len() != 1 {
            return Err(bad_request(format!("{oid} is a merge or a root commit")));
        }
        let summary = self.commit(oid)?.summary().to_string();
        let edits = changes::decode_changes(&row.changes)?
            .into_iter()
            .map(|c| FileEdit {
                path: c.path,
                new: c.old,
            })
            .collect();
        self.create_changeset(&NewChangeset {
            parents: vec![*oid],
            author: author.to_vec(),
            committer: committer.map(|c| c.to_vec()),
            message: format!("Revert \"{summary}\"\n\nThis reverts commit {oid}.\n").into_bytes(),
            edits,
        })
    }
}
