//! Content-addressed object store.
//!
//! Objects are kept one-per-file in git's loose layout (`ab/cdef…`, zlib
//! encoded). The store has no packfiles and therefore needs no maintenance
//! (the server's git mirror keeps refs only; see `GitMirror::absorb_objects`):
//! an object is written once, never changes, and can be cached by anyone
//! without coordination. Because the layout is git's own, a client store can
//! be attached to any git checkout via `.git/objects/info/alternates`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};

use crate::object::{self, Commit, Kind, TreeEntry};
use crate::oid::Oid;

pub struct ObjectStore {
    root: PathBuf,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl ObjectStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<ObjectStore> {
        let root = root.into();
        fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(ObjectStore { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, oid: &Oid) -> PathBuf {
        let hex = oid.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    pub fn has(&self, oid: &Oid) -> bool {
        self.path(oid).exists()
    }

    /// The stored loose bytes, exactly as they go over the wire.
    pub fn get_raw(&self, oid: &Oid) -> Result<Option<Vec<u8>>> {
        match fs::read(self.path(oid)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
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
        match self.get_raw(oid)? {
            Some(raw) => Ok(Some(object::decode_loose(&raw)?)),
            None => Ok(None),
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
