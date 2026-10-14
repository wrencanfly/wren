mod cache;
mod checkout;
mod client;
mod mcp;
mod server;

use std::collections::BTreeMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use wren_core::api::*;
use wren_core::{ObjectStore, Repo};

use crate::client::{find_checkout_config, Client};

/// Wren: agent-scale source control on top of git.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone)]
struct Target {
    /// Wren server (or cache) URL. Defaults to the current checkout's server.
    #[arg(long, env = "WREN_SERVER", global = true)]
    server: Option<String>,
    /// Bearer token for servers started with --token.
    #[arg(long, env = "WREN_TOKEN", global = true, hide_env_values = true)]
    token: Option<String>,
    /// Repository name. Defaults to the current checkout's repo.
    #[arg(long, env = "WREN_REPO", global = true)]
    repo: Option<String>,
    /// Print raw JSON.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the authoritative server.
    Server {
        /// Data directory (object store + per-repo metadata).
        #[arg(long, default_value = "wren-data")]
        data: PathBuf,
        #[arg(long, default_value = "127.0.0.1:7070")]
        listen: SocketAddr,
        /// Serve a repository mirrored from a git URL: NAME=URL (repeatable).
        #[arg(long = "repo", value_name = "NAME=URL")]
        repos: Vec<String>,
        /// Seconds between upstream syncs (0 = only on startup / on demand).
        #[arg(long, default_value_t = 30)]
        poll: u64,
        /// Require `Authorization: Bearer <token>` on every request. Without
        /// it the server is open to anyone who can reach it.
        #[arg(long, env = "WREN_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// Run a caching tier in front of a server or another cache.
    Cache {
        #[arg(long)]
        upstream: String,
        #[arg(long, default_value = "wren-cache")]
        dir: PathBuf,
        #[arg(long, default_value = "127.0.0.1:7071")]
        listen: SocketAddr,
        /// Maximum concurrent requests to the upstream tier.
        #[arg(long, default_value_t = 32)]
        upstream_concurrency: usize,
        /// Seconds before an upstream request is abandoned.
        #[arg(long, default_value_t = 120)]
        upstream_timeout: u64,
        /// Token clients of this cache must present.
        #[arg(long, env = "WREN_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Token this cache presents to its upstream.
        #[arg(long, env = "WREN_UPSTREAM_TOKEN", hide_env_values = true)]
        upstream_token: Option<String>,
    },
    /// Materialize a commit on disk (optionally sparse, optionally as a git repo).
    Checkout {
        dest: PathBuf,
        #[arg(long, default_value = "HEAD")]
        rev: String,
        /// Directory to include (cone mode, repeatable). Default: everything.
        #[arg(long = "sparse", value_name = "DIR")]
        sparse: Vec<String>,
        /// Also create a working `.git` (index, sparse config, promisor remote).
        #[arg(long)]
        git: bool,
        /// With --git: write a full index instead of a sparse index. Fetches
        /// every tree (never blobs) outside the sparse cone.
        #[arg(long, requires = "git")]
        full_index: bool,
        /// Node-wide object store shared by all checkouts on this machine.
        #[arg(long, env = "WREN_STORE")]
        store: Option<PathBuf>,
        /// Git remote URL to configure (default: the server's upstream).
        #[arg(long)]
        origin: Option<String>,
        #[command(flatten)]
        target: Target,
    },
    /// First-parent history, optionally limited to a path.
    Log {
        #[arg(default_value = "HEAD")]
        rev: String,
        #[arg(last = true)]
        path: Option<String>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        #[command(flatten)]
        target: Target,
    },
    /// Show a commit and the files it changed.
    Show {
        #[arg(default_value = "HEAD")]
        rev: String,
        #[command(flatten)]
        target: Target,
    },
    /// Line-by-line attribution (first-parent).
    Blame {
        path: String,
        #[arg(long, default_value = "HEAD")]
        rev: String,
        #[command(flatten)]
        target: Target,
    },
    /// Changes between two revisions (default base: head's first parent).
    Diff {
        head: String,
        base: Option<String>,
        /// Show only file names and status.
        #[arg(long)]
        name_status: bool,
        #[arg(long)]
        path: Option<String>,
        #[command(flatten)]
        target: Target,
    },
    /// Best common ancestor(s) of two revisions.
    MergeBase {
        a: String,
        b: String,
        #[command(flatten)]
        target: Target,
    },
    /// Find the first bad commit on BAD's first-parent history by running a
    /// command in fresh (sparse) checkouts. Exit 0 = good, anything else = bad.
    Bisect {
        #[arg(long)]
        good: String,
        #[arg(long)]
        bad: String,
        /// Only check out these directories for each test run.
        #[arg(long = "sparse", value_name = "DIR")]
        sparse: Vec<String>,
        #[arg(long, env = "WREN_STORE")]
        store: Option<PathBuf>,
        /// Test command, run in the checkout root.
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
        #[command(flatten)]
        target: Target,
    },
    /// Exit 0 if A is an ancestor of B, 1 otherwise.
    IsAncestor {
        a: String,
        b: String,
        #[command(flatten)]
        target: Target,
    },
    /// The commit through which a change landed on a branch.
    Landed {
        commit: String,
        #[arg(long = "into", default_value = "HEAD")]
        into: String,
        #[command(flatten)]
        target: Target,
    },
    /// Search file contents at a revision without checking anything out
    /// (like `git grep -n`). The pattern is a Rust regex.
    Grep {
        pattern: String,
        /// Limit the search to a file or directory.
        #[arg(default_value = "")]
        path: String,
        #[arg(long, default_value = "HEAD")]
        rev: String,
        #[arg(short = 'i', long)]
        ignore_case: bool,
        /// Maximum number of matching lines.
        #[arg(short = 'm', long, default_value_t = 100)]
        limit: usize,
        #[command(flatten)]
        target: Target,
    },
    /// Print a file at a revision without checking anything out.
    Cat {
        path: String,
        #[arg(long, default_value = "HEAD")]
        rev: String,
        #[command(flatten)]
        target: Target,
    },
    /// List a directory at a revision.
    Ls {
        #[arg(default_value = "")]
        path: String,
        #[arg(long, default_value = "HEAD")]
        rev: String,
        #[command(flatten)]
        target: Target,
    },
    /// List branches and tags.
    Refs {
        #[command(flatten)]
        target: Target,
    },
    /// List repositories on the server.
    Repos {
        #[command(flatten)]
        target: Target,
    },
    /// Ask the server to fetch from upstream now.
    Sync {
        #[command(flatten)]
        target: Target,
    },
    /// Serve the query API to an AI agent as an MCP server over stdio.
    Mcp {
        #[command(flatten)]
        target: Target,
    },
}

fn client(t: &Target) -> Result<Client> {
    let cfg = find_checkout_config();
    let server = t
        .server
        .clone()
        .or_else(|| cfg.as_ref().map(|c| c.1.server.clone()))
        .unwrap_or_else(|| "http://127.0.0.1:7070".to_string());
    let repo = t
        .repo
        .clone()
        .or_else(|| cfg.as_ref().map(|c| c.1.repo.clone()));
    Ok(Client::new(
        &server,
        &repo.unwrap_or_default(),
        t.token.as_deref(),
    ))
}

async fn need_repo(t: &Target) -> Result<Client> {
    let mut c = client(t)?;
    if c.repo.is_empty() {
        let repos = c.repos().await?;
        match repos.as_slice() {
            [only] => c.repo = only.name.clone(),
            _ => bail!(
                "pass --repo (or set WREN_REPO); server has: {}",
                repos
                    .iter()
                    .map(|r| r.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    Ok(c)
}

fn print_json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Days-since-epoch to civil date (Howard Hinnant's algorithm).
fn date(ts: i64) -> String {
    let z = ts.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn data_layout(data: &std::path::Path) -> (PathBuf, PathBuf) {
    (data.join("objects"), data.join("repos"))
}

async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.cmd {
        Cmd::Server {
            data,
            listen,
            repos,
            poll,
            token,
        } => {
            let (objects, repos_dir) = data_layout(&data);
            let store = Arc::new(ObjectStore::open(objects)?);
            let mut map: BTreeMap<String, Arc<Repo>> = BTreeMap::new();
            for spec in &repos {
                let (name, url) = spec.split_once('=').context("--repo expects NAME=URL")?;
                // Local paths are made absolute so the mirror can find them.
                let url = match std::fs::canonicalize(url) {
                    Ok(p) if !url.contains("://") => p.display().to_string(),
                    _ => url.to_string(),
                };
                map.insert(
                    name.into(),
                    Arc::new(Repo::open(
                        repos_dir.join(name),
                        name,
                        store.clone(),
                        Some(&url),
                    )?),
                );
            }
            // Repos registered by earlier runs.
            if let Ok(rd) = std::fs::read_dir(&repos_dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if !map.contains_key(&name) && e.path().join("meta.sqlite").exists() {
                        map.insert(
                            name.clone(),
                            Arc::new(Repo::open(e.path(), &name, store.clone(), None)?),
                        );
                    }
                }
            }
            if token.is_none() && !listen.ip().is_loopback() {
                tracing::warn!("listening on {listen} without --token: every mirrored repository is readable by anyone who can reach this port");
            }
            let state = Arc::new(server::ServerState::new(store, map, token));
            server::spawn_pollers(state.clone(), (poll > 0).then(|| Duration::from_secs(poll)));
            let listener = tokio::net::TcpListener::bind(listen).await?;
            tracing::info!(
                "wren server listening on http://{listen} ({} repos)",
                state.repos.len()
            );
            axum::serve(listener, server::router(state)).await?;
        }
        Cmd::Cache {
            upstream,
            dir,
            listen,
            upstream_concurrency,
            upstream_timeout,
            token,
            upstream_token,
        } => {
            let state = Arc::new(cache::CacheState {
                store: Arc::new(ObjectStore::open(dir.join("objects"))?),
                api_dir: dir.join("api"),
                upstream: upstream.trim_end_matches('/').to_string(),
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(upstream_timeout))
                    .build()?,
                token,
                upstream_token,
                upstream_permits: tokio::sync::Semaphore::new(upstream_concurrency),
                stats: Default::default(),
                semantics: Default::default(),
            });
            state.discover_semantics().await;
            let listener = tokio::net::TcpListener::bind(listen).await?;
            tracing::info!(
                "wren cache listening on http://{listen}, upstream {}",
                state.upstream
            );
            axum::serve(listener, cache::router(state)).await?;
        }
        Cmd::Checkout {
            dest,
            rev,
            sparse,
            git,
            full_index,
            store,
            origin,
            target,
        } => {
            let c = need_repo(&target).await?;
            let store_dir = match store {
                Some(s) => s,
                None => default_store()?,
            };
            let rep = checkout::checkout(
                &c,
                checkout::Options {
                    rev,
                    dest: dest.clone(),
                    sparse,
                    git,
                    full_index,
                    store_dir,
                    origin,
                },
            )
            .await?;
            eprintln!(
                "checked out {} into {}: {} files, {} trees walked, fetched {} objects ({:.1} MiB) in {} requests, {} ms",
                rep.commit.short(),
                dest.display(),
                rep.files,
                rep.trees_walked,
                rep.objects_fetched,
                rep.bytes_fetched as f64 / (1 << 20) as f64,
                rep.requests,
                rep.millis
            );
        }
        Cmd::Log {
            rev,
            path,
            limit,
            target,
        } => {
            let c = need_repo(&target).await?;
            let limit_s = limit.to_string();
            let mut q = vec![("rev", rev.as_str()), ("limit", limit_s.as_str())];
            if let Some(p) = &path {
                q.push(("path", p));
            }
            let r: LogResponse = c.get("log", &q).await?;
            if target.json {
                return print_json(&r).map(|_| ExitCode::SUCCESS);
            }
            for ci in r.commits {
                let first = ci.message.lines().next().unwrap_or("");
                println!(
                    "{} {} {:<16} {}",
                    ci.oid.short(),
                    date(ci.author.time),
                    truncate(&ci.author.name, 16),
                    first
                );
            }
        }
        Cmd::Show { rev, target } => {
            let c = need_repo(&target).await?;
            let ci: CommitInfo = c
                .get(&format!("commits/{rev}"), &[("changes", "true")])
                .await?;
            if target.json {
                return print_json(&ci).map(|_| ExitCode::SUCCESS);
            }
            println!("commit {}", ci.oid);
            for p in &ci.parents {
                println!("parent {p}");
            }
            println!(
                "Author: {} <{}>  {}",
                ci.author.name,
                ci.author.email,
                date(ci.author.time)
            );
            println!();
            for l in ci.message.lines() {
                println!("    {l}");
            }
            println!();
            for ch in ci.changes.unwrap_or_default() {
                println!("{}\t{}", ch.status.letter(), ch.path);
            }
        }
        Cmd::Blame { path, rev, target } => {
            let c = need_repo(&target).await?;
            let b: BlameResponse = c.get("blame", &[("rev", &rev), ("path", &path)]).await?;
            if target.json {
                return print_json(&b).map(|_| ExitCode::SUCCESS);
            }
            let width = b.lines.len().to_string().len();
            let mut out = std::io::stdout().lock();
            for l in &b.lines {
                let info = &b.commits[&l.commit];
                writeln!(
                    out,
                    "{} ({:<14} {} {:>width$}) {}",
                    &l.commit.to_hex()[..8],
                    truncate(&info.author, 14),
                    date(info.time),
                    l.line,
                    l.content
                )?;
            }
        }
        Cmd::Diff {
            head,
            base,
            name_status,
            path,
            target,
        } => {
            let c = need_repo(&target).await?;
            let mut q = vec![("head", head.as_str())];
            if let Some(b) = &base {
                q.push(("base", b));
            }
            if let Some(p) = &path {
                q.push(("path", p));
            }
            if !name_status {
                q.push(("patch", "true"));
            }
            let d: DiffResponse = c.get("diff", &q).await?;
            if target.json {
                return print_json(&d).map(|_| ExitCode::SUCCESS);
            }
            if name_status {
                for ch in d.changes {
                    println!("{}\t{}", ch.status.letter(), ch.path);
                }
            } else {
                print!("{}", d.patch.unwrap_or_default());
            }
        }
        Cmd::MergeBase { a, b, target } => {
            let c = need_repo(&target).await?;
            let r: MergeBaseResponse = c.get("merge-base", &[("a", &a), ("b", &b)]).await?;
            if target.json {
                return print_json(&r).map(|_| ExitCode::SUCCESS);
            }
            if r.bases.is_empty() {
                return Ok(ExitCode::FAILURE);
            }
            r.bases.iter().for_each(|o| println!("{o}"));
        }
        Cmd::Bisect {
            good,
            bad,
            sparse,
            store,
            cmd,
            target,
        } => {
            let c = need_repo(&target).await?;
            let store_dir = match store {
                Some(s) => s,
                None => default_store()?,
            };
            let (mut good, mut bad) = (c.resolve(&good).await?, c.resolve(&bad).await?);
            let work = std::env::temp_dir().join(format!("wren-bisect-{}", std::process::id()));
            loop {
                let (g, b) = (good.to_hex(), bad.to_hex());
                let step: BisectStep = c.get("bisect", &[("good", &g), ("bad", &b)]).await?;
                let Some(next) = step.next else {
                    let first = step.first_bad.context("bisect finished without a result")?;
                    let ci: CommitInfo = c.get(&format!("commits/{first}"), &[]).await?;
                    println!("{first} is the first bad commit");
                    println!("    {}", ci.message.lines().next().unwrap_or(""));
                    break;
                };
                eprintln!(
                    "bisect: testing {} ({} candidates left, ~{} steps)",
                    next.short(),
                    step.candidates,
                    step.steps_left
                );
                let dir = work.join(next.to_hex());
                let opts = checkout::Options {
                    rev: next.to_hex(),
                    dest: dir.clone(),
                    sparse: sparse.clone(),
                    git: false,
                    full_index: false,
                    store_dir: store_dir.clone(),
                    origin: None,
                };
                checkout::checkout(&c, opts).await?;
                let status = std::process::Command::new(&cmd[0])
                    .args(&cmd[1..])
                    .current_dir(&dir)
                    .status()?;
                let _ = std::fs::remove_dir_all(&dir);
                if status.success() {
                    good = next;
                } else {
                    bad = next;
                }
            }
            let _ = std::fs::remove_dir_all(&work);
        }
        Cmd::IsAncestor { a, b, target } => {
            let c = need_repo(&target).await?;
            let r: IsAncestorResponse = c.get("is-ancestor", &[("a", &a), ("b", &b)]).await?;
            if target.json {
                print_json(&r)?;
            }
            return Ok(if r.result {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
        Cmd::Landed {
            commit,
            into,
            target,
        } => {
            let c = need_repo(&target).await?;
            let r: LandedResponse = c
                .get("landed", &[("commit", &commit), ("ref", &into)])
                .await?;
            if target.json {
                return print_json(&r).map(|_| ExitCode::SUCCESS);
            }
            match r.landed {
                Some(o) => println!("{o}"),
                None => {
                    eprintln!("{commit} is not contained in {into}");
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        Cmd::Grep {
            pattern,
            path,
            rev,
            ignore_case,
            limit,
            target,
        } => {
            let c = need_repo(&target).await?;
            let limit = limit.to_string();
            let mut q = vec![
                ("pattern", pattern.as_str()),
                ("rev", rev.as_str()),
                ("limit", limit.as_str()),
            ];
            if !path.is_empty() {
                q.push(("path", path.as_str()));
            }
            if ignore_case {
                q.push(("ignore_case", "true"));
            }
            let r: wren_core::grep::GrepResponse = c.get("grep", &q).await?;
            if target.json {
                return print_json(&r).map(|_| ExitCode::SUCCESS);
            }
            for m in &r.matches {
                println!("{}:{}:{}", m.path, m.line, m.text);
            }
            if r.truncated {
                eprintln!(
                    "(stopped after {} matches; raise --limit for more)",
                    r.matches.len()
                );
            }
            if r.matches.is_empty() {
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::Cat { path, rev, target } => {
            let c = need_repo(&target).await?;
            let data = c
                .get_bytes("file", &[("rev", &rev), ("path", &path)])
                .await?;
            std::io::stdout().write_all(&data)?;
        }
        Cmd::Ls { path, rev, target } => {
            let c = need_repo(&target).await?;
            let t: TreeResponse = c.get("tree", &[("rev", &rev), ("path", &path)]).await?;
            if target.json {
                return print_json(&t).map(|_| ExitCode::SUCCESS);
            }
            for e in t.entries {
                let kind = serde_json::to_value(e.kind)?
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                println!("{} {:<6} {}\t{}", e.mode, kind, e.oid, e.name);
            }
        }
        Cmd::Refs { target } => {
            let c = need_repo(&target).await?;
            let refs: Vec<RefInfo> = c.get("refs", &[]).await?;
            if target.json {
                return print_json(&refs).map(|_| ExitCode::SUCCESS);
            }
            for r in refs {
                println!("{} {}", r.commit, r.name);
            }
        }
        Cmd::Repos { target } => {
            let c = client(&target)?;
            let repos = c.repos().await?;
            if target.json {
                return print_json(&repos).map(|_| ExitCode::SUCCESS);
            }
            for r in repos {
                println!(
                    "{:<20} {:>8} changesets  {}",
                    r.name,
                    r.changesets,
                    r.upstream.unwrap_or_default()
                );
            }
        }
        Cmd::Sync { target } => {
            let c = need_repo(&target).await?;
            let r: wren_core::repo::SyncReport = c.post("sync").await?;
            print_json(&r)?;
        }
        Cmd::Mcp { target } => {
            let c = need_repo(&target).await?;
            mcp::serve(c).await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn default_store() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set; pass --store")?;
    Ok(PathBuf::from(home).join(".cache/wren/objects"))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).chain(['…']).collect()
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("WREN_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wren: {e:#}");
            ExitCode::from(2)
        }
    }
}
