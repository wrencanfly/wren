//! `wren checkout`: materialize a commit (or a sparse slice of it) on disk.
//!
//! The client starts from the root tree and asks for missing trees level by
//! level, then for the blobs it needs. Directories outside the sparse cone
//! are never requested, so a sparse checkout costs what it reads, not what
//! the repository weighs.
//!
//! With `--git`, the result is also a working git repository: objects live in
//! the node-wide Wren store (attached via `objects/info/alternates`), the
//! index is written directly with stat data and skip-worktree bits, and the
//! upstream is configured as a promisor remote so `git commit` / `git push`
//! work as usual.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Result};
use rayon::prelude::*;
use wren_core::changes::join;
use wren_core::index::{write_index, IndexEntry, Stat};
use wren_core::object::{MODE_EXEC, MODE_GITLINK, MODE_SYMLINK};
use wren_core::sparse::Sparse;
use wren_core::{ObjectStore, Oid};

use crate::client::{CheckoutConfig, Client};

pub struct Options {
    pub rev: String,
    pub dest: PathBuf,
    pub sparse: Vec<String>,
    pub git: bool,
    /// In --git mode, list every file in the index (fetching every tree)
    /// instead of writing a sparse index. Only needed for git commands that
    /// do not understand sparse indexes.
    pub full_index: bool,
    pub store_dir: PathBuf,
    /// Override for the git remote URL written into `.git/config`.
    pub origin: Option<String>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub commit: Oid,
    pub files: usize,
    pub trees_walked: usize,
    pub objects_fetched: u64,
    /// How many of them arrived as deltas.
    pub deltas: u64,
    pub bytes_fetched: u64,
    pub requests: u64,
    pub millis: u128,
}

pub struct FileOut {
    pub path: String,
    pub mode: u32,
    pub oid: Oid,
    /// The same path in the reference tree (see `LAST_ROOT`), if any: a
    /// delta base hint for fetching `oid`.
    pub base: Option<Oid>,
}

/// File in a node store naming the root tree of the last checkout from it.
/// The next checkout walks that tree alongside the new one and offers each
/// path's old version as a delta base, so updating a warm node transfers
/// roughly the change, not every changed object whole.
const LAST_ROOT: &str = "wren-last-root";

/// Gives a lease back when dropped.
struct Release(PathBuf, String);

impl Drop for Release {
    fn drop(&mut self) {
        wren_core::bounded::release(&self.0, &self.1);
    }
}

fn last_root(store: &ObjectStore) -> Option<Oid> {
    let s = std::fs::read_to_string(store.root().join(LAST_ROOT)).ok()?;
    let oid: Oid = s.trim().parse().ok()?;
    store.has(&oid).then_some(oid)
}

fn set_last_root(store: &ObjectStore, tree: &Oid) -> Result<()> {
    let tmp = store
        .root()
        .join(format!("{LAST_ROOT}.{}", std::process::id()));
    std::fs::write(&tmp, format!("{tree}\n"))?;
    std::fs::rename(tmp, store.root().join(LAST_ROOT))?;
    Ok(())
}

