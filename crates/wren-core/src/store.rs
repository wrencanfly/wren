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
//! anyone without coordination.
//!
//! With a bucket (`open_with`), segments are shared through it: every
//! segment this store adds is uploaded before it is used, segments other
//! servers upload are read from the bucket on demand, and loose objects the
//! server writes are sealed into a segment (`publish`) before any metadata
//! that refers to them is published. Because the layout is git's own, any git
//! repository can borrow a store through `objects/info/alternates`: the
//! server's mirror does, and so do `--git` checkouts.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};

use crate::object::{self, Commit, Kind, TreeEntry};
use crate::oid::Oid;
use crate::pack::Packs;
use crate::segments::Remote;

pub struct ObjectStore {
    root: PathBuf,
    packs: Packs,
    remote: Option<Arc<Remote>>,
    /// Loose objects were written since the last `publish`.
    dirty: AtomicBool,
    sealing: Mutex<()>,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl ObjectStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<ObjectStore> {
        ObjectStore::open_with(root, None)
    }

    /// A store whose segments are shared through `remote`. Segments read
    /// from it are cached under `root/remote`.
    pub fn open_with(root: impl Into<PathBuf>, remote: Option<Arc<Remote>>) -> Result<ObjectStore> {
        let root = root.into();
        fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        let packs = Packs::open_with(
            root.join("pack"),
            remote.clone().map(|r| (r, root.join("remote"))),
        )?;
        Ok(ObjectStore {
            root,
            packs,
            remote,
            dirty: AtomicBool::new(true),
            sealing: Mutex::new(()),
        })
    }

    pub fn remote(&self) -> Option<&Arc<Remote>> {
        self.remote.as_ref()
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
    /// visible. With a bucket, the segment is uploaded first.
    pub fn add_pack(&self, pack: &Path) -> Result<()> {
        if let Some(r) = &self.remote {
            r.put_segment(pack)?;
        }
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
        self.dirty.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// With a bucket: seal every loose object into one new segment and
    /// upload it, so other servers can read them. Call before publishing
    /// metadata that refers to objects this server wrote. When this
    /// returns, every object written before the call is in the bucket
    /// (concurrent callers wait for a seal in progress). Without a bucket,
    /// or with nothing new, it does nothing.
    pub fn publish(&self) -> Result<usize> {
        if self.remote.is_none() {
            return Ok(0);
        }
        let _seal = self.sealing.lock().unwrap();
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return Ok(0);
        }
        let r = self.seal_loose();
        if r.is_err() {
            self.dirty.store(true, Ordering::SeqCst);
        }
        r
    }

    fn seal_loose(&self) -> Result<usize> {
        let mut files = Vec::new();
        let mut objects = Vec::new();
        for fan in fs::read_dir(&self.root)?.flatten() {
            let name = fan.file_name().to_string_lossy().to_string();
            if name.len() != 2 || !fan.path().is_dir() {
                continue;
            }
            for f in fs::read_dir(fan.path())?.flatten() {
                let rest = f.file_name().to_string_lossy().to_string();
                let Ok(oid) = Oid::from_hex(&format!("{name}{rest}")) else {
                    continue;
                };
                let (kind, data) = object::decode_loose(&fs::read(f.path())?)?;
                objects.push((oid, kind, data));
                files.push(f.path());
            }
        }
        if objects.is_empty() {
            return Ok(0);
        }
        let tmp = self.root.join(format!(
            "seal-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let refs: Vec<(Oid, Kind, &[u8])> = objects
            .iter()
            .map(|(o, k, d)| (*o, *k, d.as_slice()))
            .collect();
        let pack = crate::pack::write_pack(&tmp, &refs)?;
        self.add_pack(&pack)?;
        let _ = fs::remove_dir_all(&tmp);
        // Readers that miss a loose file fall back to the segment, which
        // is already known.
        for f in files {
            let _ = fs::remove_file(f);
        }
        Ok(objects.len())
    }

    /// With a bucket: download every segment this store only reads from
    /// the bucket, so that the store is complete on local disk (git reads
    /// it through alternates). Returns the number downloaded.
    pub fn hydrate(&self) -> Result<usize> {
        self.packs.hydrate()
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
        // No listing of the bucket: at worst an object is written twice.
        if !self.path(&oid).exists() && !self.packs.has_known(&oid) {
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
