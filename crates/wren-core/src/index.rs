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
    let digest = Sha1::digest(&b);
    b.extend(digest.as_slice());
    let tmp = path.with_extension("lock");
    std::fs::File::create(&tmp)?.write_all(&b)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
