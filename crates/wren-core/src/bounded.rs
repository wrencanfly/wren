//! A node's object store with a size budget (`wren daemon`).
//!
//! Objects arrive loose and are sealed into segments; each segment is a
//! generation. When the store is over budget, the oldest generations go
//! whole: their hot objects are first copied forward (written loose again,
//! then sealed into a new generation), and the segment is deleted. Nothing
//! is deleted object by object, and nothing is ever rewritten in place.
//!
//! What is copied forward from a generation being dropped:
//! * every tree, commit and tag: only blobs are evicted, so tree walks and
//!   git's history reads keep working (trees are a small part of a store);
//! * blobs accessed since the generation was sealed (`touch`);
//! * blobs reachable from a lease: a checkout registers its root tree in
//!   `leases/`, and git reads that checkout's objects through alternates
//!   without telling anyone, so they must stay while the lease lasts. A
//!   lease is a file naming commits or trees; it expires when its
//!   modification time is older than the lease time.
//!
//! Maintenance holds an exclusive lock on `maintenance.lock` while it runs.
//! A checkout takes its lease *before* it looks at the store, then waits
//! for any maintenance already running (`lease`): every later run sees the
//! lease, so nothing the checkout found in the store disappears under it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use serde::Serialize;

use crate::object::Kind;
use crate::oid::Oid;
use crate::store::ObjectStore;

/// Directory, inside a store, of lease files.
pub const LEASES: &str = "leases";
/// File, inside a store, recording each segment's generation.
const MANIFEST: &str = "generations";
/// Held (exclusively) while maintenance runs.
const LOCK: &str = "maintenance.lock";

fn lock_file(store_root: &Path) -> Result<std::fs::File> {
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(store_root.join(LOCK))?)
}

pub struct BoundedStore {
    pub store: Arc<ObjectStore>,
    pub budget: u64,
    pub lease_time: Duration,
    state: Mutex<Gens>,
    /// Generation in which each object was last asked for.
    access: Mutex<HashMap<Oid, u64>>,
    maintaining: Mutex<()>,
}

struct Gens {
    /// The generation objects arriving now will be sealed into.
    current: u64,
    of: HashMap<String, u64>,
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct MaintainReport {
    pub sealed: usize,
    pub size_before: u64,
    pub size_after: u64,
    pub segments_dropped: usize,
    pub objects_dropped: usize,
    pub objects_kept: usize,
}

/// Register (or renew) a lease on `roots` (commits or trees) in the store
/// at `store_root`, under `name` (one file per checkout), then wait for any
/// maintenance already running. When this returns, no maintenance will
/// drop an object reachable from `roots` until the lease expires.
pub fn lease(store_root: &Path, name: &str, roots: &[Oid]) -> Result<()> {
    let dir = store_root.join(LEASES);
    std::fs::create_dir_all(&dir)?;
    let body: String = roots.iter().map(|r| format!("{r}\n")).collect();
    let tmp = dir.join(format!(".{name}.{}", std::process::id()));
    std::fs::write(&tmp, body)?;
    std::fs::rename(tmp, dir.join(name))?;
    let f = lock_file(store_root)?;
    f.lock()?;
    f.unlock()?;
    Ok(())
}

/// Give a lease back.
pub fn release(store_root: &Path, name: &str) {
    let _ = std::fs::remove_file(store_root.join(LEASES).join(name));
}

impl BoundedStore {
    pub fn open(store: Arc<ObjectStore>, budget: u64, lease_time: Duration) -> Result<Self> {
        let mut of = HashMap::new();
        if let Ok(s) = std::fs::read_to_string(store.root().join(MANIFEST)) {
            for line in s.lines() {
                if let Some((name, g)) = line.split_once(' ') {
                    if let Ok(g) = g.parse() {
                        of.insert(name.to_string(), g);
                    }
                }
            }
        }
        let current = of.values().max().map_or(1, |m| m + 1);
        Ok(BoundedStore {
            store,
            budget,
            lease_time,
            state: Mutex::new(Gens { current, of }),
            access: Mutex::new(HashMap::new()),
            maintaining: Mutex::new(()),
        })
    }

    /// Record that `oids` were asked for.
    pub fn touch(&self, oids: &[Oid]) {
        let g = self.state.lock().unwrap().current;
        let mut a = self.access.lock().unwrap();
        for o in oids {
            a.insert(*o, g);
        }
    }

    /// Bytes in segments plus loose objects.
    pub fn size(&self) -> u64 {
        let segments: u64 = self
            .store
            .packs()
            .local_segments()
            .iter()
            .map(|s| s.1)
            .sum();
        segments + loose_bytes(self.store.root())
    }

