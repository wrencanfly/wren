//! Content-addressed object store, in git's own object-directory layout.
//!
//! * Segments (`pack/pack-*.pack` + `.idx`): immutable git packs, written
//!   once and never repacked. A fetch's pack is moved in whole, with git's
//!   deltas, so the server stores a repository in about the size of its git
//!   pack (see `pack`).
//! * Loose objects (`ab/cdef…`, zlib): what clients and caches receive over
//!   the wire and keep, and what the server writes for objects it creates.
//!
//! Either way an object is written once, never changes, and can be cached by
//! anyone without coordination. Because the layout is git's own, any git
//! repository can borrow a store through `objects/info/alternates`: the
//! server's mirror does, and so do `--git` checkouts.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};

use crate::object::{self, Commit, Kind, TreeEntry};
use crate::oid::Oid;
use crate::pack::Packs;

pub struct ObjectStore {
    root: PathBuf,
    packs: Packs,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl ObjectStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<ObjectStore> {
        let root = root.into();
        fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        let packs = Packs::open(root.join("pack"))?;
        Ok(ObjectStore { root, packs })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, oid: &Oid) -> PathBuf {
        let hex = oid.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    pub fn has(&self, oid: &Oid) -> bool {
        self.path(oid).exists() || self.packs.has(oid)
    }

    /// The object in loose encoding, exactly as it goes over the wire. A
    /// loose object is returned as stored; one in a segment is re-encoded.
    pub fn get_raw(&self, oid: &Oid) -> Result<Option<Vec<u8>>> {
        match fs::read(self.path(oid)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(self
                .packs
                .get(oid)?
                .map(|(kind, data)| object::encode_loose(kind, &data))),
            Err(e) => Err(e.into()),
        }
    }

    /// Segments in this store.
    pub fn packs(&self) -> &Packs {
        &self.packs
    }

    /// Move a complete git pack (`.pack` with its `.idx`, and `.rev` if
    /// present) into the store as a segment. The `.idx` goes last: a
    /// segment without one is ignored, so a crash leaves nothing half
    /// visible.
    pub fn add_pack(&self, pack: &Path) -> Result<()> {
        let dir = self.packs.dir();
        fs::create_dir_all(dir)?;
        let name = pack.file_name().context("pack path without a name")?;
        let dest = dir.join(name);
        for ext in ["pack", "rev", "idx"] {
            let from = pack.with_extension(ext);
            if from.exists() {
                move_file(&from, &dest.with_extension(ext))?;
            }
        }
        self.packs.refresh()?;
        Ok(())
    }

    /// Store loose bytes for `oid`. Writes are atomic (temp file + rename), so
    /// concurrent writers of the same object are harmless.
    pub fn put_raw(&self, oid: &Oid, raw: &[u8]) -> Result<()> {
        let path = self.path(oid);
        if path.exists() {
            return Ok(());
        }
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(raw)?;
        }
        // Loose objects are read-only in git too.
        let mut perm = fs::metadata(&tmp)?.permissions();
        perm.set_readonly(true);
        fs::set_permissions(&tmp, perm)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Verify `raw` hashes to `oid` before storing it. Used for data received
    /// from the network.
    pub fn put_raw_verified(&self, oid: &Oid, raw: &[u8]) -> Result<(Kind, Vec<u8>)> {
        let decoded = object::decode_loose_verified(oid, raw)?;
        self.put_raw(oid, raw)?;
        Ok(decoded)
    }

    pub fn put(&self, kind: Kind, data: &[u8]) -> Result<Oid> {
        let oid = Oid::hash_object(kind.as_str(), data);
        if !self.has(&oid) {
            self.put_raw(&oid, &object::encode_loose(kind, data))?;
        }
        Ok(oid)
    }

    pub fn get(&self, oid: &Oid) -> Result<Option<(Kind, Vec<u8>)>> {
        match fs::read(self.path(oid)) {
            Ok(raw) => Ok(Some(object::decode_loose(&raw)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.packs.get(oid),
            Err(e) => Err(e.into()),
        }
    }

    pub fn get_kind(&self, oid: &Oid, want: Kind) -> Result<Vec<u8>> {
        let (kind, data) = self
            .get(oid)?
            .ok_or_else(|| crate::not_found(format!("object {oid} not found")))?;
        if kind != want {
            bail!(
                "object {oid} is a {}, expected {}",
                kind.as_str(),
                want.as_str()
            );
        }
        Ok(data)
    }

    pub fn read_commit(&self, oid: &Oid) -> Result<Commit> {
        Commit::parse(&self.get_kind(oid, Kind::Commit)?)
            .with_context(|| format!("parsing commit {oid}"))
    }

    pub fn read_tree(&self, oid: &Oid) -> Result<Vec<TreeEntry>> {
        object::parse_tree(&self.get_kind(oid, Kind::Tree)?)
            .with_context(|| format!("parsing tree {oid}"))
    }

    pub fn read_blob(&self, oid: &Oid) -> Result<Vec<u8>> {
        self.get_kind(oid, Kind::Blob)
    }
}

/// Rename, or copy and delete across filesystems.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    let tmp = to.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::copy(from, &tmp).with_context(|| format!("copying {}", from.display()))?;
    fs::rename(&tmp, to)?;
    fs::remove_file(from)?;
    Ok(())
}
