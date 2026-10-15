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
//!
//! With a bucket (see `segments`), segments other servers uploaded are read
//! in place: the `.idx` is downloaded whole, the `.pack` in blocks, on
//! demand, into a sparse local file. `hydrate` completes them into local
//! files when git needs to read them. `write_pack` turns objects this server
//! created into a segment of its own.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use flate2::{Decompress, FlushDecompress, Status};
use memmap2::Mmap;
use sha1::{Digest, Sha1};

use crate::object::Kind;
use crate::oid::Oid;
use crate::segments::{Remote, RemoteSegment};

const OBJ_OFS_DELTA: u8 = 6;
const OBJ_REF_DELTA: u8 = 7;
/// Delta chains in git packs are at most 50 deep by default; refuse cycles
/// and absurd chains in damaged packs.
const MAX_CHAIN: usize = 10_000;
/// Remote segments are read and cached locally in blocks of this size.
const BLOCK: u64 = 1 << 20;
/// At most this many blocks per ranged request.
const MAX_RUN: u64 = 16;

pub struct Pack {
    /// Unique per opened pack, for cache keys. (A pack's position in the
    /// list changes as segments are added, so it cannot be the key.)
    id: usize,
    /// `pack-<hash>`
    name: String,
    path: PathBuf,
    data: Data,
    idx: Mmap,
    count: usize,
}

enum Data {
    /// A complete local file.
    Local(Mmap),
    /// A segment in a bucket, read in blocks that are kept in a sparse
    /// local file.
    Remote(RemoteData),
}

struct RemoteData {
    remote: Arc<Remote>,
    file: File,
    len: u64,
    /// One bit per block: present in `file`.
    have: Mutex<Vec<u64>>,
    have_path: PathBuf,
    /// Offsets of every entry, sorted, then the trailer's: an entry ends
    /// where the next one starts, so it can be fetched exactly.
    ends: Vec<u64>,
}

static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn check_idx(idx: &[u8], what: &Path) -> Result<usize> {
    ensure!(
        idx.len() >= 8 + 1024 && idx[..8] == [0xff, b't', b'O', b'c', 0, 0, 0, 2],
        "{} is not a version 2 pack index",
        what.display()
    );
    let count = be32(idx, 8 + 255 * 4) as usize;
    ensure!(
        idx.len() >= 8 + 1024 + count * (20 + 4 + 4) + 40,
        "{} is truncated",
        what.display()
    );
    Ok(count)
}