pub async fn checkout(client: &Client, opts: Options) -> Result<Report> {
    let start = Instant::now();
    if opts.dest.exists() && std::fs::read_dir(&opts.dest)?.next().is_some() {
        bail!("{} exists and is not empty", opts.dest.display());
    }
    let sparse = Sparse::new(&opts.sparse);
    let commit_oid = client.resolve(&opts.rev).await?;
    // A node daemon keeps the objects of a leased commit. Taken before the
    // store is opened, so that what this checkout finds there stays. A
    // --git checkout keeps its lease (git reads its objects from the store
    // through alternates, without asking anyone); a plain one has its
    // files when done and gives the lease back.
    std::fs::create_dir_all(&opts.store_dir)?;
    let lease_name = wren_core::bounded::lease_name(&opts.dest);
    wren_core::bounded::lease(&opts.store_dir, &lease_name, &[commit_oid])?;
    let _release = (!opts.git).then(|| Release(opts.store_dir.clone(), lease_name.clone()));
    let store = Arc::new(ObjectStore::open(&opts.store_dir)?);
    client.ensure(&store, &[commit_oid]).await?;
    let commit = store.read_commit(&commit_oid)?;

    // Breadth-first, want-only tree walk. `inside` marks directories that are
    // part of the checkout. Directories outside the cone are never fetched:
    // in --git mode each one becomes a single sparse-directory entry in git's
    // sparse index, pointing at its tree id. Only with --full-index are they
    // walked (trees only, never blobs) to list every file in the index.
    // (git does read each sparse directory's own tree object; see below.)
    // (path, tree, inside the cone, the same path in the reference tree)
    let mut level: Vec<(String, Oid, bool, Option<Oid>)> =
        vec![(String::new(), commit.tree, true, last_root(&store))];
    let mut files: Vec<FileOut> = Vec::new();
    let mut outside: Vec<FileOut> = Vec::new();
    let mut sparse_dirs: Vec<(String, Oid)> = Vec::new();
    let mut boundary_bases: Vec<Option<Oid>> = Vec::new();
    let walk_outside = opts.git && opts.full_index;
    let mut dirs: Vec<String> = Vec::new();
    let mut trees_walked = 0;
    while !level.is_empty() {
        let wants: Vec<(Oid, Option<Oid>)> = level.iter().map(|l| (l.1, l.3)).collect();
        client.ensure_with_bases(&store, &wants).await?;
        trees_walked += level.len();
        let parsed = {
            let store = store.clone();
            tokio::task::spawn_blocking(move || -> Result<Vec<_>> {
                level
                    .into_par_iter()
                    .map(|(path, oid, inside, reference)| {
                        // The reference tree's entries, when this node has
                        // that tree; a hint only, so absence is fine.
                        let reference: std::collections::HashMap<Vec<u8>, _> = reference
                            .filter(|r| store.has(r))
                            .map(|r| store.read_tree(&r))
                            .transpose()?
                            .unwrap_or_default()
                            .into_iter()
                            .map(|e| (e.name.clone(), e))
                            .collect();
                        Ok((path, inside, store.read_tree(&oid)?, reference))
                    })
                    .collect()
            })
            .await??
        };
        let mut next = Vec::new();
        for (dir, inside, entries, reference) in parsed {
            for e in entries {
                let path = join(&dir, &e.name_str());
                let base = reference
                    .get(&e.name)
                    .filter(|r| r.is_tree() == e.is_tree() && r.oid != e.oid)
                    .map(|r| r.oid);
                if e.is_tree() {
                    let child_inside = inside && sparse.wants_dir(&path);
                    if child_inside {
                        dirs.push(path.clone());
                    }
                    if child_inside || walk_outside {
                        next.push((path, e.oid, child_inside, base));
                    } else if opts.git && inside {
                        sparse_dirs.push((path, e.oid));
                        boundary_bases.push(base);
                    }
                } else {
                    let f = FileOut {
                        path: path.clone(),
                        mode: e.mode,
                        oid: e.oid,
                        base,
                    };
                    if inside && sparse.wants_file(&path) {
                        files.push(f);
                    } else {
                        outside.push(f);
                    }
                }
            }
        }
        level = next;
    }

    // git's sparse index still reads the tree object of each sparse
    // directory (to compare it with HEAD), but never anything below it, so
    // the boundary trees are fetched and their subtrees are not.
    let boundary: Vec<(Oid, Option<Oid>)> = sparse_dirs
        .iter()
        .zip(&boundary_bases)
        .map(|(d, b)| (d.1, *b))
        .collect();
    client.ensure_with_bases(&store, &boundary).await?;
    trees_walked += boundary.len();

    let blobs: Vec<(Oid, Option<Oid>)> = files
        .iter()
        .filter(|f| f.mode != MODE_GITLINK)
        .map(|f| (f.oid, f.base))
        .collect();
    client.ensure_with_bases(&store, &blobs).await?;

    // Materialize the working tree in parallel.
    std::fs::create_dir_all(&opts.dest)?;
    let dest = opts.dest.canonicalize()?;
    for d in &dirs {
        std::fs::create_dir_all(dest.join(d))?;
    }
    let (files, stats) = {
        let store = store.clone();
        let dest = dest.clone();
        tokio::task::spawn_blocking(move || -> Result<_> {
            let stats = files
                .par_iter()
                .map(|f| write_file(&store, &dest, f))
                .collect::<Result<Vec<_>>>()?;
            Ok((files, stats))
        })
        .await??
    };

    if opts.git {
        let info = client.repo_info().await.ok();
        let origin = opts
            .origin
            .clone()
            .or_else(|| info.as_ref().and_then(|i| i.upstream.clone()));
        let branch = branch_for(
            client,
            &opts.rev,
            info.as_ref().and_then(|i| i.head.clone()),
        )
        .await;
        let git = GitSetup {
            commit: commit_oid,
            branch,
            origin,
            sparse: &sparse,
            server: &client.server,
            repo: &client.repo,
        };
        write_git_dir(&dest, &store, &git, &files, &stats, &outside, &sparse_dirs)?;
    }
    let cfg = CheckoutConfig {
        server: client.server.clone(),
        repo: client.repo.clone(),
        commit: commit_oid,
        rev: opts.rev.clone(),
        sparse: sparse.dirs().to_vec(),
    };
    std::fs::create_dir_all(dest.join(".wren"))?;
    std::fs::write(
        dest.join(".wren/config.json"),
        serde_json::to_vec_pretty(&cfg)?,
    )?;

    set_last_root(&store, &commit.tree)?;

    Ok(Report {
        commit: commit_oid,
        files: files.len(),
        trees_walked,
        deltas: client.stats.deltas.load(Ordering::Relaxed),
        objects_fetched: client.stats.objects.load(Ordering::Relaxed),
        bytes_fetched: client.stats.bytes.load(Ordering::Relaxed),
        requests: client.stats.requests.load(Ordering::Relaxed),
        millis: start.elapsed().as_millis(),
    })
}

