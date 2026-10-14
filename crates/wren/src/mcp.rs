//! `wren mcp`: expose the query API to AI agents as Model Context Protocol
//! tools over stdio (newline-delimited JSON-RPC 2.0). An agent can read files,
//! history, blame and diffs of any revision without a local checkout, and
//! commit, land and revert on the server without one either.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use wren_core::api::*;
use wren_core::write::{Conflict, Landed};

use crate::client::{
    ChangesetRequest, Client, Content, Edit, FileChange, LandOnto, LandRequest, Person,
    RevertRequest,
};

/// Author of commits made through MCP when `WREN_AUTHOR` is not set.
const DEFAULT_AUTHOR: &str = "Wren MCP <wren-mcp@users.noreply.invalid>";

const PROTOCOL_VERSION: &str = "2025-06-18";

/// Default cap on a tool's text output (about 10k tokens). Agents can raise
/// it per call with `max_chars`, or narrow the request instead.
const DEFAULT_MAX_CHARS: usize = 40_000;
/// Default number of lines returned by wren_read_file and wren_blame.
const DEFAULT_LINES: usize = 400;

fn tools() -> Value {
    let rev = json!({"type": "string", "description": "Revision: branch, tag, commit id (may be abbreviated), with optional ~N / ^N suffixes. Default HEAD. It is resolved to a full commit id first, and every answer names the commit it describes."});
    let max_chars = json!({"type": "integer", "description": "Cap on the size of the output in characters (default 40000). Prefer narrowing the request."});
    let start =
        json!({"type": "integer", "description": "First line to return (1-based, default 1)."});
    let end = json!({"type": "integer", "description": "Last line to return (inclusive). Default: start + 399."});
    json!([
        {"name": "wren_log", "description": "First-parent commit history, newest first, optionally limited to commits that touched a file or directory. One line per commit.",
         "inputSchema": {"type": "object", "properties": {"rev": rev, "path": {"type": "string"}, "limit": {"type": "integer", "default": 20, "maximum": 500}}}},
        {"name": "wren_show", "description": "A commit's metadata, message and the files it changed relative to its first parent.",
         "inputSchema": {"type": "object", "properties": {"rev": rev, "max_chars": max_chars}}},
        {"name": "wren_diff", "description": "Unified diff between two revisions. base defaults to head's first parent. Use name_only or path to keep it small.",
         "inputSchema": {"type": "object", "properties": {"head": rev, "base": rev, "path": {"type": "string", "description": "Limit to a file or directory"}, "name_only": {"type": "boolean"}, "max_chars": max_chars}, "required": ["head"]}},
        {"name": "wren_blame", "description": "Which commit last changed each line of a file (first-parent history), for a range of lines.",
         "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}, "rev": rev, "start_line": start, "end_line": end, "max_chars": max_chars}, "required": ["path"]}},
        {"name": "wren_read_file", "description": "Contents of a file at a revision, with line numbers, for a range of lines.",
         "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}, "rev": rev, "start_line": start, "end_line": end, "max_chars": max_chars}, "required": ["path"]}},
        {"name": "wren_grep", "description": "Search file contents at a revision (like `git grep -n`). The pattern is a regular expression. Returns path:line:text.",
         "inputSchema": {"type": "object", "properties": {"pattern": {"type": "string"}, "path": {"type": "string", "description": "Limit to a file or directory"}, "rev": rev, "ignore_case": {"type": "boolean"}, "limit": {"type": "integer", "default": 50, "maximum": 1000}}, "required": ["pattern"]}},
        {"name": "wren_list_dir", "description": "Entries of a directory at a revision. Directories end with '/'.",
         "inputSchema": {"type": "object", "properties": {"path": {"type": "string", "default": ""}, "rev": rev}}},
        {"name": "wren_merge_base", "description": "Best common ancestor(s) of two revisions.",
         "inputSchema": {"type": "object", "properties": {"a": rev, "b": rev}, "required": ["a", "b"]}},
        {"name": "wren_is_ancestor", "description": "Whether revision a is an ancestor of (contained in) revision b.",
         "inputSchema": {"type": "object", "properties": {"a": rev, "b": rev}, "required": ["a", "b"]}},
        {"name": "wren_landed", "description": "The commit on a branch's first-parent history through which a given commit landed (e.g. the merge that brought it into main). Use this instead of scanning the log.",
         "inputSchema": {"type": "object", "properties": {"commit": rev, "into": rev}, "required": ["commit"]}},
        {"name": "wren_bisect_step", "description": "One step of bisection over bad's first-parent history. Returns the next commit to test, or the first bad commit when finished. Run your test on `next` (e.g. via wren_read_file or a checkout), then call again with it as the new good or bad.",
         "inputSchema": {"type": "object", "properties": {"good": rev, "bad": rev}, "required": ["good", "bad"]}},
        {"name": "wren_refs", "description": "Branches and tags with the commits they point to.",
         "inputSchema": {"type": "object", "properties": {"prefix": {"type": "string", "description": "Only refs whose name starts with this, e.g. refs/heads/ or refs/tags/v2."}, "limit": {"type": "integer", "default": 100}}}},
        {"name": "wren_commit", "description": "Create a commit on the server from whole-file contents, with no checkout. Give each changed file's complete new content (or delete: true). With onto, the commit is also landed on that branch in the same call (rebased onto its tip if the branch moved). On conflicts nothing lands: read the conflicted files at the reported branch tip with wren_read_file, redo the edit on top of them, and call wren_commit again.",
         "inputSchema": {"type": "object", "properties": {
            "message": {"type": "string", "description": "Commit message."},
            "changes": {"type": "array", "description": "Files to write or delete.", "items": {"type": "object", "properties": {
                "path": {"type": "string", "description": "Repository-relative path, e.g. src/main.rs."},
                "content": {"type": "string", "description": "The file's complete new content (UTF-8 text)."},
                "delete": {"type": "boolean", "description": "Delete the file instead."},
                "executable": {"type": "boolean", "description": "Set (true) or clear (false) the executable bit. Default: keep the file's current mode."}},
                "required": ["path"]}},
            "parent": {"type": "string", "description": "Revision to commit on top of. Default: the tip of onto if given, else HEAD."},
            "onto": {"type": "string", "description": "Branch to land the commit on, e.g. main. Omit to create the commit without moving any branch."},
            "max_chars": max_chars}, "required": ["message", "changes"]}},
        {"name": "wren_land", "description": "Land existing commits on a branch: rebase them onto its tip on the server and move the branch, retrying if it moved. On conflicts nothing lands: read the conflicted files at the reported branch tip with wren_read_file, then recreate the change on top of it with wren_commit (onto the branch).",
         "inputSchema": {"type": "object", "properties": {
            "changesets": {"type": "array", "items": {"type": "string"}, "description": "Commits to land, oldest first; each must be the parent of the next."},
            "onto": {"type": "string", "description": "Branch, e.g. main."},
            "max_chars": max_chars}, "required": ["changesets", "onto"]}},
        {"name": "wren_revert", "description": "Create a commit that undoes a (non-merge) commit, and land it on onto if given. On conflicts nothing lands: the reverted lines changed since, so read the files at the reported branch tip with wren_read_file and undo the change by hand with wren_commit.",
         "inputSchema": {"type": "object", "properties": {
            "commit": rev,
            "onto": {"type": "string", "description": "Branch to land the revert on, e.g. main. Omit to only create the revert commit."},
            "max_chars": max_chars}, "required": ["commit"]}}
    ])
}

/// The author for commits made by this MCP server.
fn author() -> Result<Person> {
    let raw = std::env::var("WREN_AUTHOR").unwrap_or_else(|_| DEFAULT_AUTHOR.to_string());
    Person::parse(&raw).context("WREN_AUTHOR")
}

/// Turn wren_commit's arguments into a changeset request.
fn commit_request(args: &Value, author: Person) -> Result<ChangesetRequest> {
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .filter(|m| !m.trim().is_empty())
        .context("message is required")?;
    let changes = args
        .get("changes")
        .and_then(|v| v.as_array())
        .context("changes must be an array of {path, content} or {path, delete: true}")?;
    let mut out = Vec::with_capacity(changes.len());
    for ch in changes {
        let path = ch
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|p| !p.is_empty())
            .context("every change needs a path")?;
        let content = ch.get("content").and_then(|v| v.as_str());
        let delete = ch.get("delete").and_then(|v| v.as_bool()) == Some(true);
        let edit = match (content, delete) {
            (Some(_), true) => bail!("{path}: give either content or delete: true, not both"),
            (Some(c), false) => Edit::Write(Content::Text {
                content: c.to_string(),
            }),
            (None, true) => Edit::Delete { delete: true },
            (None, false) => bail!("{path}: give the file's full content, or delete: true"),
        };
        let mode = match ch.get("executable").and_then(|v| v.as_bool()) {
            Some(_) if delete => None,
            Some(true) => Some("100755".to_string()),
            Some(false) => Some("100644".to_string()),
            None => None,
        };
        out.push(FileChange {
            path: path.to_string(),
            edit,
            mode,
        });
    }
    let onto = args.get("onto").and_then(|v| v.as_str());
    let parent = args
        .get("parent")
        .and_then(|v| v.as_str())
        .or(onto)
        .unwrap_or("HEAD");
    Ok(ChangesetRequest {
        parents: vec![parent.to_string()],
        author,
        committer: None,
        message: message.to_string(),
        changes: out,
        land: onto.map(|o| LandOnto {
            onto: o.to_string(),
        }),
    })
}

fn conflict_list(conflicts: &[Conflict]) -> String {
    conflicts
        .iter()
        .map(|c| {
            let detail = c
                .detail
                .as_deref()
                .map(|d| format!(": {d}"))
                .unwrap_or_default();
            format!(
                "- {} ({}, in {}){detail}",
                c.path,
                c.kind,
                c.changeset.short()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Describe a land result for an agent. `what` names the commits ("commit
/// abc..."), `redo` says how to try again after conflicts. Returns the text
/// and whether the tool failed (nothing landed).
fn describe_land(l: &Landed, what: &str, onto: &str, redo: &str) -> (String, bool) {
    match l {
        Landed::Landed {
            name,
            old,
            new,
            dropped,
            changesets,
            ..
        } => {
            let mut s = if old == new {
                format!("{what}: {name} already contained these changes; it stays at {new}.")
            } else {
                format!("Landed {what} on {name}: {old} -> {new}.")
            };
            if !dropped.is_empty() {
                s.push_str(&format!(
                    "\nDropped (the branch already had their changes): {}",
                    dropped.iter().map(|o| o.to_hex()).collect::<Vec<_>>().join(", ")
                ));
            }
            if changesets.len() > 1 {
                s.push_str(&format!(
                    "\nCommits now on {name}, oldest first: {}",
                    changesets.iter().map(|o| o.to_hex()).collect::<Vec<_>>().join(", ")
                ));
            }
            (s, false)
        }
        // The instructions come before the list, so capping never cuts them.
        Landed::Conflicts {
            onto: tip,
            conflicts,
        } => (
            format!(
                "NOT landed: {what} conflicts with the tip of {onto} ({tip}). Nothing changed on {onto}.\nNext: read each conflicted file at rev \"{tip}\" with wren_read_file, then {redo}\n{} conflicts:\n{}",
                conflicts.len(),
                conflict_list(conflicts)
            ),
            true,
        ),
        Landed::Rejected { reason } => (
            format!(
                "NOT landed: the git host rejected updating {onto}: {reason}\nThe branch is probably protected or a hook refused the push. Do not retry unchanged; tell the user."
            ),
            true,
        ),
    }
}

/// Run a write tool against the control-plane server. Returns the text and
/// whether it failed (conflicts or a rejected land).
async fn write_tool(w: &Client, name: &str, args: &Value) -> Result<(String, bool)> {
    match name {
        "wren_commit" => {
            let req = commit_request(args, author()?)?;
            let parent = req.parents[0].clone();
            let r = w.create_changeset(&req).await?;
            let what = format!("commit {}", r.oid);
            Ok(match (&r.land, &req.land) {
                (Some(l), Some(LandOnto { onto })) => {
                    let redo = format!("redo your edit on top of that content and call wren_commit again with onto \"{onto}\" (the parent defaults to the branch tip).");
                    let (mut text, failed) = describe_land(l, &what, onto, &redo);
                    if failed {
                        text = format!("Created {what} on {parent}.\n{text}");
                    }
                    (text, failed)
                }
                _ => (
                    format!(
                        "Created {what} (tree {}) on top of {parent}. No branch moved; land it with wren_land (changesets [\"{}\"], onto the branch).",
                        r.tree.map(|t| t.to_hex()).unwrap_or_else(|| "?".into()),
                        r.oid
                    ),
                    false,
                ),
            })
        }
        "wren_land" => {
            let changesets: Vec<String> = args
                .get("changesets")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if changesets.is_empty() {
                bail!("changesets must list at least one commit");
            }
            let onto = s(args, "onto", "");
            if onto.is_empty() {
                bail!("onto (a branch) is required");
            }
            let l = w
                .land(&LandRequest {
                    changesets: changesets.clone(),
                    onto: onto.to_string(),
                    committer: None,
                })
                .await?;
            let what = if changesets.len() == 1 {
                format!("commit {}", changesets[0])
            } else {
                format!("{} commits", changesets.len())
            };
            let redo =
                format!("recreate the change on top of it with wren_commit (onto \"{onto}\").");
            Ok(describe_land(&l, &what, onto, &redo))
        }
        "wren_revert" => {
            let commit = s(args, "commit", "");
            if commit.is_empty() {
                bail!("commit is required");
            }
            let onto = args.get("onto").and_then(|v| v.as_str());
            let r = w
                .revert(&RevertRequest {
                    commit: commit.to_string(),
                    author: author()?,
                    committer: None,
                    onto: onto.map(String::from),
                })
                .await?;
            let what = format!("revert commit {}", r.oid);
            Ok(match (&r.land, onto) {
                (Some(l), Some(onto)) => {
                    let redo = format!("undo the change by hand on top of it with wren_commit (onto \"{onto}\").");
                    let (mut text, failed) = describe_land(l, &what, onto, &redo);
                    if failed {
                        text = format!("Created {what} (reverting {commit}).\n{text}");
                    }
                    (text, failed)
                }
                _ => (
                    format!("Created {what} (reverting {commit}). No branch moved; land it with wren_land (changesets [\"{}\"], onto the branch).", r.oid),
                    false,
                ),
            })
        }
        _ => bail!("unknown tool {name}"),
    }
}

fn s<'a>(args: &'a Value, k: &str, default: &'a str) -> &'a str {
    args.get(k).and_then(|v| v.as_str()).unwrap_or(default)
}

fn n(args: &Value, k: &str) -> Option<usize> {
    args.get(k).and_then(|v| v.as_u64()).map(|v| v as usize)
}

/// Resolve a revision to a full commit id. Answers to queries that name
/// only full ids are immutable, so every cache tier can serve them; asking
/// with "HEAD" or a branch name would go to the authoritative server.
async fn pin(c: &Client, rev: &str) -> Result<String> {
    if rev.len() == 40 && rev.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Ok(rev.to_string());
    }
    let r: Resolved = c.get("resolve", &[("rev", rev)]).await?;
    Ok(r.oid.to_hex())
}

/// The 1-based inclusive line range an agent asked for, clamped to `total`.
fn line_range(args: &Value, total: usize) -> (usize, usize) {
    let start = n(args, "start_line").unwrap_or(1).max(1);
    let end = n(args, "end_line")
        .unwrap_or(start + DEFAULT_LINES - 1)
        .min(total);
    (start, end.max(start.saturating_sub(1)))
}

/// Cut `text` to the caller's budget and say so, so the agent knows to
/// narrow the request rather than assume it saw everything.
fn cap(text: String, args: &Value) -> String {
    let max = n(args, "max_chars").unwrap_or(DEFAULT_MAX_CHARS).max(1000);
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let cut = text[..cut].rfind('\n').map(|i| i + 1).unwrap_or(cut);
    format!(
        "{}[truncated: showing {} of {} characters; narrow the request (path, start_line/end_line, name_only) or raise max_chars]",
        &text[..cut],
        cut,
        text.len()
    )
}

/// Run a tool. Reads go to `c` (which may be a cache), writes to `w` (the
/// control-plane server). Returns the capped text and whether the tool
/// failed in a way the agent must act on (conflicts, a rejected land).
async fn call(c: &Client, w: &Client, name: &str, args: &Value) -> Result<(String, bool)> {
    let (text, failed) = match name {
        "wren_commit" | "wren_land" | "wren_revert" => write_tool(w, name, args).await?,
        _ => (read_tool(c, name, args).await?, false),
    };
    Ok((cap(text, args), failed))
}

async fn read_tool(c: &Client, name: &str, args: &Value) -> Result<String> {
    Ok(match name {
        "wren_log" => {
            let rev = pin(c, s(args, "rev", "HEAD")).await?;
            let limit = n(args, "limit").unwrap_or(20).min(500).to_string();
            let mut q = vec![("rev", rev.as_str()), ("limit", limit.as_str())];
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                q.push(("path", p));
            }
            let r: LogResponse = c.get("log", &q).await?;
            r.commits
                .iter()
                .map(|ci| {
                    format!(
                        "{} {} <{}> {}",
                        ci.oid,
                        ci.author.name,
                        ci.author.email,
                        ci.message.lines().next().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "wren_show" => {
            let rev = pin(c, s(args, "rev", "HEAD")).await?;
            let ci: CommitInfo = c
                .get(&format!("commits/{rev}"), &[("changes", "true")])
                .await?;
            serde_json::to_string_pretty(&ci)?
        }
        "wren_diff" => {
            let head = pin(c, s(args, "head", "HEAD")).await?;
            let base = match args.get("base").and_then(|v| v.as_str()) {
                Some(b) => Some(pin(c, b).await?),
                None => None,
            };
            let mut q = vec![("head", head.as_str())];
            if let Some(b) = &base {
                q.push(("base", b.as_str()));
            }
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                q.push(("path", p));
            }
            let name_only = args
                .get("name_only")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !name_only {
                q.push(("patch", "true"));
            }
            let d: DiffResponse = c.get("diff", &q).await?;
            let body = if name_only {
                d.changes
                    .iter()
                    .map(|ch| format!("{}\t{}", ch.status.letter(), ch.path))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                d.patch.unwrap_or_default()
            };
            format!("# diff {}..{}\n{body}", d.base, d.head)
        }
        "wren_blame" => {
            let rev = pin(c, s(args, "rev", "HEAD")).await?;
            let path = s(args, "path", "");
            let b: BlameResponse = c
                .get("blame", &[("rev", rev.as_str()), ("path", path)])
                .await?;
            let total = b.lines.len();
            let (start, end) = line_range(args, total);
            let shown = b.lines.get(start - 1..end).unwrap_or(&[]);
            let used: std::collections::BTreeSet<_> = shown.iter().map(|l| l.commit).collect();
            let mut out = vec![format!(
                "# blame {path} @ {rev} (lines {start}-{end} of {total})"
            )];
            out.extend(
                shown
                    .iter()
                    .map(|l| format!("{} {:>5}| {}", l.commit.short(), l.line, l.content)),
            );
            out.extend(
                b.commits
                    .iter()
                    .filter(|(o, _)| used.contains(*o))
                    .map(|(o, ci)| format!("# {} {}: {}", o.short(), ci.author, ci.summary)),
            );
            out.join("\n")
        }
        "wren_read_file" => {
            let rev = pin(c, s(args, "rev", "HEAD")).await?;
            let path = s(args, "path", "");
            let data = c
                .get_bytes("file", &[("rev", rev.as_str()), ("path", path)])
                .await?;
            if data[..data.len().min(8000)].contains(&0) {
                format!("# {path} @ {rev} is a binary file ({} bytes)", data.len())
            } else {
                let text = String::from_utf8_lossy(&data);
                let lines: Vec<&str> = text.lines().collect();
                let total = lines.len();
                let (start, end) = line_range(args, total);
                let mut out = vec![format!("# {path} @ {rev} (lines {start}-{end} of {total})")];
                out.extend(
                    lines
                        .get(start - 1..end)
                        .unwrap_or(&[])
                        .iter()
                        .enumerate()
                        .map(|(i, l)| format!("{:>6}\t{l}", start + i)),
                );
                out.join("\n")
            }
        }
        "wren_grep" => {
            let rev = pin(c, s(args, "rev", "HEAD")).await?;
            let limit = n(args, "limit").unwrap_or(50).min(1000).to_string();
            let mut q = vec![
                ("pattern", s(args, "pattern", "")),
                ("rev", rev.as_str()),
                ("limit", limit.as_str()),
            ];
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                q.push(("path", p));
            }
            if args.get("ignore_case").and_then(|v| v.as_bool()) == Some(true) {
                q.push(("ignore_case", "true"));
            }
            let r: wren_core::grep::GrepResponse = c.get("grep", &q).await?;
            let mut out = vec![format!("# grep @ {rev}: {} matches", r.matches.len())];
            out.extend(
                r.matches
                    .iter()
                    .map(|m| format!("{}:{}:{}", m.path, m.line, m.text)),
            );
            if r.truncated {
                out.push("[more matches exist; narrow path/pattern or raise limit]".into());
            }
            out.join("\n")
        }
        "wren_list_dir" => {
            let rev = pin(c, s(args, "rev", "HEAD")).await?;
            let t: TreeResponse = c
                .get(
                    "tree",
                    &[("rev", rev.as_str()), ("path", s(args, "path", ""))],
                )
                .await?;
            t.entries
                .iter()
                .map(|e| {
                    if e.mode == "040000" {
                        format!("{}/", e.name)
                    } else {
                        e.name.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "wren_merge_base" => {
            let (a, b) = (
                pin(c, s(args, "a", "")).await?,
                pin(c, s(args, "b", "")).await?,
            );
            let r: MergeBaseResponse = c
                .get("merge-base", &[("a", a.as_str()), ("b", b.as_str())])
                .await?;
            r.bases
                .iter()
                .map(|o| o.to_hex())
                .collect::<Vec<_>>()
                .join("\n")
        }
        "wren_is_ancestor" => {
            let (a, b) = (
                pin(c, s(args, "a", "")).await?,
                pin(c, s(args, "b", "")).await?,
            );
            let r: IsAncestorResponse = c
                .get("is-ancestor", &[("a", a.as_str()), ("b", b.as_str())])
                .await?;
            r.result.to_string()
        }
        "wren_landed" => {
            let commit = pin(c, s(args, "commit", "")).await?;
            let into = pin(c, s(args, "into", "HEAD")).await?;
            let r: LandedResponse = c
                .get(
                    "landed",
                    &[("commit", commit.as_str()), ("ref", into.as_str())],
                )
                .await?;
            r.landed
                .map(|o| o.to_hex())
                .unwrap_or_else(|| format!("not contained in {into}"))
        }
        "wren_bisect_step" => {
            let good = pin(c, s(args, "good", "")).await?;
            let bad = pin(c, s(args, "bad", "")).await?;
            let r: BisectStep = c
                .get("bisect", &[("good", good.as_str()), ("bad", bad.as_str())])
                .await?;
            match (r.next, r.first_bad) {
                (Some(n), _) => format!(
                    "test next: {n} ({} candidates left, at most {} more steps)",
                    r.candidates, r.steps_left
                ),
                (None, Some(b)) => format!("first bad commit: {b}"),
                _ => "bisect finished without a result".into(),
            }
        }
        "wren_refs" => {
            let refs: Vec<RefInfo> = c.get("refs", &[]).await?;
            let prefix = s(args, "prefix", "");
            let limit = n(args, "limit").unwrap_or(100);
            let matching: Vec<&RefInfo> =
                refs.iter().filter(|r| r.name.starts_with(prefix)).collect();
            let mut out: Vec<String> = matching
                .iter()
                .take(limit)
                .map(|r| format!("{} {}", r.commit, r.name))
                .collect();
            if matching.len() > limit {
                out.push(format!(
                    "[{} more refs; use prefix (e.g. refs/heads/) or raise limit]",
                    matching.len() - limit
                ));
            }
            out.join("\n")
        }
        _ => bail!("unknown tool {name}"),
    })
}

/// Serve MCP over stdio. `c` answers queries; `w` takes writes (commit,
/// land, revert), so it must be the control-plane server, not a cache.
pub async fn serve(c: Client, w: Client) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}});
                out.write_all(format!("{err}\n").as_bytes()).await?;
                continue;
            }
        };
        let Some(id) = msg.get("id").cloned() else {
            continue;
        }; // notification
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL_VERSION),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "wren", "version": env!("CARGO_PKG_VERSION")},
                "instructions": format!("Source control for repository '{}' served by Wren at {}: queries, and commits made on the server (wren_commit, wren_land, wren_revert). No checkout needed.", c.repo, c.server),
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(match call(&c, &w, name, &args).await {
                    Ok((text, false)) => json!({"content": [{"type": "text", "text": text}]}),
                    Ok((text, true)) => {
                        json!({"content": [{"type": "text", "text": text}], "isError": true})
                    }
                    Err(e) => {
                        json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true})
                    }
                })
            }
            _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
        };
        let resp = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        };
        out.write_all(format!("{resp}\n").as_bytes()).await?;
        out.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wren_core::changes::FileVersion;
    use wren_core::Oid;

    fn oid(c: char) -> Oid {
        c.to_string().repeat(40).parse().unwrap()
    }

    fn me() -> Person {
        Person::parse("T <t@x>").unwrap()
    }

    #[test]
    fn write_tools_are_listed() {
        let tools = tools();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for n in ["wren_commit", "wren_land", "wren_revert"] {
            assert!(names.contains(&n), "{names:?}");
        }
        let commit = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "wren_commit")
            .unwrap();
        assert_eq!(
            commit["inputSchema"]["required"],
            json!(["message", "changes"])
        );
        assert!(commit["inputSchema"]["properties"]["max_chars"].is_object());
    }

    #[test]
    fn default_author_parses() {
        let p = Person::parse(DEFAULT_AUTHOR).unwrap();
        assert_eq!(p.email, "wren-mcp@users.noreply.invalid");
    }

    #[test]
    fn commit_request_from_args() {
        let args = json!({
            "message": "fix",
            "onto": "main",
            "changes": [
                {"path": "a.rs", "content": "fn a() {}\n"},
                {"path": "run.sh", "content": "#!/bin/sh\n", "executable": true},
                {"path": "plain", "content": "x", "executable": false},
                {"path": "old.txt", "delete": true, "executable": true},
            ],
        });
        let req = commit_request(&args, me()).unwrap();
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({
                "parents": ["main"],
                "author": {"name": "T", "email": "t@x"},
                "message": "fix",
                "changes": [
                    {"path": "a.rs", "content": "fn a() {}\n"},
                    {"path": "run.sh", "content": "#!/bin/sh\n", "mode": "100755"},
                    {"path": "plain", "content": "x", "mode": "100644"},
                    {"path": "old.txt", "delete": true},
                ],
                "land": {"onto": "main"},
            })
        );
        // An explicit parent wins over onto; without either, HEAD.
        let req = commit_request(
            &json!({"message": "m", "parent": "abc123", "onto": "main", "changes": []}),
            me(),
        )
        .unwrap();
        assert_eq!(req.parents, ["abc123"]);
        let req = commit_request(&json!({"message": "m", "changes": []}), me()).unwrap();
        assert_eq!(req.parents, ["HEAD"]);
        assert!(req.land.is_none());
    }

    #[test]
    fn commit_request_rejects_bad_changes() {
        for (args, want) in [
            (json!({"changes": []}), "message"),
            (json!({"message": "m"}), "changes"),
            (
                json!({"message": "m", "changes": [{"content": "x"}]}),
                "path",
            ),
            (
                json!({"message": "m", "changes": [{"path": "a"}]}),
                "full content",
            ),
            (
                json!({"message": "m", "changes": [{"path": "a", "content": "x", "delete": true}]}),
                "not both",
            ),
        ] {
            let e = commit_request(&args, me()).unwrap_err().to_string();
            assert!(e.contains(want), "{args}: {e}");
        }
    }

    #[test]
    fn describes_land_outcomes() {
        let landed = Landed::Landed {
            name: "refs/heads/main".into(),
            old: oid('1'),
            new: oid('2'),
            changesets: vec![oid('2')],
            dropped: vec![],
            attempts: 1,
        };
        let (text, failed) = describe_land(&landed, "commit x", "main", "redo");
        assert!(!failed);
        assert!(
            text.contains(&format!("{} -> {}", oid('1'), oid('2'))),
            "{text}"
        );

        let v = |c| {
            Some(FileVersion {
                mode: 0o100644,
                oid: oid(c),
            })
        };
        let conflicts = Landed::Conflicts {
            onto: oid('9'),
            conflicts: vec![
                Conflict {
                    changeset: oid('a'),
                    path: "src/lib.rs".into(),
                    kind: "content".into(),
                    base: v('3'),
                    ours: v('4'),
                    theirs: v('5'),
                    detail: None,
                },
                Conflict {
                    changeset: oid('a'),
                    path: "gone.txt".into(),
                    kind: "modify/delete".into(),
                    base: v('3'),
                    ours: None,
                    theirs: v('5'),
                    detail: Some("deleted on the branch".into()),
                },
            ],
        };
        let (text, failed) = describe_land(
            &conflicts,
            "commit x",
            "main",
            "call wren_commit again with onto \"main\".",
        );
        assert!(failed);
        for want in [
            "NOT landed",
            "- src/lib.rs (content, in aaaaaaaa",
            "- gone.txt (modify/delete, in aaaaaaaa",
            ": deleted on the branch",
            "wren_read_file, then call wren_commit again",
            &format!("rev \"{}\"", oid('9')),
        ] {
            assert!(text.contains(want), "missing {want:?} in {text}");
        }

        let (text, failed) = describe_land(
            &Landed::Rejected {
                reason: "protected".into(),
            },
            "commit x",
            "main",
            "",
        );
        assert!(failed && text.contains("protected"), "{text}");
    }

    #[test]
    fn long_conflict_lists_are_capped() {
        let conflicts: Vec<Conflict> = (0..5000)
            .map(|i| Conflict {
                changeset: oid('a'),
                path: format!("dir/file{i}.txt"),
                kind: "content".into(),
                base: None,
                ours: None,
                theirs: None,
                detail: None,
            })
            .collect();
        let (text, _) = describe_land(
            &Landed::Conflicts {
                onto: oid('9'),
                conflicts,
            },
            "commit x",
            "main",
            "call wren_commit again.",
        );
        let capped = cap(text, &json!({"max_chars": 2000}));
        assert!(capped.len() < 2300, "{}", capped.len());
        assert!(capped.contains("[truncated"), "{capped}");
        assert!(capped.contains("5000 conflicts:"), "{capped}");
        assert!(
            capped.contains("then call wren_commit again."),
            "next steps survive the cap: {capped}"
        );
    }
}