    fn save(&self, g: &Gens) -> Result<()> {
        let mut names: Vec<_> = g.of.iter().collect();
        names.sort();
        let body: String = names.iter().map(|(n, g)| format!("{n} {g}\n")).collect();
        let path = self.store.root().join(MANIFEST);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, body)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// Seal loose objects into the current generation and start the next.
    fn seal(&self) -> Result<usize> {
        let Some((name, n)) = self.store.seal()? else {
            return Ok(0);
        };
        let mut g = self.state.lock().unwrap();
        let cur = g.current;
        g.of.insert(name, cur);
        g.current += 1;
        self.save(&g)?;
        Ok(n)
    }

    /// Root trees under unexpired leases (expired lease files are removed).
    fn lease_roots(&self) -> Vec<Oid> {
        let dir = self.store.root().join(LEASES);
        let mut roots = Vec::new();
        let now = SystemTime::now();
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let age = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .unwrap_or_default();
            if age > self.lease_time {
                let _ = std::fs::remove_file(e.path());
                continue;
            }
            if let Ok(s) = std::fs::read_to_string(e.path()) {
                roots.extend(s.lines().filter_map(|l| l.trim().parse::<Oid>().ok()));
            }
        }
        roots
    }

    /// Blobs reachable from `roots` (commits or trees), as far as this
    /// store has the trees.
    fn reachable_blobs(&self, roots: &[Oid]) -> HashSet<Oid> {
        let mut blobs = HashSet::new();
        let mut seen = HashSet::new();
        let mut stack: Vec<Oid> = roots
            .iter()
            .map(|r| self.store.read_commit(r).map(|c| c.tree).unwrap_or(*r))
            .collect();
        while let Some(t) = stack.pop() {
            if !seen.insert(t) {
                continue;
            }
            let Ok(entries) = self.store.read_tree(&t) else {
                continue;
            };
            for e in entries {
                if e.is_tree() {
                    stack.push(e.oid);
                } else if !e.is_gitlink() {
                    blobs.insert(e.oid);
                }
            }
        }
        blobs
    }

    /// Seal what arrived, then, while over budget, drop the oldest
    /// generations (see the module docs). Safe to call concurrently with
    /// readers and writers of the store; calls are serialized.
    pub fn maintain(&self) -> Result<MaintainReport> {
        let _one = self.maintaining.lock().unwrap();
        // Checkouts wait on this (see `lease`); released when dropped.
        let lock = lock_file(self.store.root())?;
        lock.lock()?;
        let mut report = MaintainReport {
            sealed: self.seal()?,
            ..Default::default()
        };
        let mut size = self.size();
        report.size_before = size;
        if size <= self.budget {
            report.size_after = size;
            return Ok(report);
        }
        // Down to 90%, so that maintenance does not run on every request.
        let target = self.budget / 10 * 9;
        let mut roots = self.lease_roots();
        if let Ok(s) = std::fs::read_to_string(self.store.root().join("wren-last-root")) {
            roots.extend(s.trim().parse::<Oid>().ok());
        }
        let leased = self.reachable_blobs(&roots);
        let mut segments = self.store.packs().local_segments();
        {
            let g = self.state.lock().unwrap();
            // Oldest generation first; segments that are in no generation
            // (a snapshot, say) count as the oldest.
            segments.sort_by_key(|(name, _)| (g.of.get(name).copied().unwrap_or(0), name.clone()));
        }
        let mut dropped_any = false;
        for (name, seg_size) in segments.iter().take(segments.len().saturating_sub(1)) {
            if size <= target {
                break;
            }
            let gen = self
                .state
                .lock()
                .unwrap()
                .of
                .get(name)
                .copied()
                .unwrap_or(0);
            let oids = self.store.packs().segment_oids(name);
            let mut dropped = Vec::new();
            let mut kept_bytes = 0;
            for oid in &oids {
                let hot = self
                    .access
                    .lock()
                    .unwrap()
                    .get(oid)
                    .is_some_and(|&a| a > gen);
                let Some((kind, data)) = self.store.get(oid)? else {
                    continue;
                };
                if kind != Kind::Blob || hot || leased.contains(oid) {
                    let raw = crate::object::encode_loose(kind, &data);
                    kept_bytes += raw.len() as u64;
                    self.store.put_raw(oid, &raw)?;
                    report.objects_kept += 1;
                } else {
                    dropped.push(*oid);
                }
            }
            self.store.packs().remove(name)?;
            {
                let mut g = self.state.lock().unwrap();
                g.of.remove(name);
                self.save(&g)?;
            }
            {
                let mut a = self.access.lock().unwrap();
                for o in &dropped {
                    a.remove(o);
                }
            }
            report.objects_dropped += dropped.len();
            report.segments_dropped += 1;
            size = size.saturating_sub(*seg_size) + kept_bytes;
            dropped_any = true;
        }
        if dropped_any {
            // What was kept becomes the newest generation.
            self.seal()?;
        }
        report.size_after = self.size();
        Ok(report)
    }
}

