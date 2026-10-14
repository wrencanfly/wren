//! Read-only access to git packfiles, used as Wren's storage segments.
//!
//! A segment is a git pack (`pack-<hash>.pack` + `.idx` v2) in the store's
//! `pack/` directory. Segments are written once and never modified or
//! repacked: a fetch's pack is moved in as is (it already carries git's
//! deltas), so storing a repository costs about what its git pack does.
//! Because the format is git's own, the server's git mirror reads the same
//! segments through `objects/info/alternates`.
//!
//! Objects are found through the `.idx` (fan-out table + binary search) and
//! rebuilt by inflating the entry and applying its delta chain. Resolved
//! delta bases are kept in a small cache shared by all readers.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, ensure, Context, Result};
use flate2::{Decompress, FlushDecompress, Status};
use memmap2::Mmap;

use crate::object::Kind;
use crate::oid::Oid;

const OBJ_OFS_DELTA: u8 = 6;
const OBJ_REF_DELTA: u8 = 7;
/// Delta chains in git packs are at most 50 deep by default; refuse cycles
/// and absurd chains in damaged packs.
const MAX_CHAIN: usize = 10_000;

pub struct Pack {
    /// Unique per opened pack, for cache keys. (A pack's position in the
    /// list changes as segments are added, so it cannot be the key.)
    id: usize,
    path: PathBuf,
    pack: Mmap,
    idx: Mmap,
    count: usize,
}

impl Pack {
    /// Open `pack-*.pack` and its `.idx`.
    pub fn open(pack_path: &Path) -> Result<Pack> {
        let idx_path = pack_path.with_extension("idx");
        // SAFETY: segments are immutable once their .idx exists; nothing
        // writes to a mapped file.
        let pack = unsafe { Mmap::map(&File::open(pack_path)?)? };
        let idx = unsafe {
            Mmap::map(&File::open(&idx_path).with_context(|| format!("{}", idx_path.display()))?)?
        };
        ensure!(
            pack.len() >= 32 && &pack[..4] == b"PACK",
            "{} is not a pack",
            pack_path.display()
        );
        ensure!(
            idx.len() >= 8 + 1024 && idx[..8] == [0xff, b't', b'O', b'c', 0, 0, 0, 2],
            "{} is not a version 2 pack index",
            idx_path.display()
        );
        let count = be32(&idx, 8 + 255 * 4) as usize;
        ensure!(
            idx.len() >= 8 + 1024 + count * (20 + 4 + 4) + 40,
            "{} is truncated",
            idx_path.display()
        );
        static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        Ok(Pack {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            path: pack_path.to_path_buf(),
            pack,
            idx,
            count,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn count(&self) -> usize {
        self.count
    }

    fn oid_at(&self, i: usize) -> &[u8] {
        let base = 8 + 1024;
        &self.idx[base + i * 20..base + i * 20 + 20]
    }

    /// Offset of `oid` in the pack, if the pack has it.
    pub fn find(&self, oid: &Oid) -> Option<u64> {
        let first = oid.as_bytes()[0] as usize;
        let lo = if first == 0 {
            0
        } else {
            be32(&self.idx, 8 + (first - 1) * 4) as usize
        };
        let hi = be32(&self.idx, 8 + first * 4) as usize;
        let (mut lo, mut hi) = (lo, hi);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.oid_at(mid).cmp(oid.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(self.offset_at(mid)),
            }
        }
        None
    }

    fn offset_at(&self, i: usize) -> u64 {
        let n = self.count;
        let off_table = 8 + 1024 + n * 24;
        let v = be32(&self.idx, off_table + i * 4);
        if v & 0x8000_0000 == 0 {
            v as u64
        } else {
            let large = off_table + n * 4 + (v & 0x7fff_ffff) as usize * 8;
            u64::from_be_bytes(self.idx[large..large + 8].try_into().unwrap())
        }
    }

    /// Every object id in the pack.
    pub fn oids(&self) -> impl Iterator<Item = Oid> + '_ {
        (0..self.count).map(|i| Oid::from_bytes(self.oid_at(i)).unwrap())
    }

    /// Parse the entry header at `offset`: (type, inflated size, offset of
    /// the delta base or the base's id, offset of the zlib data).
    fn entry(&self, offset: u64) -> Result<Entry> {
        let data = &self.pack[..];
        let mut p = offset as usize;
        ensure!(p < data.len(), "offset {offset} beyond the pack");
        let mut c = data[p];
        p += 1;
        let kind = (c >> 4) & 7;
        let mut size = (c & 15) as u64;
        let mut shift = 4;
        while c & 0x80 != 0 {
            c = *data.get(p).context("truncated entry header")?;
            p += 1;
            size |= ((c & 0x7f) as u64) << shift;
            shift += 7;
        }
        let base = match kind {
            OBJ_OFS_DELTA => {
                let mut c = *data.get(p).context("truncated delta offset")?;
                p += 1;
                let mut ofs = (c & 0x7f) as u64;
                while c & 0x80 != 0 {
                    c = *data.get(p).context("truncated delta offset")?;
                    p += 1;
                    ofs = ((ofs + 1) << 7) | (c & 0x7f) as u64;
                }
                ensure!(ofs <= offset && ofs > 0, "bad delta offset at {offset}");
                Base::Offset(offset - ofs)
            }
            OBJ_REF_DELTA => {
                let oid = Oid::from_bytes(data.get(p..p + 20).context("truncated delta base")?)?;
                p += 20;
                Base::Ref(oid)
            }
            1..=4 => Base::None,
            k => bail!("unknown pack entry type {k} at {offset}"),
        };
        Ok(Entry {
            kind,
            size: size as usize,
            base,
            data: p,
        })
    }

    fn inflate(&self, e: &Entry) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(e.size);
        let mut z = Decompress::new(true);
        let status = z.decompress_vec(&self.pack[e.data..], &mut out, FlushDecompress::Finish)?;
        ensure!(
            status == Status::StreamEnd && out.len() == e.size,
            "corrupt entry in {}",
            self.path.display()
        );
        Ok(out)
    }
}

