//! `wren mcp`: expose the query API to AI agents as Model Context Protocol
//! tools over stdio (newline-delimited JSON-RPC 2.0). An agent can read files,
//! history, blame and diffs of any revision without a local checkout.

use anyhow::Result;
use wren_core::api::*;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::client::Client;

const PROTOCOL_VERSION: &str = "2025-06-18";

fn tools() -> Value {
    let rev = json!({"type": "string", "description": "Revision: branch, tag, commit id (may be abbreviated), with optional ~N / ^N suffixes. Default HEAD."});
    json!([
        {"name": "wren_log", "description": "First-parent commit history, newest first, optionally limited to commits that touched a file or directory.",
         "inputSchema": {"type": "object", "properties": {"rev": rev, "path": {"type": "string"}, "limit": {"type": "integer", "default": 20}}}},
        {"name": "wren_show", "description": "A commit's metadata, message and the files it changed relative to its first parent.",
         "inputSchema": {"type": "object", "properties": {"rev": rev}}},
        {"name": "wren_diff", "description": "Unified diff between two revisions. base defaults to head's first parent.",
         "inputSchema": {"type": "object", "properties": {"head": rev, "base": rev, "path": {"type": "string", "description": "Limit to a file or directory"}, "name_only": {"type": "boolean"}}, "required": ["head"]}},
        {"name": "wren_blame", "description": "Which commit last changed each line of a file (first-parent history).",
         "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}, "rev": rev}, "required": ["path"]}},
        {"name": "wren_read_file", "description": "Contents of a file at a revision.",
         "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}, "rev": rev}, "required": ["path"]}},
        {"name": "wren_list_dir", "description": "Entries of a directory at a revision.",
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
         "inputSchema": {"type": "object", "properties": {}}}
    ])
}

fn s<'a>(args: &'a Value, k: &str, default: &'a str) -> &'a str {
    args.get(k).and_then(|v| v.as_str()).unwrap_or(default)
}

async fn call(c: &Client, name: &str, args: &Value) -> Result<String> {
    Ok(match name {
        "wren_log" => {
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(20)
                .to_string();
            let mut q = vec![("rev", s(args, "rev", "HEAD")), ("limit", limit.as_str())];
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
            let ci: CommitInfo = c
                .get(
                    &format!("commits/{}", s(args, "rev", "HEAD")),
                    &[("changes", "true")],
                )
                .await?;
            serde_json::to_string_pretty(&ci)?
        }
        "wren_diff" => {
            let mut q = vec![("head", s(args, "head", "HEAD"))];
            if let Some(b) = args.get("base").and_then(|v| v.as_str()) {
                q.push(("base", b));
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
            if name_only {
                d.changes
                    .iter()
                    .map(|ch| format!("{}\t{}", ch.status.letter(), ch.path))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                d.patch.unwrap_or_default()
            }
        }
        "wren_blame" => {
            let b: BlameResponse = c
                .get(
                    "blame",
                    &[
                        ("rev", s(args, "rev", "HEAD")),
                        ("path", s(args, "path", "")),
                    ],
                )
                .await?;
            b.lines
                .iter()
                .map(|l| format!("{} {:>5}| {}", l.commit.short(), l.line, l.content))
                .chain(
                    b.commits
                        .iter()
                        .map(|(o, ci)| format!("# {} {}: {}", o.short(), ci.author, ci.summary)),
                )
                .collect::<Vec<_>>()
                .join("\n")
        }
        "wren_read_file" => {
            let data = c
                .get_bytes(
                    "file",
                    &[
                        ("rev", s(args, "rev", "HEAD")),
                        ("path", s(args, "path", "")),
                    ],
                )
                .await?;
            String::from_utf8_lossy(&data).into_owned()
        }
        "wren_list_dir" => {
            let t: TreeResponse = c
                .get(
                    "tree",
                    &[
                        ("rev", s(args, "rev", "HEAD")),
                        ("path", s(args, "path", "")),
                    ],
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
            let r: MergeBaseResponse = c
                .get(
                    "merge-base",
                    &[("a", s(args, "a", "")), ("b", s(args, "b", ""))],
                )
                .await?;
            r.bases
                .iter()
                .map(|o| o.to_hex())
                .collect::<Vec<_>>()
                .join("\n")
        }
        "wren_is_ancestor" => {
            let r: IsAncestorResponse = c
                .get(
                    "is-ancestor",
                    &[("a", s(args, "a", "")), ("b", s(args, "b", ""))],
                )
                .await?;
            r.result.to_string()
        }
        "wren_landed" => {
            let r: LandedResponse = c
                .get(
                    "landed",
                    &[
                        ("commit", s(args, "commit", "")),
                        ("ref", s(args, "into", "HEAD")),
                    ],
                )
                .await?;
            r.landed
                .map(|o| o.to_hex())
                .unwrap_or_else(|| "not contained".into())
        }
        "wren_bisect_step" => {
            let r: BisectStep = c
                .get(
                    "bisect",
                    &[("good", s(args, "good", "")), ("bad", s(args, "bad", ""))],
                )
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
            refs.iter()
                .map(|r| format!("{} {}", r.commit, r.name))
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => anyhow::bail!("unknown tool {name}"),
    })
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
