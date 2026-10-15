//! Snapshots: one segment holding a commit's tree, to seed new nodes.
//!
//! When many nodes start at once, each asking for the same few thousand
//! objects one batch at a time would flood the tier above them. Instead the
//! server periodically writes the default branch's tip (every tree, and
//! the blobs of a cone of popular directories) into a single segment and
//! publishes it under its content hash. A new node downloads it (resumably,
//! through the cache tiers, which keep it like any immutable file), installs
//! it as a segment, and afterwards asks only for what it still lacks.
//!
//! A snapshot is a pure function of (commit, cone): objects in a fixed
//! order, written by `pack::write_pack`, so any server sharing the
//! metadata can rebuild the same bytes on demand and serve them.

use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::object::Kind;
use crate::oid::Oid;
use crate::repo::Repo;
use crate::sparse::Sparse;

/// The current snapshot of a repository (what `latest` returns).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SnapshotInfo {
    /// `pack-<hex>`: the segment's name, and so its content hash.
    pub name: String,
    pub commit: Oid,
    /// Directories whose blobs are included (empty: all of them).
    pub cone: Vec<String>,
    pub objects: usize,
    /// Size of the `.pack`.
    pub size: u64,
}

const KV: &str = "snapshot";

/// Write the snapshot of `commit` into `dir` (if it is not there already)
/// and return its description.
pub fn build(repo: &Repo, commit: &Oid, cone: &[String], dir: &Path) -> Result<SnapshotInfo> {
    let store = &repo.store;
    let sparse = Sparse::new(cone);
    let c = store.read_commit(commit)?;
    // Breadth-first, entries in tree order: deterministic.
    let mut objects: Vec<(Oid, Kind, Vec<u8>)> =
        vec![(*commit, Kind::Commit, store.get_kind(commit, Kind::Commit)?)];
    // Every tree (a node needs them all for sparse indexes and tree walks),
    // and the blobs inside the cone.
    let mut level = vec![(String::new(), c.tree)];
    while !level.is_empty() {
        let mut next = Vec::new();
        for (path, tree) in level {
            let data = store.get_kind(&tree, Kind::Tree)?;
            for e in crate::object::parse_tree(&data)? {
                let p = crate::changes::join(&path, &String::from_utf8_lossy(&e.name));
                if e.is_tree() {
                    next.push((p, e.oid));
                } else if !e.is_gitlink() && sparse.wants_file(&p) {
                    objects.push((e.oid, Kind::Blob, store.get_kind(&e.oid, Kind::Blob)?));
                }
            }
            objects.push((tree, Kind::Tree, data));
        }
        level = next;
    }
    let refs: Vec<(Oid, Kind, &[u8])> = objects
        .iter()
        .map(|(o, k, d)| (*o, *k, d.as_slice()))
        .collect();
    let tmp = dir.join(format!("tmp-{}", std::process::id()));
    let pack = crate::pack::write_pack(&tmp, &refs)?;
    let name = pack.file_stem().unwrap().to_string_lossy().into_owned();
    std::fs::create_dir_all(dir)?;
    let dest = dir.join(format!("{name}.pack"));
    let size = std::fs::metadata(&pack)?.len();
    if !dest.with_extension("idx").exists() {
        std::fs::rename(&pack, &dest)?;
        std::fs::rename(pack.with_extension("idx"), dest.with_extension("idx"))?;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    let mut seen = std::collections::HashSet::new();
    Ok(SnapshotInfo {
        name,
        commit: *commit,
        cone: cone.to_vec(),
        objects: objects.iter().filter(|o| seen.insert(o.0)).count(),
        size,
    })
}

/// Build a snapshot of `rev`'s commit and make it the repository's current
/// one (in the shared metadata).
pub fn publish(repo: &Repo, rev: &str, cone: &[String], dir: &Path) -> Result<SnapshotInfo> {
    let commit = repo.resolve(rev)?;
    if let Some(cur) = current(repo)? {
        if cur.commit == commit
            && cur.cone == cone
            && dir.join(format!("{}.idx", cur.name)).exists()
        {
            return Ok(cur);
        }
    }
    let info = build(repo, &commit, cone, dir)?;
    repo.meta.set_kv(KV, &serde_json::to_string(&info)?)?;
    Ok(info)
}

pub fn current(repo: &Repo) -> Result<Option<SnapshotInfo>> {
    match repo.meta.get_kv(KV)? {
        Some(s) if !s.is_empty() => Ok(Some(serde_json::from_str(&s)?)),
        _ => Ok(None),
    }
}

/// Path of a snapshot file in `dir`, rebuilding it when this server does
/// not have it but `repo`'s current snapshot is that one (another server
/// built it). `file` is `pack-<hex>.pack` or `pack-<hex>.idx`.
pub fn locate(repo: &Repo, dir: &Path, file: &str) -> Result<Option<PathBuf>> {
    let Some((name, ext)) = file.rsplit_once('.') else {
        return Ok(None);
    };
    if !(ext == "pack" || ext == "idx") || !is_segment_name(name) {
        return Ok(None);
    }
    let path = dir.join(file);
    if path.exists() {
        return Ok(Some(path));
    }
    match current(repo)? {
        Some(cur) if cur.name == name => {
            let rebuilt = build(repo, &cur.commit, &cur.cone, dir)?;
            ensure!(
                rebuilt.name == cur.name,
                "snapshot {} could not be rebuilt identically",
                cur.name
            );
            Ok(Some(path))
        }
        _ => Ok(None),
    }
}

pub fn is_segment_name(name: &str) -> bool {
    name.strip_prefix("pack-")
        .is_some_and(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Check a downloaded snapshot file against its name: a `.pack` ends with
/// its own checksum, which is the name; an `.idx` ends with its own
/// checksum, preceded by the pack's.
pub fn verify(name: &str, ext: &str, bytes: &[u8]) -> Result<()> {
    let want = name.strip_prefix("pack-").context("bad snapshot name")?;
    let n = bytes.len();
    ensure!(n >= 40, "{name}.{ext} is truncated");
    let own = Sha1::digest(&bytes[..n - 20]);
    ensure!(own[..] == bytes[n - 20..], "{name}.{ext} is corrupt");
    let pack_sum = match ext {
        "pack" => &bytes[n - 20..],
        "idx" => &bytes[n - 40..n - 20],
        _ => anyhow::bail!("not a snapshot file: {ext}"),
    };
    ensure!(
        hex::encode(pack_sum) == want,
        "{name}.{ext} belongs to another snapshot"
    );
    Ok(())
}