enum Base {
    None,
    Offset(u64),
    Ref(Oid),
}

struct Entry {
    kind: u8,
    size: usize,
    base: Base,
    data: usize,
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn kind_of(t: u8) -> Result<Kind> {
    Ok(match t {
        1 => Kind::Commit,
        2 => Kind::Tree,
        3 => Kind::Blob,
        4 => Kind::Tag,
        _ => bail!("not a base object type: {t}"),
    })
}

/// Apply a git delta to `base`.
pub fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut p = 0;
    let varint = |p: &mut usize| -> Result<usize> {
        let (mut v, mut shift) = (0usize, 0);
        loop {
            let c = *delta.get(*p).context("truncated delta")?;
            *p += 1;
            v |= ((c & 0x7f) as usize) << shift;
            shift += 7;
            if c & 0x80 == 0 {
                return Ok(v);
            }
        }
    };
    let src = varint(&mut p)?;
    let dst = varint(&mut p)?;
    ensure!(src == base.len(), "delta base size mismatch");
    let mut out = Vec::with_capacity(dst);
    while p < delta.len() {
        let op = delta[p];
        p += 1;
        if op & 0x80 != 0 {
            let mut off = 0usize;
            let mut len = 0usize;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    off |= (*delta.get(p).context("truncated copy")? as usize) << (8 * i);
                    p += 1;
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    len |= (*delta.get(p).context("truncated copy")? as usize) << (8 * i);
                    p += 1;
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            out.extend_from_slice(base.get(off..off + len).context("copy out of range")?);
        } else if op != 0 {
            let n = op as usize;
            out.extend_from_slice(delta.get(p..p + n).context("truncated insert")?);
            p += n;
        } else {
            bail!("reserved delta opcode 0");
        }
    }
    ensure!(out.len() == dst, "delta result size mismatch");
    Ok(out)
}

/// Resolved objects that delta chains start from, shared by all readers.
/// Bounded by total bytes; the oldest entries go first.
/// (pack id, offset) -> resolved object.
type CacheKey = (usize, u64);

struct BaseCache {
    map: HashMap<CacheKey, (Kind, Arc<Vec<u8>>)>,
    order: VecDeque<CacheKey>,
    bytes: usize,
    limit: usize,
}

impl BaseCache {
    fn get(&self, key: CacheKey) -> Option<(Kind, Arc<Vec<u8>>)> {
        self.map.get(&key).cloned()
    }

