//! Minimal writer for git's index file (version 2/3), so a Wren checkout is a
//! working git repository without asking git to re-read or re-hash anything.
//! Files outside the sparse cone get the skip-worktree bit, exactly as
//! `git sparse-checkout` would set it. Whole directories outside the cone can
//! be written as sparse-directory entries (git's sparse index), so their
//! trees never need to be fetched.

use std::fs::Metadata;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Result;
use sha1::{Digest, Sha1};

use crate::oid::Oid;

#[derive(Clone, Debug, Default)]
pub struct Stat {
    pub ctime: (u32, u32),
    pub mtime: (u32, u32),
    pub dev: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
}

impl Stat {
    pub fn from_metadata(m: &Metadata) -> Stat {
        Stat {
            ctime: (m.ctime() as u32, m.ctime_nsec() as u32),
            mtime: (m.mtime() as u32, m.mtime_nsec() as u32),
            dev: m.dev() as u32,
            ino: m.ino() as u32,
            uid: m.uid(),
            gid: m.gid(),
            size: m.size() as u32,
        }
    }
}

#[derive(Clone, Debug)]
pub struct IndexEntry {
    pub path: Vec<u8>,
    pub mode: u32,
    pub oid: Oid,
    /// None for entries not present in the working tree.
    pub stat: Option<Stat>,
    pub skip_worktree: bool,
}

const SKIP_WORKTREE: u16 = 0x4000;
const EXTENDED: u16 = 0x4000;

pub fn write_index(path: &Path, entries: &mut [IndexEntry]) -> Result<()> {
    write_index_with(path, entries, None, &[])
}

/// A directory for git's cache-tree extension (`TREE`): its path ("" for
/// the root) and its tree id. Entry and subtree counts are computed from
/// the index entries.
pub struct CachedTree {
    pub path: String,
    pub oid: Oid,
}

/// `write_index`, optionally with git's fsmonitor extension (`FSMN`, version
/// 2): `token` is what git will hand the fsmonitor hook next, and every
/// entry is marked valid (an empty "dirty" bitmap), so git trusts the index
/// for every path the hook does not report as changed, without a stat.
pub fn write_index_with(
    path: &Path,
    entries: &mut [IndexEntry],
    fsmonitor_token: Option<&str>,
    trees: &[CachedTree],
) -> Result<()> {
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let extended = entries.iter().any(|e| e.skip_worktree);
    let version: u32 = if extended { 3 } else { 2 };
    let mut b: Vec<u8> = Vec::with_capacity(entries.len() * 96 + 32);
    b.extend(b"DIRC");
    b.extend(version.to_be_bytes());
    b.extend((entries.len() as u32).to_be_bytes());
    for e in entries.iter() {
        let start = b.len();
        let st = e.stat.clone().unwrap_or_default();
        for v in [
            st.ctime.0, st.ctime.1, st.mtime.0, st.mtime.1, st.dev, st.ino, e.mode, st.uid, st.gid,
            st.size,
        ] {
            b.extend(v.to_be_bytes());
        }
        b.extend(e.oid.0);
        let mut flags = e.path.len().min(0xfff) as u16;
        if e.skip_worktree {
            flags |= EXTENDED;
        }
        b.extend(flags.to_be_bytes());
        if e.skip_worktree {
            b.extend(SKIP_WORKTREE.to_be_bytes());
        }
        b.extend(&e.path);
        // NUL-terminate and pad the entry to a multiple of 8 bytes.
        let len = b.len() - start;
        let padded = (len + 8) & !7;
        b.resize(start + padded, 0);
    }
    // A sparse index (entries for whole directories, named "dir/") must
    // announce itself with an empty "sdir" extension.
    if entries.iter().any(|e| e.path.last() == Some(&b'/')) {
        b.extend(b"sdir");
        b.extend(0u32.to_be_bytes());
    }
    if !trees.is_empty() {
        // Cache tree: every directory's tree id is already known, so git
        // writes a commit's tree by recomputing only the directories whose
        // entries changed (and checks only their blobs).
        let mut ext = Vec::new();
        let ids: std::collections::HashMap<&str, &Oid> =
            trees.iter().map(|t| (t.path.as_str(), &t.oid)).collect();
        // Children of each directory, sorted as git sorts paths.
        let mut children: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for t in trees.iter().filter(|t| !t.path.is_empty()) {
            let parent = t.path.rsplit_once('/').map_or("", |(p, _)| p);
            children.entry(parent).or_default().push(&t.path);
        }
        for v in children.values_mut() {
            v.sort_by(|a, b| {
                let (a, b) = (format!("{a}/"), format!("{b}/"));
                a.cmp(&b)
            });
        }
        let count = |dir: &str| {
            let prefix = format!("{dir}/");
            entries
                .iter()
                .filter(|e| dir.is_empty() || e.path.starts_with(prefix.as_bytes()))
                .count()
        };
        fn walk(
            dir: &str,
            ids: &std::collections::HashMap<&str, &Oid>,
            children: &std::collections::BTreeMap<&str, Vec<&str>>,
            count: &dyn Fn(&str) -> usize,
            ext: &mut Vec<u8>,
        ) {
            let name = dir.rsplit('/').next().unwrap_or(dir);
            let subs = children.get(dir).map_or(&[][..], |v| v.as_slice());
            ext.extend(name.as_bytes());
            ext.push(0);
            ext.extend(format!("{} {}\n", count(dir), subs.len()).as_bytes());
            ext.extend(ids[dir].0);
            for s in subs {
                walk(s, ids, children, count, ext);
            }
        }
        if ids.contains_key("") {
            walk("", &ids, &children, &count, &mut ext);
            b.extend(b"TREE");
            b.extend((ext.len() as u32).to_be_bytes());
            b.extend(ext);
        }
    }
    if let Some(token) = fsmonitor_token {
        let mut ext = Vec::new();
        ext.extend(2u32.to_be_bytes());
        ext.extend(token.as_bytes());
        ext.push(0);
        // An empty EWAH bitmap: bit size 0, one (empty) run-length word,
        // and the position of that word.
        let mut ewah = Vec::new();
        ewah.extend(0u32.to_be_bytes());
        ewah.extend(1u32.to_be_bytes());
        ewah.extend(0u64.to_be_bytes());
        ewah.extend(0u32.to_be_bytes());
        ext.extend((ewah.len() as u32).to_be_bytes());
        ext.extend(ewah);
        b.extend(b"FSMN");
        b.extend((ext.len() as u32).to_be_bytes());
        b.extend(ext);
    }
    let digest = Sha1::digest(&b);
    b.extend(digest.as_slice());
    let tmp = path.with_extension("lock");
    std::fs::File::create(&tmp)?.write_all(&b)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