fn map(path: &Path) -> Result<Mmap> {
    let f = File::open(path).with_context(|| format!("{}", path.display()))?;
    // SAFETY: segments are immutable once their .idx exists; nothing
    // writes to a mapped file.
    Ok(unsafe { Mmap::map(&f)? })
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl Pack {
    /// Open `pack-*.pack` and its `.idx`.
    pub fn open(pack_path: &Path) -> Result<Pack> {
        let pack = map(pack_path)?;
        let idx_path = pack_path.with_extension("idx");
        let idx = map(&idx_path)?;
        ensure!(
            pack.len() >= 32 && &pack[..4] == b"PACK",
            "{} is not a pack",
            pack_path.display()
        );
        let count = check_idx(&idx, &idx_path)?;
        Ok(Pack {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            name: stem(pack_path),
            path: pack_path.to_path_buf(),
            data: Data::Local(pack),
            idx,
            count,
        })
    }

    /// Open a segment in `remote`, caching it under `cache`: the `.idx` is
    /// downloaded whole (and checked), the `.pack` block by block on demand.
    fn open_remote(remote: &Arc<Remote>, cache: &Path, seg: &RemoteSegment) -> Result<Pack> {
        std::fs::create_dir_all(cache)?;
        let idx_path = cache.join(format!("{}.idx", seg.name));
        if !idx_path.exists() {
            let bytes = remote.get(&format!("{}.idx", seg.name))?;
            let n = bytes.len();
            ensure!(
                n >= 40 && Sha1::digest(&bytes[..n - 20])[..] == bytes[n - 20..],
                "{}.idx in {} is corrupt",
                seg.name,
                remote.url()
            );
            write_atomic(&idx_path, &bytes)?;
        }
        let idx = map(&idx_path)?;
        let count = check_idx(&idx, &idx_path)?;
        let path = cache.join(format!("{}.pack", seg.name));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        if file.metadata()?.len() != seg.size {
            file.set_len(seg.size)?; // sparse
        }
        let have_path = cache.join(format!("{}.have", seg.name));
        let blocks = seg.size.div_ceil(BLOCK) as usize;
        let mut have = vec![0u64; blocks.div_ceil(64)];
        if let Ok(b) = std::fs::read(&have_path) {
            if b.len() == have.len() * 8 {
                for (w, c) in have.iter_mut().zip(b.chunks(8)) {
                    *w = u64::from_le_bytes(c.try_into().unwrap());
                }
            }
        }
        let mut pack = Pack {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            name: seg.name.clone(),
            path,
            data: Data::Local(map(&idx_path)?), // replaced below
            idx,
            count,
        };
        let mut ends: Vec<u64> = (0..count).map(|i| pack.offset_at(i)).collect();
        ends.push(seg.size.saturating_sub(20));
        ends.sort_unstable();
        pack.data = Data::Remote(RemoteData {
            remote: remote.clone(),
            file,
            len: seg.size,
            have: Mutex::new(have),
            have_path,
            ends,
        });
        Ok(pack)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// Whether the pack is read from a bucket (not yet a complete local
    /// file).
    pub fn is_remote(&self) -> bool {
        matches!(self.data, Data::Remote(_))
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

    /// The bytes of the entry at `offset` (for a local pack, everything
    /// from there to the end).
    fn bytes(&self, offset: u64) -> Result<Cow<'_, [u8]>> {
        match &self.data {
            Data::Local(m) => {
                ensure!(offset < m.len() as u64, "offset {offset} beyond the pack");
                Ok(Cow::Borrowed(&m[offset as usize..]))
            }
            Data::Remote(r) => {
                let i = r.ends.partition_point(|&o| o <= offset);
                let end = *r
                    .ends
                    .get(i)
                    .with_context(|| format!("offset {offset} beyond {}", self.name))?;
                r.ensure(&self.name, offset..end)?;
                let mut buf = vec![0u8; (end - offset) as usize];
                read_at(&r.file, &mut buf, offset)?;
                Ok(Cow::Owned(buf))
            }
        }
    }

    /// Parse the entry at `offset`: (type, inflated size, the delta base,
    /// position of the zlib data in the returned bytes).
    fn entry(&self, offset: u64) -> Result<(Entry, Cow<'_, [u8]>)> {
        let data = self.bytes(offset)?;
        let mut p = 0;
        let mut c = *data.first().context("empty entry")?;
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
        Ok((
            Entry {
                kind,
                size: size as usize,
                base,
                data: p,
            },
            data,
        ))
    }

    fn inflate(&self, e: &Entry, bytes: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(e.size);
        let mut z = Decompress::new(true);
        let status = z.decompress_vec(&bytes[e.data..], &mut out, FlushDecompress::Finish)?;
        ensure!(
            status == Status::StreamEnd && out.len() == e.size,
            "corrupt entry in {}",
            self.path.display()
        );
        Ok(out)
    }

    /// Download every block of a remote pack and check its checksum
    /// against its trailer and its index.
    fn fetch_all(&self) -> Result<()> {
        let Data::Remote(r) = &self.data else {
            return Ok(());
        };
        r.ensure(&self.name, 0..r.len)?;
        let mut h = Sha1::new();
        let mut buf = vec![0u8; 1 << 20];
        let body = r.len - 20;
        let mut at = 0;
        while at < body {
            let n = (body - at).min(buf.len() as u64) as usize;
            read_at(&r.file, &mut buf[..n], at)?;
            h.update(&buf[..n]);
            at += n as u64;
        }
        let mut trailer = [0u8; 20];
        read_at(&r.file, &mut trailer, body)?;
        let idx_says = &self.idx[self.idx.len() - 40..self.idx.len() - 20];
        ensure!(
            h.finalize()[..] == trailer && trailer[..] == *idx_says,
            "{} from {} does not match its checksum",
            self.name,
            r.remote.url()
        );
        r.file.sync_all()?;
        Ok(())
    }
}

impl RemoteData {
    /// Make sure bytes `range` are in the local file.
    fn ensure(&self, name: &str, range: Range) -> Result<()> {
        if range.start >= range.end {
            return Ok(());
        }
        let first = range.start / BLOCK;
        let last = (range.end - 1) / BLOCK;
        let missing: Vec<u64> = {
            let have = self.have.lock().unwrap();
            (first..=last)
                .filter(|&b| have[(b / 64) as usize] & (1 << (b % 64)) == 0)
                .collect()
        };
        if missing.is_empty() {
            return Ok(());
        }
        // Contiguous runs of missing blocks, one ranged read each. Two
        // readers may fetch the same block; the bytes are the same.
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for b in missing {
            match runs.last_mut() {
                Some((s, e)) if *e == b && b - *s < MAX_RUN => *e = b + 1,
                _ => runs.push((b, b + 1)),
            }
        }
        let ranges: Vec<std::ops::Range<u64>> = runs
            .iter()
            .map(|&(s, e)| s * BLOCK..(e * BLOCK).min(self.len))
            .collect();
        let got = self
            .remote
            .get_ranges(&format!("{name}.pack"), ranges.clone())?;
        for (r, bytes) in ranges.iter().zip(&got) {
            ensure!(
                bytes.len() as u64 == r.end - r.start,
                "short read from {name}.pack"
            );
            write_at(&self.file, bytes, r.start)?;
        }
        self.file.sync_data()?;
        let snapshot = {
            let mut have = self.have.lock().unwrap();
            for &(s, e) in &runs {
                for b in s..e {
                    have[(b / 64) as usize] |= 1 << (b % 64);
                }
            }
            have.clone()
        };
        let bytes: Vec<u8> = snapshot.iter().flat_map(|w| w.to_le_bytes()).collect();
        write_atomic(&self.have_path, &bytes)?;
        Ok(())
    }
}

type Range = std::ops::Range<u64>;

#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], at: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;
    Ok(f.read_exact_at(buf, at)?)
}

#[cfg(unix)]
fn write_at(f: &File, buf: &[u8], at: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;
    Ok(f.write_all_at(buf, at)?)
}

static TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp_name(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    path.with_file_name(name)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_name(path);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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

fn type_of(k: Kind) -> u8 {
    match k {
        Kind::Commit => 1,
        Kind::Tree => 2,
        Kind::Blob => 3,
        Kind::Tag => 4,
    }
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

/// Write `objects` into `dir` as a new segment: an undeltified git pack and
/// its v2 index (written last). Returns the `.pack` path. Duplicate ids are
/// written once.
pub fn write_pack(dir: &Path, objects: &[(Oid, Kind, &[u8])]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut seen = HashSet::new();
    let objects: Vec<&(Oid, Kind, &[u8])> = objects.iter().filter(|o| seen.insert(o.0)).collect();
    let tmp_pack = tmp_name(&dir.join("pack"));
    let mut out = std::io::BufWriter::new(File::create(&tmp_pack)?);
    let mut sum = Sha1::new();
    let mut emit = |out: &mut std::io::BufWriter<File>, b: &[u8]| -> Result<()> {
        sum.update(b);
        out.write_all(b)?;
        Ok(())
    };
    let mut header = b"PACK\0\0\0\x02".to_vec();
    header.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    emit(&mut out, &header)?;
    let mut offset = header.len() as u64;
    // (oid, crc32, offset)
    let mut entries: Vec<(Oid, u32, u64)> = Vec::with_capacity(objects.len());
    for (oid, kind, data) in objects {
        let mut e = Vec::new();
        let mut size = data.len() as u64;
        let mut c = (type_of(*kind) << 4) | (size & 15) as u8;
        size >>= 4;
        while size > 0 {
            e.push(c | 0x80);
            c = (size & 0x7f) as u8;
            size >>= 7;
        }
        e.push(c);
        let mut z = flate2::write::ZlibEncoder::new(e, flate2::Compression::default());
        z.write_all(data)?;
        let e = z.finish()?;
        let mut crc = flate2::Crc::new();
        crc.update(&e);
        entries.push((*oid, crc.sum(), offset));
        emit(&mut out, &e)?;
        offset += e.len() as u64;
    }
    let checksum: [u8; 20] = sum.finalize().into();
    out.write_all(&checksum)?;
    out.into_inner()?.sync_all()?;

    entries.sort_by_key(|e| e.0);
    let mut idx = vec![0xff, b't', b'O', b'c', 0, 0, 0, 2];
    let mut fan = [0u32; 256];
    for e in &entries {
        for f in &mut fan[e.0.as_bytes()[0] as usize..] {
            *f += 1;
        }
    }
    for f in fan {
        idx.extend_from_slice(&f.to_be_bytes());
    }
    for e in &entries {
        idx.extend_from_slice(e.0.as_bytes());
    }
    for e in &entries {
        idx.extend_from_slice(&e.1.to_be_bytes());
    }
    let mut large = Vec::new();
    for e in &entries {
        if e.2 < 0x8000_0000 {
            idx.extend_from_slice(&(e.2 as u32).to_be_bytes());
        } else {
            idx.extend_from_slice(&(0x8000_0000 | (large.len() / 8) as u32).to_be_bytes());
            large.extend_from_slice(&e.2.to_be_bytes());
        }
    }
    idx.extend_from_slice(&large);
    idx.extend_from_slice(&checksum);
    let idx_sum: [u8; 20] = Sha1::digest(&idx).into();
    idx.extend_from_slice(&idx_sum);

    let pack = dir.join(format!("pack-{}.pack", hex::encode(checksum)));
    std::fs::rename(&tmp_pack, &pack)?;
    let tmp_idx = tmp_name(&pack.with_extension("idx"));
    std::fs::write(&tmp_idx, &idx)?;
    std::fs::rename(&tmp_idx, pack.with_extension("idx"))?;
    Ok(pack)
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

/// All segments of a store: complete local files in `dir`, and (with a
/// bucket) segments read from the bucket and cached in `dir/../remote`.
pub struct Packs {
    dir: PathBuf,
    remote: Option<(Arc<Remote>, PathBuf)>,
    packs: RwLock<Vec<Arc<Pack>>>,
    cache: Mutex<BaseCache>,
    /// When the last listing started. Another process (another server) may
    /// add segments at any time, so a lookup that misses lists again,
    /// unless a listing started after the lookup did; concurrent misses
    /// share one listing.
    listed: Mutex<Instant>,
}

impl Packs {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Packs> {
        Packs::open_with(dir, None)
    }

    /// Segments in `dir`, plus those in `remote` (cached in `cache`).
    pub fn open_with(
        dir: impl Into<PathBuf>,
        remote: Option<(Arc<Remote>, PathBuf)>,
    ) -> Result<Packs> {
        let p = Packs {
            dir: dir.into(),
            remote,
            packs: Default::default(),
            cache: Mutex::new(BaseCache {
                map: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
                limit: 256 << 20,
            }),
            listed: Mutex::new(Instant::now()),
        };
        p.refresh()?;
        Ok(p)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Pick up segments added since the last call: local ones whose `.idx`
    /// exists (it is written last), and complete ones in the bucket. A
    /// segment read from the bucket is replaced by its local file once
    /// there is one.
    pub fn refresh(&self) -> Result<usize> {
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
        // Oldest first, so that the newest end up in front.
        found.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        let remote_found = match &self.remote {
            Some((r, _)) => r.list()?,
            None => Vec::new(),
        };
        let local_names: HashSet<String> = found.iter().map(|p| stem(p)).collect();
        let mut packs = self.packs.write().unwrap();
        // Remote entries that are now local files.
        packs.retain(|p| !(p.is_remote() && local_names.contains(&p.name)));
        let known: HashSet<String> = packs.iter().map(|p| p.name.clone()).collect();
        let mut added = Vec::new();
        if let Some((remote, cache)) = &self.remote {
            for seg in &remote_found {
                if !known.contains(&seg.name) && !local_names.contains(&seg.name) {
                    added.push(Arc::new(Pack::open_remote(remote, cache, seg)?));
                }
            }
        }
        for f in found {
            if !known.contains(&stem(&f)) {
                added.push(Arc::new(Pack::open(&f)?));
            }
        }
        let n = added.len();
        // Newest first: recent objects are the ones most often asked for.
        added.reverse();
        added.extend(packs.drain(..));
        *packs = added;
        Ok(n)
    }

    /// List again unless a listing started after `since`.
    fn relist_after(&self, since: Instant) -> Result<()> {
        let mut last = self.listed.lock().unwrap();
        if *last > since {
            return Ok(());
        }
        let start = Instant::now();
        self.refresh()?;
        *last = start;
        Ok(())
    }

    pub fn count(&self) -> usize {
        self.packs.read().unwrap().len()
    }

    /// (segments read from the bucket, complete local segments)
    pub fn counts(&self) -> (usize, usize) {
        let packs = self.packs.read().unwrap();
        let remote = packs.iter().filter(|p| p.is_remote()).count();
        (remote, packs.len() - remote)
    }

    fn locate_known(&self, oid: &Oid) -> Option<(Arc<Pack>, u64)> {
        let packs = self.packs.read().unwrap();
        packs
            .iter()
            .find_map(|p| p.find(oid).map(|o| (p.clone(), o)))
    }

    /// Find `oid`; on a miss, list the segments again (see `listed`).
    fn locate(&self, oid: &Oid) -> Result<Option<(Arc<Pack>, u64)>> {
        let start = Instant::now();
        if let Some(hit) = self.locate_known(oid) {
            return Ok(Some(hit));
        }
        self.relist_after(start)?;
        Ok(self.locate_known(oid))
    }

    pub fn has(&self, oid: &Oid) -> bool {
        self.locate(oid).ok().flatten().is_some()
    }

    /// Whether a segment already known has `oid` (no listing).
    pub fn has_known(&self, oid: &Oid) -> bool {
        self.locate_known(oid).is_some()
    }

    /// The object, with its delta chain resolved.
    pub fn get(&self, oid: &Oid) -> Result<Option<(Kind, Vec<u8>)>> {
        let Some((pack, offset)) = self.locate(oid)? else {
            return Ok(None);
        };
        let (kind, data) = self.resolve(&pack, offset)?;
        Ok(Some((
            kind,
            Arc::try_unwrap(data).unwrap_or_else(|a| (*a).clone()),
        )))
    }

    fn resolve(&self, pack: &Arc<Pack>, offset: u64) -> Result<(Kind, Arc<Vec<u8>>)> {
        // Walk down to a cached or undeltified base, collecting the deltas,
        // then apply them back up. Bases referenced by id may live in
        // another segment.
        let mut chain: Vec<(usize, u64, Vec<u8>)> = Vec::new(); // (pack id, offset, delta)
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
            let (e, bytes) = pack.entry(offset)?;
            match e.base {
                Base::None => {
                    let k = kind_of(e.kind)?;
                    let d = Arc::new(pack.inflate(&e, &bytes)?);
                    break (k, d);
                }
                Base::Offset(b) => {
                    chain.push((pack.id, offset, pack.inflate(&e, &bytes)?));
                    offset = b;
                }
                Base::Ref(base) => {
                    chain.push((pack.id, offset, pack.inflate(&e, &bytes)?));
                    drop(bytes);
                    (pack, offset) = self
                        .locate(&base)?
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
        while let Some((cid, coff, delta)) = chain.pop() {
            data = Arc::new(apply_delta(&data, &delta)?);
            if !chain.is_empty() {
                self.cache
                    .lock()
                    .unwrap()
                    .put((cid, coff), kind, data.clone());
            }
        }
        Ok((kind, data))
    }

    /// Download every segment that is only in the bucket and make it a
    /// complete local file in `dir` (where git, through alternates, can
    /// read it). Returns the number of segments downloaded.
    pub fn hydrate(&self) -> Result<usize> {
        if self.remote.is_none() {
            return Ok(0);
        }
        self.relist_after(Instant::now())?;
        let remote: Vec<Arc<Pack>> = self
            .packs
            .read()
            .unwrap()
            .iter()
            .filter(|p| p.is_remote())
            .cloned()
            .collect();
        std::fs::create_dir_all(&self.dir)?;
        for p in &remote {
            p.fetch_all()?;
            let dest = self.dir.join(format!("{}.pack", p.name));
            // A hard link keeps the open cache file valid; the .idx goes
            // last, as for any segment.
            if !dest.exists() {
                let tmp = tmp_name(&dest);
                std::fs::hard_link(&p.path, &tmp)
                    .or_else(|_| std::fs::copy(&p.path, &tmp).map(|_| ()))?;
                std::fs::rename(&tmp, &dest)?;
            }
            let tmp = tmp_name(&dest.with_extension("idx"));
            std::fs::copy(p.path.with_extension("idx"), &tmp)?;
            std::fs::rename(&tmp, dest.with_extension("idx"))?;
            if let Data::Remote(r) = &p.data {
                let _ = std::fs::remove_file(&r.have_path);
            }
            let _ = std::fs::remove_file(&p.path);
            let _ = std::fs::remove_file(p.path.with_extension("idx"));
        }
        self.refresh()?;
        Ok(remote.len())
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