    fn put(&mut self, key: CacheKey, kind: Kind, v: Arc<Vec<u8>>) {
        if v.len() > self.limit / 8 || self.map.contains_key(&key) {
            return;
        }
        self.bytes += v.len();
        self.map.insert(key, (kind, v));
        self.order.push_back(key);
        while self.bytes > self.limit {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some((_, v)) = self.map.remove(&old) {
                self.bytes -= v.len();
            }
        }
    }
}

/// All segments of a store.
pub struct Packs {
    dir: PathBuf,
    packs: std::sync::RwLock<Vec<Arc<Pack>>>,
    cache: Mutex<BaseCache>,
    /// When the directory was last listed. Another process sharing the
    /// store (another server) may add segments; a miss re-lists the
    /// directory, at most once per `RELIST`.
    listed: Mutex<std::time::Instant>,
}

const RELIST: std::time::Duration = std::time::Duration::from_millis(20);

impl Packs {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Packs> {
        let p = Packs {
            dir: dir.into(),
            packs: Default::default(),
            cache: Mutex::new(BaseCache {
                map: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
                limit: 256 << 20,
            }),
            listed: Mutex::new(std::time::Instant::now()),
        };
        p.refresh()?;
        Ok(p)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Pick up segments added since the last call (only those whose `.idx`
    /// exists, which is written last).
    pub fn refresh(&self) -> Result<usize> {
        *self.listed.lock().unwrap() = std::time::Instant::now();
        let mut found: Vec<PathBuf> = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "pack"))
                .filter(|p| p.with_extension("idx").exists())
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        found.sort();
        let mut packs = self.packs.write().unwrap();
        let known: std::collections::HashSet<PathBuf> =
            packs.iter().map(|p| p.path.clone()).collect();
        let mut added = 0;
        for f in found {
            if !known.contains(&f) {
                packs.push(Arc::new(Pack::open(&f)?));
                added += 1;
            }
        }
        // Newest first: recent objects are the ones most often asked for.
        if added > 0 {
            packs.sort_by_key(|p| {
                std::cmp::Reverse(std::fs::metadata(&p.path).and_then(|m| m.modified()).ok())
            });
        }
        Ok(added)
    }

    pub fn count(&self) -> usize {
        self.packs.read().unwrap().len()
    }

    fn locate_known(&self, oid: &Oid) -> Option<(Arc<Pack>, u64)> {
        let packs = self.packs.read().unwrap();
        packs
            .iter()
            .find_map(|p| p.find(oid).map(|o| (p.clone(), o)))
    }

    /// Find `oid`, re-listing the directory once if it is not in a known
    /// segment and the last listing is older than `RELIST`.
    fn locate(&self, oid: &Oid) -> Option<(Arc<Pack>, u64)> {
        if let Some(hit) = self.locate_known(oid) {
            return Some(hit);
        }
        let stale = self.listed.lock().unwrap().elapsed() >= RELIST;
        if stale && self.refresh().unwrap_or(0) > 0 {
            return self.locate_known(oid);
        }
        None
    }

    pub fn has(&self, oid: &Oid) -> bool {
        self.locate(oid).is_some()
    }

    /// The object, with its delta chain resolved.
    pub fn get(&self, oid: &Oid) -> Result<Option<(Kind, Vec<u8>)>> {
        let Some((pack, offset)) = self.locate(oid) else {
            return Ok(None);
        };
        let (kind, data) = self.resolve(&pack, offset)?;
        Ok(Some((
            kind,
            Arc::try_unwrap(data).unwrap_or_else(|a| (*a).clone()),
        )))
    }