fn write_file(store: &ObjectStore, dest: &Path, f: &FileOut) -> Result<Option<Stat>> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let path = dest.join(&f.path);
    match f.mode {
        MODE_GITLINK => {
            // Submodules are not fetched; leave an empty directory as git does.
            std::fs::create_dir_all(&path)?;
            return Ok(None);
        }
        MODE_SYMLINK => {
            let target = store.read_blob(&f.oid)?;
            std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&target), &path)?;
        }
        _ => {
            std::fs::write(&path, store.read_blob(&f.oid)?)?;
            if f.mode == MODE_EXEC {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
            }
        }
    }
    Ok(Some(Stat::from_metadata(&std::fs::symlink_metadata(
        &path,
    )?)))
}

/// If `rev` names a branch, return its short name so HEAD can be attached.
async fn branch_for(client: &Client, rev: &str, head: Option<String>) -> Option<String> {
    let full = if rev == "HEAD" {
        head?
    } else if rev.starts_with("refs/heads/") {
        rev.to_string()
    } else {
        format!("refs/heads/{rev}")
    };
    let refs: Vec<wren_core::api::RefInfo> = client.get("refs", &[]).await.ok()?;
    refs.iter()
        .any(|r| r.name == full)
        .then(|| full.trim_start_matches("refs/heads/").to_string())
}

pub struct GitSetup<'a> {
    pub commit: Oid,
    pub branch: Option<String>,
    pub origin: Option<String>,
    pub sparse: &'a Sparse,
    pub server: &'a str,
    pub repo: &'a str,
}

fn write_git_dir(
    dest: &Path,
    store: &ObjectStore,
    g: &GitSetup,
    files: &[FileOut],
    stats: &[Option<Stat>],
    outside: &[FileOut],
    sparse_dirs: &[(String, Oid)],
) -> Result<()> {
    let git = dest.join(".git");
    for d in [
        "objects/info",
        "objects/pack",
        "refs/heads",
        "refs/tags",
        "refs/remotes/origin",
        "info",
    ] {
        std::fs::create_dir_all(git.join(d))?;
    }
    // Objects come from the node-wide store; nothing is copied.
    std::fs::write(
        git.join("objects/info/alternates"),
        format!("{}\n", store.root().canonicalize()?.display()),
    )?;
    // History beyond this commit is served by the Wren API, not stored locally.
    std::fs::write(git.join("shallow"), format!("{}\n", g.commit))?;
    match &g.branch {
        Some(b) => {
            std::fs::write(git.join("HEAD"), format!("ref: refs/heads/{b}\n"))?;
            write_ref(&git.join("refs/heads").join(b), &g.commit)?;
            write_ref(&git.join("refs/remotes/origin").join(b), &g.commit)?;
        }
        None => std::fs::write(git.join("HEAD"), format!("{}\n", g.commit))?,
    }
    let mut config = String::from(
        "[core]\n\trepositoryformatversion = 1\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n",
    );
    if cfg!(target_os = "macos") {
        config.push_str("\tignorecase = true\n\tprecomposeunicode = true\n");
    }
    if !g.sparse.is_full() {
        config.push_str("\tsparseCheckout = true\n\tsparseCheckoutCone = true\n");
    }
    if !sparse_dirs.is_empty() {
        config.push_str("[index]\n\tsparse = true\n");
    }
    if let Some(origin) = &g.origin {
        config.push_str(&format!(
            "[extensions]\n\tpartialclone = origin\n[remote \"origin\"]\n\turl = {origin}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n\tpromisor = true\n\tpartialclonefilter = blob:none\n"
        ));
        if let Some(b) = &g.branch {
            config.push_str(&format!(
                "[branch \"{b}\"]\n\tremote = origin\n\tmerge = refs/heads/{b}\n"
            ));
        }
    }
    config.push_str(&format!(
        "[wren]\n\tserver = {}\n\trepo = {}\n",
        g.server, g.repo
    ));
    std::fs::write(git.join("config"), config)?;
    std::fs::write(git.join("description"), "Wren checkout\n")?;
    std::fs::write(git.join("info/exclude"), "/.wren/\n")?;
    if !g.sparse.is_full() {
        std::fs::write(git.join("info/sparse-checkout"), g.sparse.git_patterns())?;
    }

    let mut entries: Vec<IndexEntry> = files
        .iter()
        .zip(stats)
        .map(|(f, st)| IndexEntry {
            path: f.path.clone().into_bytes(),
            mode: f.mode,
            oid: f.oid,
            stat: st.clone(),
            skip_worktree: false,
        })
        .chain(outside.iter().map(|f| IndexEntry {
            path: f.path.clone().into_bytes(),
            mode: f.mode,
            oid: f.oid,
            stat: None,
            skip_worktree: true,
        }))
        .chain(sparse_dirs.iter().map(|(path, oid)| IndexEntry {
            path: format!("{path}/").into_bytes(),
            mode: wren_core::object::MODE_TREE,
            oid: *oid,
            stat: None,
            skip_worktree: true,
        }))
        .collect();
    write_index(&git.join("index"), &mut entries)?;
    Ok(())
}

fn write_ref(path: &Path, oid: &Oid) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, format!("{oid}\n"))?;
    Ok(())
}
