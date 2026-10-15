use wren_vcs::{cache, checkout, mcp, server};

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
use wren_core::write::Landed;
use wren_core::{ObjectStore, Repo};

use wren_vcs::client::{
    find_checkout_config, ChangesetRequest, Client, Content, Edit, FileChange, LandOnto,
    LandRequest, Person, RevertRequest,
};

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

/// Where writes go. Caches refuse writes, so this must be the control-plane
/// server even when queries go through a cache.
#[derive(Args, Clone)]
struct WriteTarget {
    /// Server that takes writes. Defaults to --server.
    #[arg(long, env = "WREN_WRITE_SERVER")]
    write_server: Option<String>,
    /// Bearer token for writes. Defaults to --token.
    #[arg(long, env = "WREN_WRITE_TOKEN", hide_env_values = true)]
    write_token: Option<String>,
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
        /// Enable the write API (changesets, land, refs, revert) for clients
        /// presenting `Authorization: Bearer <write-token>`. It also grants
        /// read access. Without it the server is read-only.
        #[arg(long, env = "WREN_WRITE_TOKEN", hide_env_values = true)]
        write_token: Option<String>,
        /// Keep metadata in Postgres instead of a SQLite file per repository,
        /// e.g. `postgres://wren@db/wren`. Several servers given the same
        /// database and the same --data directory serve the same
        /// repositories, and any of them can answer any request.
        #[arg(long, env = "WREN_META_URL", hide_env_values = true)]
        meta: Option<String>,
        /// Keep segments in a bucket shared by all servers, e.g.
        /// `s3://bucket/wren`, `gs://bucket/wren` or `file:///srv/segments`
        /// (credentials from the usual AWS_* / GOOGLE_* / AZURE_* variables).
        /// With --meta, servers then need no shared filesystem: each keeps
        /// its own --data directory as a cache.
        #[arg(long, env = "WREN_SEGMENTS_URL")]
        segments: Option<String>,
        /// Publish a snapshot of each repository's default branch this
        /// often (seconds; 0 = never). New node daemons seed from it.
        #[arg(long, default_value_t = 0)]
        snapshot_every: u64,
        /// Directory whose blobs snapshots include (repeatable; default:
        /// all). Every tree is always included.
        #[arg(long = "snapshot-cone", value_name = "DIR")]
        snapshot_cone: Vec<String>,
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
        /// This cache's tier. A region cache's upstream must be the server;
        /// a node cache's must be a region cache (prefer `wren daemon` for
        /// nodes).
        #[arg(long, default_value = "region", value_parser = ["region", "node"])]
        tier: String,
        /// Bytes of recently served objects to keep in memory (e.g. 256MiB).
        #[arg(long, default_value = "256MiB", value_parser = parse_size)]
        memory: u64,
        /// Start even if the upstream is not exactly the tier above (e.g. a
        /// region cache in front of another region cache, or a node
        /// pointing straight at the server in a small deployment).
        #[arg(long)]
        allow_tier_skip: bool,
    },
    /// Run the node tier: one per machine, serving the checkouts on it over
    /// a unix socket, with a bounded object store.
    Daemon {
        /// The node's object store; checkouts on this machine use it too
        /// (`wren checkout --store`, or WREN_STORE).
        #[arg(long, env = "WREN_STORE")]
        store: PathBuf,
        /// Unix socket to serve on (default: <store>/daemon.sock). Clients
        /// use it through WREN_OBJECTS=unix:<path>.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Also serve over TCP.
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// The region cache above this node.
        #[arg(long)]
        upstream: String,
        /// Store budget (e.g. 50GiB). Over it, the oldest generations of
        /// blobs are evicted; trees and leased checkouts are kept.
        #[arg(long, default_value = "20GiB", value_parser = parse_size)]
        budget: u64,
        /// Days a checkout's lease protects its objects.
        #[arg(long, default_value_t = 7)]
        lease_days: u64,
        /// Seed an empty store from this repository's snapshot.
        #[arg(long, value_name = "REPO")]
        snapshot: Option<String>,
        #[arg(long, default_value = "256MiB", value_parser = parse_size)]
        memory: u64,
        #[arg(long, default_value_t = 32)]
        upstream_concurrency: usize,
        #[arg(long, default_value_t = 120)]
        upstream_timeout: u64,
        /// Token this node presents to its upstream.
        #[arg(long, env = "WREN_UPSTREAM_TOKEN", hide_env_values = true)]
        upstream_token: Option<String>,
        /// See `wren cache --allow-tier-skip`.
        #[arg(long)]
        allow_tier_skip: bool,
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
    /// Create a commit on the server from local files, without a checkout.
    /// Optionally land it on a branch in the same request.
    Commit {
        #[arg(short, long)]
        message: String,
        /// Revision to commit on top of. Default: the --land branch, else HEAD.
        #[arg(long)]
        parent: Option<String>,
        /// Land the commit on this branch (rebased onto its tip if it moved).
        #[arg(long, value_name = "BRANCH")]
        land: Option<String>,
        /// "Name <email>". Default: WREN_AUTHOR, else git's user.name and user.email.
        #[arg(long, env = "WREN_AUTHOR")]
        author: Option<String>,
        /// Write REPO_PATH with the contents of LOCAL_FILE (repeatable). An
        /// executable file becomes 100755 and a symlink 120000; otherwise the
        /// path keeps its current mode.
        #[arg(long = "set", value_name = "REPO_PATH=LOCAL_FILE")]
        set: Vec<String>,
        /// Delete REPO_PATH (repeatable).
        #[arg(long = "delete", value_name = "REPO_PATH")]
        delete: Vec<String>,
        #[command(flatten)]
        write: WriteTarget,
        #[command(flatten)]
        target: Target,
    },
    /// Land commits on a branch: rebase them onto its tip on the server and
    /// move the branch. Exits 1 on conflicts or if the git host refuses.
    Land {
        #[arg(long, value_name = "BRANCH")]
        onto: String,
        /// Commits to land, oldest first; each must be the parent of the next.
        #[arg(required = true)]
        revs: Vec<String>,
        #[command(flatten)]
        write: WriteTarget,
        #[command(flatten)]
        target: Target,
    },
    /// Create a commit that undoes REV, and optionally land it.
    Revert {
        rev: String,
        #[arg(long, value_name = "BRANCH")]
        onto: Option<String>,
        /// "Name <email>". Default: WREN_AUTHOR, else git's user.name and user.email.
        #[arg(long, env = "WREN_AUTHOR")]
        author: Option<String>,
        #[command(flatten)]
        write: WriteTarget,
        #[command(flatten)]
        target: Target,
    },
    /// Serve the query and write API to an AI agent as an MCP server over
    /// stdio. Commits made through it are authored by WREN_AUTHOR.
    Mcp {
        #[command(flatten)]
        write: WriteTarget,
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

/// The client for writes: same repository as `read`, but on the write
/// server with the write token.
fn writer(read: &Client, t: &Target, w: &WriteTarget) -> Client {
    let server = w.write_server.as_deref().unwrap_or(&read.server);
    let token = w.write_token.as_deref().or(t.token.as_deref());
    Client::new(server, &read.repo, token)
}

/// Files above this size are uploaded on their own before the changeset, so
/// no single request gets large.
const INLINE_MAX: u64 = 256 << 10;

/// One `--set REPO_PATH=LOCAL_FILE`.
async fn local_change(w: &Client, spec: &str) -> Result<FileChange> {
    let (path, local) = spec
        .split_once('=')
        .with_context(|| format!("--set expects REPO_PATH=LOCAL_FILE, got {spec:?}"))?;
    let meta = std::fs::symlink_metadata(local).with_context(|| format!("reading {local}"))?;
    let (data, mode) = if meta.file_type().is_symlink() {
        use std::os::unix::ffi::OsStrExt;
        let target = std::fs::read_link(local)?;
        (target.as_os_str().as_bytes().to_vec(), Some("120000"))
    } else if meta.is_file() {
        use std::os::unix::fs::PermissionsExt;
        let exec = meta.permissions().mode() & 0o111 != 0;
        let data = std::fs::read(local).with_context(|| format!("reading {local}"))?;
        (data, exec.then_some("100755"))
    } else {
        bail!("{local} is not a file or a symlink");
    };
    let edit = if data.len() as u64 > INLINE_MAX {
        let oids = w.upload_blobs(&[Content::from_bytes(data)]).await?;
        Edit::Blob { blob: oids[0] }
    } else {
        Edit::Write(Content::from_bytes(data))
    };
    Ok(FileChange {
        path: path.to_string(),
        edit,
        mode: mode.map(String::from),
    })
}

/// `--author`, else WREN_AUTHOR (both via clap), else git's identity.
fn cli_author(author: Option<String>) -> Result<Person> {
    if let Some(a) = author {
        return Person::parse(&a);
    }
    let git = |key: &str| {
        std::process::Command::new("git")
            .args(["config", key])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    match (git("user.name"), git("user.email")) {
        (Some(name), Some(email)) => Ok(Person {
            name,
            email,
            time: None,
            tz: None,
        }),
        _ => bail!("pass --author \"Name <email>\" or set WREN_AUTHOR"),
    }
}

/// Print a land result: the branch move on stdout, problems on stderr.
/// Returns whether it landed.
fn report_land(l: &Landed) -> bool {
    match l {
        Landed::Landed {
            name,
            old,
            new,
            dropped,
            ..
        } => {
            println!("landed {name} {old} -> {new}");
            for d in dropped {
                eprintln!("dropped {d} (the branch already has its changes)");
            }
            true
        }
        Landed::Conflicts { onto, conflicts } => {
            eprintln!(
                "not landed: {} conflicting path(s) against {onto}:",
                conflicts.len()
            );
            for c in conflicts {
                let detail = c
                    .detail
                    .as_deref()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default();
                eprintln!(
                    "  {:<14} {}{detail}  [in {}]",
                    c.kind,
                    c.path,
                    c.changeset.short()
                );
            }
            eprintln!("update those files as of {onto} and commit again with --parent {onto}");
            false
        }
        Landed::Rejected { reason } => {
            eprintln!("not landed: the git host rejected the update: {reason}");
            false
        }
    }
}

fn exit_code(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
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

/// `123`, `64KiB`, `256MiB`, `50GiB`, `1TiB` (also KB/MB/GB/TB as powers of
/// 1000).
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().map_err(|_| format!("bad size {s:?}"))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kib" | "k" => 1 << 10,
        "mib" | "m" => 1 << 20,
        "gib" | "g" => 1 << 30,
        "tib" | "t" => 1 << 40,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        u => return Err(format!("bad size unit {u:?}")),
    };
    n.checked_mul(mult)
        .ok_or_else(|| format!("{s} is too large"))
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
            write_token,
            meta,
            segments,
            snapshot_every,
            snapshot_cone,
        } => {
            let meta = match meta {
                Some(url) => wren_core::meta::MetaConfig::Postgres(url),
                None => wren_core::meta::MetaConfig::Sqlite,
            };
            if write_token.is_some() && write_token == token {
                anyhow::bail!("--write-token must differ from --token");
            }
            let (objects, repos_dir) = data_layout(&data);
            let remote = match &segments {
                Some(url) => Some(Arc::new(wren_core::segments::Remote::open(url)?)),
                None => None,
            };
            let store = Arc::new(ObjectStore::open_with(objects, remote)?);
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
                    Arc::new(Repo::open_with(
                        repos_dir.join(name),
                        name,
                        store.clone(),
                        Some(&url),
                        &meta,
                    )?),
                );
            }
            // Repos registered by earlier runs.
            if let Ok(rd) = std::fs::read_dir(&repos_dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    let known = match &meta {
                        wren_core::meta::MetaConfig::Sqlite => {
                            e.path().join("meta.sqlite").exists()
                        }
                        wren_core::meta::MetaConfig::Postgres(_) => {
                            e.path().join("mirror.git").exists()
                        }
                    };
                    if !map.contains_key(&name) && known {
                        map.insert(
                            name.clone(),
                            Arc::new(Repo::open_with(
                                e.path(),
                                &name,
                                store.clone(),
                                None,
                                &meta,
                            )?),
                        );
                    }
                }
            }
            if token.is_none() && !listen.ip().is_loopback() {
                tracing::warn!("listening on {listen} without --token: every mirrored repository is readable by anyone who can reach this port");
            }
            let mut state = server::ServerState::new(store, map, token, write_token);
            state.snapshots = server::SnapshotConfig {
                dir: Some(data.join("snapshots")),
                cone: snapshot_cone,
                every: (snapshot_every > 0).then(|| Duration::from_secs(snapshot_every)),
            };
            let state = Arc::new(state);
            server::spawn_pollers(state.clone(), (poll > 0).then(|| Duration::from_secs(poll)));
            server::spawn_snapshotters(state.clone());
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
            tier,
            memory,
            allow_tier_skip,
        } => {
            let tier = match tier.as_str() {
                "node" => cache::Tier::Node,
                _ => cache::Tier::Region,
            };
            let state = Arc::new(cache::CacheState::new(
                cache::CacheConfig {
                    tier,
                    store_dir: dir.join("objects"),
                    dir: dir.clone(),
                    upstream,
                    token,
                    upstream_token,
                    upstream_concurrency,
                    memory: memory as usize,
                    budget: None,
                },
                Arc::new(wren_vcs::transport::Reqwest::new(Duration::from_secs(
                    upstream_timeout,
                ))?),
            )?);
            state
                .check_upstream(allow_tier_skip, Duration::from_secs(upstream_timeout))
                .await?;
            let listener = tokio::net::TcpListener::bind(listen).await?;
            tracing::info!(
                "wren cache ({}) listening on http://{listen}, upstream {}",
                tier.name(),
                state.upstream
            );
            axum::serve(listener, cache::router(state)).await?;
        }
        Cmd::Daemon {
            store,
            socket,
            listen,
            upstream,
            budget,
            lease_days,
            snapshot,
            memory,
            upstream_concurrency,
            upstream_timeout,
            upstream_token,
            allow_tier_skip,
        } => {
            std::fs::create_dir_all(&store)?;
            let state = Arc::new(cache::CacheState::new(
                cache::CacheConfig {
                    tier: cache::Tier::Node,
                    store_dir: store.clone(),
                    dir: store.join("daemon"),
                    upstream,
                    token: None,
                    upstream_token,
                    upstream_concurrency,
                    memory: memory as usize,
                    budget: Some((budget, Duration::from_secs(lease_days * 86400))),
                },
                Arc::new(wren_vcs::transport::Reqwest::new(Duration::from_secs(
                    upstream_timeout,
                ))?),
            )?);
            state
                .check_upstream(allow_tier_skip, Duration::from_secs(upstream_timeout))
                .await?;
            if let Some(repo) = &snapshot {
                let n = state.seed_from_snapshot(repo).await?;
                if n > 0 {
                    tracing::info!("seeded {n} objects from {repo}'s snapshot");
                }
            }
            // Periodic maintenance, besides the runs triggered by arrivals.
            {
                let st = state.clone();
                tokio::spawn(async move {
                    loop {
                        if let Err(e) = st.maintain().await {
                            tracing::warn!("maintenance failed: {e:#}");
                        }
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                });
            }
            let socket = socket.unwrap_or_else(|| store.join("daemon.sock"));
            let _ = std::fs::remove_file(&socket);
            let unix = tokio::net::UnixListener::bind(&socket)
                .with_context(|| format!("binding {}", socket.display()))?;
            tracing::info!(
                "wren daemon serving {} on {} (budget {budget} bytes), upstream {}",
                store.display(),
                socket.display(),
                state.upstream
            );
            let router = cache::router(state);
            match listen {
                Some(addr) => {
                    let tcp = tokio::net::TcpListener::bind(addr).await?;
                    let r2 = router.clone();
                    let (a, b) =
                        tokio::join!(async move { axum::serve(unix, router).await }, async move {
                            axum::serve(tcp, r2).await
                        });
                    a?;
                    b?;
                }
                None => axum::serve(unix, router).await?,
            }
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
                "checked out {} into {}: {} files, {} trees walked, fetched {} objects ({:.1} MiB) in {} requests, {} as deltas, {} bytes, {} ms",
                rep.commit.short(),
                dest.display(),
                rep.files,
                rep.trees_walked,
                rep.objects_fetched,
                rep.bytes_fetched as f64 / (1 << 20) as f64,
                rep.requests,
                rep.deltas,
                rep.bytes_fetched,
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
        Cmd::Commit {
            message,
            parent,
            land,
            author,
            set,
            delete,
            write,
            target,
        } => {
            let author = cli_author(author)?;
            let c = need_repo(&target).await?;
            let w = writer(&c, &target, &write);
            let mut changes = Vec::with_capacity(set.len() + delete.len());
            for spec in &set {
                changes.push(local_change(&w, spec).await?);
            }
            changes.extend(delete.into_iter().map(|path| FileChange {
                path,
                edit: Edit::Delete { delete: true },
                mode: None,
            }));
            if changes.is_empty() {
                bail!("nothing to commit: pass --set REPO_PATH=LOCAL_FILE or --delete REPO_PATH");
            }
            let parent = parent
                .or_else(|| land.clone())
                .unwrap_or_else(|| "HEAD".into());
            let r = w
                .create_changeset(&ChangesetRequest {
                    parents: vec![parent],
                    author,
                    committer: None,
                    message,
                    changes,
                    land: land.map(|onto| LandOnto { onto }),
                })
                .await?;
            let ok = !matches!(&r.land, Some(l) if !matches!(l, Landed::Landed { .. }));
            if target.json {
                print_json(&r)?;
                return Ok(exit_code(ok));
            }
            println!("{}", r.oid);
            return Ok(exit_code(r.land.as_ref().is_none_or(report_land)));
        }
        Cmd::Land {
            onto,
            revs,
            write,
            target,
        } => {
            let c = need_repo(&target).await?;
            let w = writer(&c, &target, &write);
            let l = w
                .land(&LandRequest {
                    changesets: revs,
                    onto,
                    committer: None,
                })
                .await?;
            if target.json {
                print_json(&l)?;
                return Ok(exit_code(matches!(l, Landed::Landed { .. })));
            }
            return Ok(exit_code(report_land(&l)));
        }
        Cmd::Revert {
            rev,
            onto,
            author,
            write,
            target,
        } => {
            let author = cli_author(author)?;
            let c = need_repo(&target).await?;
            let w = writer(&c, &target, &write);
            let r = w
                .revert(&RevertRequest {
                    commit: rev,
                    author,
                    committer: None,
                    onto,
                })
                .await?;
            let ok = !matches!(&r.land, Some(l) if !matches!(l, Landed::Landed { .. }));
            if target.json {
                print_json(&r)?;
                return Ok(exit_code(ok));
            }
            println!("{}", r.oid);
            return Ok(exit_code(r.land.as_ref().is_none_or(report_land)));
        }
        Cmd::Mcp { write, target } => {
            let c = need_repo(&target).await?;
            let w = writer(&c, &target, &write);
            mcp::serve(c, w).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn write_commands_parse() {
        let cli = Cli::try_parse_from([
            "wren",
            "commit",
            "-m",
            "msg",
            "--land",
            "main",
            "--set",
            "a/b.rs=/tmp/b.rs",
            "--set",
            "c=d",
            "--delete",
            "old.txt",
            "--write-server",
            "http://w",
            "--server",
            "http://r",
        ])
        .unwrap();
        let Cmd::Commit {
            message,
            parent,
            land,
            set,
            delete,
            write,
            target,
            ..
        } = cli.cmd
        else {
            panic!("not commit")
        };
        assert_eq!(message, "msg");
        assert_eq!(parent, None);
        assert_eq!(land.as_deref(), Some("main"));
        assert_eq!(set, ["a/b.rs=/tmp/b.rs", "c=d"]);
        assert_eq!(delete, ["old.txt"]);
        assert_eq!(write.write_server.as_deref(), Some("http://w"));
        assert_eq!(target.server.as_deref(), Some("http://r"));

        let cli = Cli::try_parse_from(["wren", "land", "--onto", "main", "a", "b"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Land { ref revs, .. } if revs == &["a", "b"]));
        assert!(Cli::try_parse_from(["wren", "land", "--onto", "main"]).is_err());
        let cli = Cli::try_parse_from(["wren", "revert", "abc", "--onto", "main"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Revert { ref rev, onto: Some(_), .. } if rev == "abc"));
    }

    #[test]
    fn writer_falls_back_to_read_target() {
        let read = Client::new("http://cache", "demo", Some("read"));
        let t = Target {
            server: None,
            token: Some("read".into()),
            repo: None,
            json: false,
        };
        let none = WriteTarget {
            write_server: None,
            write_token: None,
        };
        let w = writer(&read, &t, &none);
        assert_eq!(
            (w.server.as_str(), w.repo.as_str()),
            ("http://cache", "demo")
        );
        let set = WriteTarget {
            write_server: Some("http://server/".into()),
            write_token: Some("write".into()),
        };
        let w = writer(&read, &t, &set);
        assert_eq!(
            (w.server.as_str(), w.repo.as_str()),
            ("http://server", "demo")
        );
    }
}