    fn resolve(&self, pack: &Arc<Pack>, offset: u64) -> Result<(Kind, Arc<Vec<u8>>)> {
        // Walk down to a cached or undeltified base, then apply the deltas
        // back up. Bases referenced by id may live in another segment.
        let mut chain: Vec<(Arc<Pack>, Entry, u64)> = Vec::new();
        let (mut pack, mut offset) = (pack.clone(), offset);
        let (kind, mut data) = loop {
            ensure!(
                chain.len() < MAX_CHAIN,
                "delta chain too long in {}",
                pack.path.display()
            );
            if let Some(hit) = self.cache.lock().unwrap().get((pack.id, offset)) {
                break hit;
            }
            let e = pack.entry(offset)?;
            match e.base {
                Base::None => {
                    let k = kind_of(e.kind)?;
                    let d = Arc::new(pack.inflate(&e)?);
                    break (k, d);
                }
                Base::Offset(b) => {
                    chain.push((pack.clone(), e, offset));
                    offset = b;
                }
                Base::Ref(base) => {
                    chain.push((pack.clone(), e, offset));
                    (pack, offset) = self
                        .locate(&base)
                        .with_context(|| format!("delta base {base} is missing"))?;
                }
            }
        };
        if !chain.is_empty() {
            // The bottom of a chain is the most likely to be reused.
            self.cache
                .lock()
                .unwrap()
                .put((pack.id, offset), kind, data.clone());
        }
        while let Some((cpack, e, coff)) = chain.pop() {
            let delta = cpack.inflate(&e)?;
            data = Arc::new(apply_delta(&data, &delta)?);
            if !chain.is_empty() {
                self.cache
                    .lock()
                    .unwrap()
                    .put((cpack.id, coff), kind, data.clone());
            }
        }
        Ok((kind, data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn delta_ops() {
        let base = b"hello, world";
        // src 12, dst 11: copy 0..7 ("hello, "), insert "git!"
        let delta = [12, 11, 0x80 | 0x01 | 0x10, 0, 7, 4, b'g', b'i', b't', b'!'];
        assert_eq!(apply_delta(base, &delta).unwrap(), b"hello, git!");
        assert!(apply_delta(b"short", &delta).is_err());
    }

    /// Every object of a real, deltified git pack reads back with the id
    /// git gave it.
    #[test]
    fn reads_git_packs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };
        git(&["init", "-q"]);
        let mut text = String::new();
        for i in 0..60 {
            text.push_str(&format!("line {i} of a file that keeps growing\n"));
            std::fs::create_dir_all(repo.join("d")).unwrap();
            std::fs::write(repo.join("d/f.txt"), &text).unwrap();
            std::fs::write(repo.join(format!("d/n{}", i % 7)), format!("{i}\n")).unwrap();
            git(&["add", "-A"]);
            git(&[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@x",
                "commit",
                "-q",
                "-m",
                &format!("c{i}"),
            ]);
        }
        git(&["repack", "-adq", "--depth=50", "--window=50"]);
        let packs = Packs::open(repo.join(".git/objects/pack")).unwrap();
        assert_eq!(packs.count(), 1);
        let all = git(&[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ]);
        let mut n = 0;
        for line in all.lines() {
            let oid: Oid = line.parse().unwrap();
            let (kind, data) = packs.get(&oid).unwrap().expect("in the pack");
            assert_eq!(Oid::hash_object(kind.as_str(), &data), oid);
            n += 1;
        }
        assert!(n > 180, "{n}");
        // The pack really has deltas, so chains were exercised.
        let verify = git(&[
            "verify-pack",
            "-v",
            &format!(
                "{}",
                packs.packs.read().unwrap()[0]
                    .path()
                    .with_extension("idx")
                    .display()
            ),
        ]);
        assert!(verify.contains("chain length = "), "{verify}");
        assert!(packs
            .get(&Oid::hash_object("blob", b"absent"))
            .unwrap()
            .is_none());

        // A second segment arrives while the cache is warm; packs are kept
        // newest first, so positions shift. Reads across both stay correct.
        // (Cache keys once were positions. That bug needs two packs with a
        // cached entry at the same offset; the end-to-end test, with many
        // small fetched packs, is what caught it.)
        for i in 0..20 {
            text.push_str(&format!("more {i}\n"));
            std::fs::write(repo.join("d/f.txt"), &text).unwrap();
            git(&["add", "-A"]);
            git(&[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@x",
                "commit",
                "-q",
                "-m",
                &format!("m{i}"),
            ]);
        }
        std::thread::sleep(std::time::Duration::from_millis(1100)); // distinct mtime
        git(&["repack", "-dq"]); // packs only the new loose objects
        assert_eq!(packs.refresh().unwrap(), 1);
        assert_eq!(packs.count(), 2);
        let all = git(&[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ]);
        for _ in 0..2 {
            for line in all.lines() {
                let oid: Oid = line.parse().unwrap();
                let (kind, data) = packs.get(&oid).unwrap().expect("in a pack");
                assert_eq!(Oid::hash_object(kind.as_str(), &data), oid);
            }
        }
    }
}
