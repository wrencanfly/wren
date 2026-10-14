//! `wren mcp`: expose the query API to AI agents as Model Context Protocol
//! tools over stdio (newline-delimited JSON-RPC 2.0). An agent can read files,
//! history, blame and diffs of any revision without a local checkout.

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use wren_core::api::*;

use crate::client::Client;

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
         "inputSchema": {"type": "object", "properties": {"prefix": {"type": "string", "description": "Only refs whose name starts with this, e.g. refs/heads/ or refs/tags/v2."}, "limit": {"type": "integer", "default": 100}}}}
    ])
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

async fn call(c: &Client, name: &str, args: &Value) -> Result<String> {
    let text = match name {
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
        _ => anyhow::bail!("unknown tool {name}"),
    };
    Ok(cap(text, args))
}

pub async fn serve(c: Client) -> Result<()> {
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
                "instructions": format!("Source control queries for repository '{}' served by Wren at {}. No checkout needed.", c.repo, c.server),
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(match call(&c, name, &args).await {
                    Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
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