fn loose_bytes(root: &Path) -> u64 {
    let mut n = 0;
    for fan in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let name = fan.file_name();
        if name.len() != 2 || !fan.path().is_dir() {
            continue;
        }
        for f in std::fs::read_dir(fan.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            n += f.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    n
}

/// Where a store's lease for a checkout at `dest` lives.
pub fn lease_name(dest: &Path) -> String {
    use sha1::{Digest, Sha1};
    let abs: PathBuf = std::path::absolute(dest).unwrap_or_else(|_| dest.to_path_buf());
    hex::encode(Sha1::digest(abs.to_string_lossy().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{encode_tree, TreeEntry, MODE_BLOB};

    fn blob(store: &ObjectStore, i: usize) -> Oid {
        // Incompressible, so that sizes are predictable.
        let mut x = 0x9e3779b97f4a7c15u64 ^ (i as u64 + 1);
        let data: Vec<u8> = (0..20_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        store.put(Kind::Blob, &data).unwrap()
    }

    fn tree(store: &ObjectStore, blobs: &[Oid]) -> Oid {
        let entries: Vec<TreeEntry> = blobs
            .iter()
            .enumerate()
            .map(|(i, b)| TreeEntry {
                mode: MODE_BLOB,
                name: format!("f{i:03}").into_bytes(),
                oid: *b,
            })
            .collect();
        store.put(Kind::Tree, &encode_tree(entries.iter())).unwrap()
    }

    #[test]
    fn evicts_cold_blobs_by_generation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(ObjectStore::open(dir.path()).unwrap());
        let b = BoundedStore::open(store.clone(), 1 << 40, Duration::from_secs(3600)).unwrap();

        // Generation 1: 30 blobs and a tree over the first 10 (leased).
        let g1: Vec<Oid> = (0..30).map(|i| blob(&store, i)).collect();
        let t1 = tree(&store, &g1[..10]);
        lease(dir.path(), "co1", &[t1]).unwrap();
        b.touch(&g1);
        assert_eq!(b.maintain().unwrap().sealed, 31);
        // Generation 2: 30 more blobs and a tree over them (not leased).
        let g2: Vec<Oid> = (30..60).map(|i| blob(&store, i)).collect();
        let t2 = tree(&store, &g2);
        b.touch(&g2);
        b.maintain().unwrap();
        // Blobs 20..25 of generation 1 are read again later: hot.
        b.touch(&g1[20..25]);
        // Generation 3: 30 more.
        let g3: Vec<Oid> = (60..90).map(|i| blob(&store, i)).collect();
        b.maintain().unwrap();
        assert_eq!(store.packs().local_segments().len(), 3);

        // A budget that one dropped generation (less what it keeps) meets.
        let full = b.size();
        let b = BoundedStore {
            budget: full * 19 / 20,
            ..b
        };
        let r = b.maintain().unwrap();
        assert_eq!(r.segments_dropped, 1, "{r:?}");
        assert!(r.size_after <= b.budget, "{r:?}");
        // Leased (0..10) and hot (20..25) blobs and the tree stay; the
        // other 15 blobs of generation 1 are gone.
        for (i, o) in g1.iter().enumerate() {
            let kept = i < 10 || (20..25).contains(&i);
            assert_eq!(store.has(o), kept, "blob {i}");
        }
        assert!(store.has(&t1) && store.has(&t2));
        assert!(g2.iter().chain(&g3).all(|o| store.has(o)));
        assert_eq!(r.objects_dropped, 15);

        // Generations survive a restart; an expired lease no longer
        // protects, and nothing was read since. Everything but the newest
        // generation goes: that is the one holding what was just copied
        // forward (blobs 0..10 and 20..25, and the trees).
        let b = BoundedStore::open(store.clone(), full / 4, Duration::from_secs(0)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let r = b.maintain().unwrap();
        assert_eq!(r.segments_dropped, 2, "{r:?}");
        assert!(!dir.path().join(LEASES).join("co1").exists());
        assert!(store.has(&t1) && store.has(&t2), "trees are never evicted");
        assert!(g2.iter().chain(&g3).all(|o| !store.has(o)));
        assert!(store.has(&g1[0]) && store.has(&g1[20]));
    }
}
