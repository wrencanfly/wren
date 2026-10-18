//! `wren mount`: a virtualized checkout (M7). Linux through FUSE
//! (`fuse`), macOS through a localhost NFSv3 server mounted by the
//! system's own NFS client (`nfs`): no kernel extension, no root.
//!
//! The mount shows a commit's whole tree at once, but nothing is fetched
//! until it is read: a directory's tree when it is listed or looked up, a
//! file's blob when it is opened or stat'ed (its size is in the blob). Reads
//! go through the node's object store and, for what it lacks, the object
//! plane (`WREN_OBJECTS`, normally the node daemon), like any checkout.
//!
//! Writes go to an overlay directory, never to the store: a file is copied
//! up when it is opened for writing; new files, directories and symlinks
//! are created there; a deleted file or directory of the commit is recorded
//! as a whiteout. A directory deleted and created again hides what the
//! commit had in it (it is opaque). Renaming a directory that exists in the
//! commit returns `EXDEV`, so tools fall back to copy and delete.
//!
//! **git.** With `--git`, `.git` lives in the overlay and its index lists
//! every file of the commit, all marked valid for git's fsmonitor; the
//! mount journals every path it changes, and `wren fsmonitor-hook` answers
//! git's "what changed since token T" from that journal. So `git status`
//! looks only at what was written, instead of stat'ing (and fetching) every
//! file. The untracked cache does the same for directory scans. (git
//! treats FUSE and NFS as remote filesystems and turns fsmonitor off there
//! unless `fsmonitor.allowRemote` is set, which the mount does.)
//!
//! This module is the union view itself, independent of the protocol that
//! serves it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write as _;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use wren_core::object::{TreeEntry, MODE_EXEC, MODE_GITLINK, MODE_SYMLINK, MODE_TREE};
use wren_core::{ObjectStore, Oid};

use crate::client::Client;

#[cfg(target_os = "linux")]
pub mod fuse;
#[cfg(target_os = "macos")]
pub mod nfs;

/// Blob contents kept in memory for reads, at most this many bytes.
const BLOB_CACHE: usize = 256 << 20;

/// Files of the overlay directory.
pub fn upper_dir(overlay: &Path) -> PathBuf {
    overlay.join("files")
}
fn journal_path(overlay: &Path) -> PathBuf {
    overlay.join("journal")
}
fn whiteouts_path(overlay: &Path) -> PathBuf {
    overlay.join("whiteouts")
}
/// Where AppleDouble files (`._name`) live, outside the view's listings.
fn sidecar_dir(overlay: &Path) -> PathBuf {
    overlay.join("appledouble")
}

/// What a path is in the union view.
#[derive(Clone, Debug)]
enum Node {
    /// From the commit (not yet changed).
    Lower(TreeEntry),
    /// In the overlay.
    Upper(std::fs::Metadata),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
}

/// A path's attributes, for either protocol.
#[derive(Clone, Debug)]
pub struct Attr {
    pub ino: u64,
    pub kind: Kind,
    pub perm: u16,
    pub size: u64,
    pub mtime: SystemTime,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
}

/// A directory entry.
pub struct Entry {
    pub ino: u64,
    pub name: String,
    pub kind: Kind,
    /// Its attributes, when they are cheap: no blob is fetched for them.
    pub attr: Option<Attr>,
}

struct State {
    paths: Vec<String>,
    inos: HashMap<String, u64>,
    whiteouts: BTreeSet<String>,
    trees: HashMap<Oid, Arc<Vec<TreeEntry>>>,
    blobs: HashMap<Oid, Arc<Vec<u8>>>,
    blob_bytes: usize,
    /// Blob sizes from the server, so that stat needs no blob.
    sizes: HashMap<Oid, u64>,
    /// Directories whose sizes were asked for.
    sized: BTreeSet<String>,
    journal: std::fs::File,
    seq: u64,
}

/// A commit's tree with an overlay on top.
pub struct View {
    rt: tokio::runtime::Runtime,
    client: Client,
    store: Arc<ObjectStore>,
    commit: Oid,
    root: Oid,
    time: SystemTime,
    overlay: PathBuf,
    upper: PathBuf,
    uid: u32,
    gid: u32,
    state: Mutex<State>,
}

pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

pub fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

fn err(code: i32) -> anyhow::Error {
    std::io::Error::from_raw_os_error(code).into()
}

/// The errno behind an error (EIO when there is none).
pub fn errno_of(e: &anyhow::Error) -> i32 {
    e.downcast_ref::<std::io::Error>()
        .and_then(|io| io.raw_os_error().or_else(|| kind_errno(io.kind())))
        .unwrap_or(libc::EIO)
}

fn kind_errno(k: std::io::ErrorKind) -> Option<i32> {
    use std::io::ErrorKind::*;
    Some(match k {
        NotFound => libc::ENOENT,
        AlreadyExists => libc::EEXIST,
        PermissionDenied => libc::EACCES,
        _ => return None,
    })
}

impl View {
    pub fn new(
        client: Client,
        store: Arc<ObjectStore>,
        commit: Oid,
        root: Oid,
        time: SystemTime,
        overlay: PathBuf,
    ) -> Result<View> {
        let upper = upper_dir(&overlay);
        std::fs::create_dir_all(&upper)?;
        let whiteouts: BTreeSet<String> = std::fs::read_to_string(whiteouts_path(&overlay))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        let seq = current_seq(&overlay);
        let journal = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(journal_path(&overlay))?;
        // SAFETY: getuid/getgid cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Ok(View {
            rt: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?,
            client,
            store,
            commit,
            root,
            time,
            overlay,
            upper,
            uid,
            gid,
            state: Mutex::new(State {
                paths: vec![String::new()],
                inos: HashMap::from([(String::new(), 1)]),
                whiteouts,
                trees: HashMap::new(),
                blobs: HashMap::new(),
                blob_bytes: 0,
                sizes: HashMap::new(),
                sized: BTreeSet::new(),
                journal,
                seq,
            }),
        })
    }

    /// The inode number of `path` (the root is 1); stable for the mount's
    /// life, and kept by a renamed path.
    pub fn ino(&self, path: &str) -> u64 {
        let mut s = self.state.lock().unwrap();
        if let Some(&i) = s.inos.get(path) {
            return i;
        }
        s.paths.push(path.to_string());
        let i = s.paths.len() as u64;
        s.inos.insert(path.to_string(), i);
        i
    }

    pub fn path(&self, ino: u64) -> Option<String> {
        let s = self.state.lock().unwrap();
        s.paths.get((ino as usize).checked_sub(1)?).cloned()
    }

    fn ensure(&self, oid: &Oid) -> Result<()> {
        if self.store.has(oid) {
            return Ok(());
        }
        self.rt.block_on(self.client.ensure(&self.store, &[*oid]))
    }

    fn tree(&self, oid: &Oid) -> Result<Arc<Vec<TreeEntry>>> {
        if let Some(t) = self.state.lock().unwrap().trees.get(oid) {
            return Ok(t.clone());
        }
        self.ensure(oid)?;
        let t = Arc::new(self.store.read_tree(oid)?);
        self.state.lock().unwrap().trees.insert(*oid, t.clone());
        Ok(t)
    }

    fn blob(&self, oid: &Oid) -> Result<Arc<Vec<u8>>> {
        if let Some(b) = self.state.lock().unwrap().blobs.get(oid) {
            return Ok(b.clone());
        }
        self.ensure(oid)?;
        let b = Arc::new(self.store.read_blob(oid)?);
        let mut s = self.state.lock().unwrap();
        if s.blob_bytes + b.len() > BLOB_CACHE {
            s.blobs.clear();
            s.blob_bytes = 0;
        }
        s.blob_bytes += b.len();
        s.blobs.insert(*oid, b.clone());
        Ok(b)
    }

    /// The size of blob `oid` if known without fetching it.
    fn known_size(&self, oid: &Oid) -> Result<Option<u64>> {
        {
            let s = self.state.lock().unwrap();
            if let Some(b) = s.blobs.get(oid) {
                return Ok(Some(b.len() as u64));
            }
            if let Some(n) = s.sizes.get(oid) {
                return Ok(Some(*n));
            }
        }
        self.store.size(oid)
    }

    /// Ask the server for the sizes of the blobs in directory `dir` of the
    /// commit (once): one request instead of a blob per file. A server
    /// that does not give sizes leaves them unknown.
    fn load_sizes(&self, dir: &str) {
        if !self.state.lock().unwrap().sized.insert(dir.to_string()) {
            return;
        }
        let rev = self.commit.to_hex();
        let t: Result<wren_core::api::TreeResponse> = self.rt.block_on(
            self.client
                .get("tree", &[("rev", &rev), ("path", dir), ("sizes", "true")]),
        );
        match t {
            Ok(t) => {
                let mut s = self.state.lock().unwrap();
                for e in t.entries {
                    if let Some(n) = e.size {
                        s.sizes.insert(e.oid, n);
                    }
                }
            }
            Err(e) => tracing::debug!(dir, "no sizes from the server: {e:#}"),
        }
    }

    /// The commit's entry at `path` (the root is a tree entry with no name).
    fn lower(&self, path: &str) -> Result<Option<TreeEntry>> {
        let mut entry = TreeEntry {
            mode: MODE_TREE,
            name: Vec::new(),
            oid: self.root,
        };
        if path.is_empty() {
            return Ok(Some(entry));
        }
        for part in path.split('/') {
            if !entry.is_tree() {
                return Ok(None);
            }
            match self
                .tree(&entry.oid)?
                .iter()
                .find(|e| e.name == part.as_bytes())
            {
                Some(e) => entry = e.clone(),
                None => return Ok(None),
            }
        }
        Ok(Some(entry))
    }

    fn whited(&self, path: &str) -> bool {
        let s = self.state.lock().unwrap();
        let mut p = path;
        loop {
            if s.whiteouts.contains(p) {
                return true;
            }
            if p.is_empty() {
                return false;
            }
            p = parent_of(p);
        }
    }

    /// macOS keeps extended attributes on filesystems without them (NFS)
    /// in AppleDouble files, `._name` next to `name`. They are stored
    /// apart, never listed or journaled: the system finds them by name,
    /// and git and other tools never see them. (A `._` path the commit
    /// has is an ordinary file.)
    fn is_sidecar(&self, path: &str) -> bool {
        cfg!(target_os = "macos")
            && path.rsplit('/').next().is_some_and(|n| n.starts_with("._"))
            && !matches!(self.lower(path), Ok(Some(_)))
    }

    fn upper_path(&self, path: &str) -> PathBuf {
        if self.is_sidecar(path) {
            return sidecar_dir(&self.overlay).join(path);
        }
        self.upper.join(path)
    }

    /// Make sure the directory `path` goes in exists in the overlay.
    fn upper_parent(&self, path: &str) -> Result<()> {
        if let Some(p) = self.upper_path(path).parent() {
            std::fs::create_dir_all(p)?;
        }
        Ok(())
    }

    fn node(&self, path: &str) -> Result<Option<Node>> {
        if let Ok(m) = std::fs::symlink_metadata(self.upper_path(path)) {
            return Ok(Some(Node::Upper(m)));
        }
        if self.whited(path) {
            return Ok(None);
        }
        Ok(self.lower(path)?.map(Node::Lower))
    }

    fn kind(node: &Node) -> Kind {
        match node {
            // A submodule shows as an empty directory, as git checks it out
            // when it is not initialized.
            Node::Lower(e) if e.is_tree() || e.mode == MODE_GITLINK => Kind::Dir,
            Node::Lower(e) if e.mode == MODE_SYMLINK => Kind::Symlink,
            Node::Lower(_) => Kind::File,
            Node::Upper(m) if m.is_dir() => Kind::Dir,
            Node::Upper(m) if m.file_type().is_symlink() => Kind::Symlink,
            Node::Upper(_) => Kind::File,
        }
    }

    /// `node`'s attributes; `cheap`: not if that needs a blob fetched.
    fn attr_of(&self, path: &str, node: &Node, cheap: bool) -> Result<Option<Attr>> {
        let kind = Self::kind(node);
        let (perm, size, mtime, nlink) = match node {
            Node::Lower(_) if kind == Kind::Dir => (0o755, 4096, self.time, 2),
            Node::Lower(e) => {
                let mut size = self.known_size(&e.oid)?;
                if size.is_none() {
                    self.load_sizes(parent_of(path));
                    size = self.known_size(&e.oid)?;
                }
                let size = match size {
                    Some(n) => n,
                    None if cheap => return Ok(None),
                    None => self.blob(&e.oid)?.len() as u64,
                };
                let perm = match e.mode {
                    MODE_SYMLINK => 0o777,
                    MODE_EXEC => 0o755,
                    _ => 0o644,
                };
                (perm, size, self.time, 1)
            }
            Node::Upper(m) => (
                (m.mode() & 0o7777) as u16,
                m.len(),
                m.modified().unwrap_or(self.time),
                m.nlink() as u32,
            ),
        };
        Ok(Some(Attr {
            ino: self.ino(path),
            kind,
            perm,
            size,
            mtime,
            nlink,
            uid: self.uid,
            gid: self.gid,
        }))
    }

    /// `path`'s attributes, or `None` if there is no such path.
    pub fn stat(&self, path: &str) -> Result<Option<Attr>> {
        match self.node(path)? {
            Some(n) => self.attr_of(path, &n, false),
            None => Ok(None),
        }
    }

    fn stat_some(&self, path: &str) -> Result<Attr> {
        self.stat(path)?
            .ok_or_else(|| err(libc::ENOENT))
            .context("vanished")
    }

    /// The union listing of directory `path`: name -> node.
    fn list(&self, path: &str) -> Result<BTreeMap<String, Node>> {
        let mut out = BTreeMap::new();
        if !self.whited(path) {
            if let Some(e) = self.lower(path)?.filter(|e| e.is_tree()) {
                for c in self.tree(&e.oid)?.iter() {
                    let name = String::from_utf8_lossy(&c.name).into_owned();
                    let child = join(path, &name);
                    if !self.whited(&child) {
                        out.insert(name, Node::Lower(c.clone()));
                    }
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(self.upper_path(path)) {
            for e in rd.flatten() {
                if let Ok(m) = std::fs::symlink_metadata(e.path()) {
                    out.insert(e.file_name().to_string_lossy().into_owned(), Node::Upper(m));
                }
            }
        }
        Ok(out)
    }

    /// Directory `path`, in name order.
    pub fn readdir(&self, path: &str) -> Result<Vec<Entry>> {
        self.load_sizes(path);
        let mut out = Vec::new();
        for (name, node) in self.list(path)? {
            let child = join(path, &name);
            out.push(Entry {
                ino: self.ino(&child),
                kind: Self::kind(&node),
                attr: self.attr_of(&child, &node, true)?,
                name,
            });
        }
        Ok(out)
    }

    pub fn record(&self, paths: &[&str]) {
        let paths: Vec<&str> = paths
            .iter()
            .copied()
            .filter(|p| !self.is_sidecar(p))
            .collect();
        let mut s = self.state.lock().unwrap();
        for p in paths {
            if p == ".git" || p.starts_with(".git/") {
                continue;
            }
            s.seq += 1;
            let line = format!("{}\t{p}\n", s.seq);
            let _ = s.journal.write_all(line.as_bytes());
        }
        let _ = s.journal.flush();
    }

    fn save_whiteouts(&self, s: &State) -> Result<()> {
        let body: String = s.whiteouts.iter().map(|w| format!("{w}\n")).collect();
        let tmp = whiteouts_path(&self.overlay).with_extension("tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(tmp, whiteouts_path(&self.overlay))?;
        Ok(())
    }

    fn whiteout(&self, path: &str) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        s.whiteouts.insert(path.to_string());
        self.save_whiteouts(&s)
    }

    /// Make sure `dir` exists in the overlay (its ancestors too).
    fn upper_dirs(&self, dir: &str) -> Result<()> {
        std::fs::create_dir_all(self.upper_path(dir))?;
        Ok(())
    }

    /// Copy a file or symlink of the commit into the overlay.
    fn copy_up(&self, path: &str) -> Result<()> {
        if std::fs::symlink_metadata(self.upper_path(path)).is_ok() {
            return Ok(());
        }
        let Some(Node::Lower(e)) = self.node(path)? else {
            return Err(err(libc::ENOENT));
        };
        if e.is_tree() || e.mode == MODE_GITLINK {
            self.upper_dirs(path)?;
            return Ok(());
        }
        self.upper_parent(path)?;
        let data = self.blob(&e.oid)?;
        let dest = self.upper_path(path);
        if e.mode == MODE_SYMLINK {
            std::os::unix::fs::symlink(String::from_utf8_lossy(&data).as_ref(), &dest)?;
        } else {
            std::fs::write(&dest, &*data)?;
            let mode = if e.mode == MODE_EXEC { 0o755 } else { 0o644 };
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    }

    pub fn readlink(&self, path: &str) -> Result<Vec<u8>> {
        match self.node(path)? {
            Some(Node::Lower(e)) => Ok(self.blob(&e.oid)?.to_vec()),
            Some(Node::Upper(_)) => Ok(std::fs::read_link(self.upper_path(path))?
                .into_os_string()
                .into_encoded_bytes()),
            None => Err(err(libc::ENOENT)),
        }
    }

    /// Change mode, size or times (each optional).
    pub fn setattr(
        &self,
        path: &str,
        mode: Option<u32>,
        size: Option<u64>,
        atime: Option<SystemTime>,
        mtime: Option<SystemTime>,
    ) -> Result<Attr> {
        if mode.is_some() || size.is_some() || atime.is_some() || mtime.is_some() {
            match (size, self.node(path)?) {
                // Truncated to nothing: no need to fetch the old contents.
                (Some(0), Some(Node::Lower(e))) if !e.is_tree() && e.mode != MODE_SYMLINK => {
                    self.truncate_up(path, &e)?
                }
                _ => self.copy_up(path)?,
            }
            // Mode last: it may take away the write permission the others
            // need (NFS sends "mode 0444, times now" in one call).
            let p = self.upper_path(path);
            if let Some(sz) = size {
                open_for_write(&p, |f| Ok(f.set_len(sz)?))?;
            }
            if atime.is_some() || mtime.is_some() {
                let mut times = std::fs::FileTimes::new();
                if let Some(a) = atime {
                    times = times.set_accessed(a);
                }
                if let Some(m) = mtime {
                    times = times.set_modified(m);
                }
                // The owner may set times through any open file.
                std::fs::File::open(&p)?.set_times(times)?;
            }
            if let Some(m) = mode {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(m & 0o7777))?;
            }
            self.record(&[path]);
        }
        self.stat_some(path)
    }

    /// An empty copy of a file of the commit, with its mode.
    fn truncate_up(&self, path: &str, e: &TreeEntry) -> Result<()> {
        self.upper_parent(path)?;
        std::fs::write(self.upper_path(path), b"")?;
        let mode = if e.mode == MODE_EXEC { 0o755 } else { 0o644 };
        std::fs::set_permissions(self.upper_path(path), std::fs::Permissions::from_mode(mode))?;
        Ok(())
    }

    pub fn mkdir(&self, path: &str, mode: u32) -> Result<Attr> {
        if self.node(path)?.is_some() {
            return Err(err(libc::EEXIST));
        }
        self.upper_parent(path)?;
        std::fs::create_dir(self.upper_path(path))?;
        std::fs::set_permissions(
            self.upper_path(path),
            std::fs::Permissions::from_mode(mode & 0o7777),
        )?;
        self.record(&[path]);
        self.stat_some(path)
    }

    pub fn unlink(&self, path: &str) -> Result<()> {
        match self.node(path)? {
            None => return Err(err(libc::ENOENT)),
            Some(Node::Upper(m)) if m.is_dir() => return Err(err(libc::EISDIR)),
            Some(Node::Upper(_)) => std::fs::remove_file(self.upper_path(path))?,
            Some(Node::Lower(e)) if e.is_tree() || e.mode == MODE_GITLINK => {
                return Err(err(libc::EISDIR))
            }
            Some(Node::Lower(_)) => {}
        }
        if !self.whited(path) && self.lower(path)?.is_some() {
            self.whiteout(path)?;
        }
        self.record(&[path]);
        Ok(())
    }

    pub fn rmdir(&self, path: &str) -> Result<()> {
        match self.node(path)? {
            None => return Err(err(libc::ENOENT)),
            Some(n) if Self::kind(&n) != Kind::Dir => return Err(err(libc::ENOTDIR)),
            _ => {}
        }
        if !self.list(path)?.is_empty() {
            return Err(err(libc::ENOTEMPTY));
        }
        let _ = std::fs::remove_dir(self.upper_path(path));
        if !self.whited(path) && self.lower(path)?.is_some() {
            self.whiteout(path)?;
        }
        self.record(&[path]);
        Ok(())
    }

    /// `unlink` or `rmdir`, whichever `path` needs (NFS has one call).
    pub fn remove(&self, path: &str) -> Result<()> {
        match self.node(path)? {
            Some(n) if Self::kind(&n) == Kind::Dir => self.rmdir(path),
            _ => self.unlink(path),
        }
    }

    pub fn symlink(&self, path: &str, target: &Path) -> Result<Attr> {
        if self.node(path)?.is_some() {
            return Err(err(libc::EEXIST));
        }
        self.upper_parent(path)?;
        std::os::unix::fs::symlink(target, self.upper_path(path))?;
        self.record(&[path]);
        self.stat_some(path)
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let lower_from = !self.whited(from) && self.lower(from)?.is_some();
        match self.node(from)? {
            None => return Err(err(libc::ENOENT)),
            Some(Node::Lower(e)) if e.is_tree() => return Err(err(libc::EXDEV)),
            Some(Node::Upper(m)) if m.is_dir() && lower_from => return Err(err(libc::EXDEV)),
            _ => {}
        }
        self.copy_up(from)?;
        // A file of the commit being replaced: whiteout, then the move.
        if let Some(Node::Lower(e)) = self.node(to)? {
            if e.is_tree() {
                return Err(err(libc::EISDIR));
            }
            self.whiteout(to)?;
        }
        self.upper_parent(to)?;
        std::fs::rename(self.upper_path(from), self.upper_path(to))?;
        if lower_from {
            self.whiteout(from)?;
        }
        // The moved path keeps its inode; the old name gets a new one.
        {
            let mut s = self.state.lock().unwrap();
            if let Some(i) = s.inos.remove(from) {
                s.paths[i as usize - 1] = to.to_string();
                if let Some(old) = s.inos.insert(to.to_string(), i) {
                    s.paths[old as usize - 1] = format!("\0stale{old}");
                }
            }
        }
        self.record(&[from, to]);
        Ok(())
    }

    /// The file at `path`, opened in the overlay for writing (copied up
    /// first, or just emptied when `truncate`).
    pub fn open_write(&self, path: &str, truncate: bool) -> Result<std::fs::File> {
        let node = self.node(path)?.ok_or_else(|| err(libc::ENOENT))?;
        match (&node, truncate) {
            (Node::Lower(e), true) if !e.is_tree() && e.mode != MODE_SYMLINK => {
                self.truncate_up(path, e)?
            }
            _ => self.copy_up(path)?,
        }
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .truncate(truncate)
            .open(self.upper_path(path))?;
        self.record(&[path]);
        Ok(f)
    }

    /// The overlay file at `path` for reading, if it is in the overlay.
    pub fn open_upper(&self, path: &str) -> Result<Option<std::fs::File>> {
        match self.node(path)?.ok_or_else(|| err(libc::ENOENT))? {
            Node::Lower(_) => Ok(None),
            Node::Upper(_) => Ok(Some(std::fs::File::open(self.upper_path(path))?)),
        }
    }

    /// Create (or open, unless `exclusive`) a file; `mode` for a new one.
    pub fn create(
        &self,
        path: &str,
        mode: u32,
        exclusive: bool,
        truncate: bool,
    ) -> Result<std::fs::File> {
        let exists = self.node(path)?.is_some();
        if exists && exclusive {
            return Err(err(libc::EEXIST));
        }
        if exists {
            return self.open_write(path, truncate);
        }
        self.upper_parent(path)?;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(truncate)
            .open(self.upper_path(path))?;
        f.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777))?;
        self.record(&[path]);
        Ok(f)
    }

    /// Up to `size` bytes of `path` at `offset`, and whether that is the
    /// end of the file.
    pub fn read(&self, path: &str, offset: u64, size: u32) -> Result<(Vec<u8>, bool)> {
        match self.node(path)?.ok_or_else(|| err(libc::ENOENT))? {
            Node::Lower(e) => {
                let b = self.blob(&e.oid)?;
                let start = (offset as usize).min(b.len());
                let end = (start + size as usize).min(b.len());
                Ok((b[start..end].to_vec(), end == b.len()))
            }
            Node::Upper(m) => {
                let f = std::fs::File::open(self.upper_path(path))?;
                Ok(read_at(&f, offset, size, m.len())?)
            }
        }
    }

    /// Write `data` at `offset`, with no open file (NFS): the file is
    /// copied up first if it is the commit's.
    pub fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<Attr> {
        self.copy_up(path)?;
        open_for_write(
            &self.upper_path(path),
            |f| Ok(f.write_all_at(data, offset)?),
        )?;
        self.record(&[path]);
        self.stat_some(path)
    }
}

/// Run `f` on `p` opened for writing, even if its mode forbids it: a
/// stateless write (NFS) to a file created read-only (as git creates its
/// objects) comes after the create, with no open file to write through.
fn open_for_write(p: &Path, f: impl FnOnce(&std::fs::File) -> Result<()>) -> Result<()> {
    match std::fs::OpenOptions::new().write(true).open(p) {
        Ok(file) => f(&file),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let mode = std::fs::metadata(p)?.permissions().mode();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode | 0o200))?;
            let r = std::fs::OpenOptions::new()
                .write(true)
                .open(p)
                .map_err(anyhow::Error::from)
                .and_then(|file| f(&file));
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))?;
            r
        }
        Err(e) => Err(e.into()),
    }
}

/// Up to `size` bytes of `f` at `offset` (a file of length `len`), and
/// whether that reaches its end.
pub fn read_at(f: &std::fs::File, offset: u64, size: u32, len: u64) -> Result<(Vec<u8>, bool)> {
    let mut buf = vec![0u8; size as usize];
    let mut n = 0;
    while n < buf.len() {
        match f.read_at(&mut buf[n..], offset + n as u64)? {
            0 => break,
            k => n += k,
        }
    }
    buf.truncate(n);
    Ok((buf, offset + n as u64 >= len))
}

/// The journal's last sequence number.
fn current_seq(overlay: &Path) -> u64 {
    std::fs::read_to_string(journal_path(overlay))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split('\t').next()?.parse().ok())
        .max()
        .unwrap_or(0)
}

/// Fetch every tree of `root` (no blobs), for a full git index: the
/// files, and every directory with its tree id.
#[allow(clippy::type_complexity)]
async fn all_trees(
    client: &Client,
    store: &Arc<ObjectStore>,
    root: Oid,
) -> Result<(Vec<(String, TreeEntry)>, Vec<(String, Oid)>)> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    let mut level = vec![(String::new(), root)];
    while !level.is_empty() {
        let oids: Vec<Oid> = level.iter().map(|l| l.1).collect();
        client.ensure(store, &oids).await?;
        let mut next = Vec::new();
        for (dir, t) in level {
            dirs.push((dir.clone(), t));
            for e in store.read_tree(&t)? {
                let p = join(&dir, &String::from_utf8_lossy(&e.name));
                if e.is_tree() {
                    next.push((p, e.oid));
                } else {
                    files.push((p, e));
                }
            }
        }
        level = next;
    }
    Ok((files, dirs))
}

pub struct GitOptions {
    pub branch: Option<String>,
    /// The promisor remote (Wren's git front end).
    pub origin: String,
}

/// Write `.git` into the overlay: refs, config (fsmonitor hook, untracked
/// cache, promisor remote; no automatic gc or maintenance, which would read
/// every file and, detached in the background, keep the mount busy),
/// alternates to the node store, and a full index marked valid for
/// fsmonitor.
pub async fn setup_git(
    client: &Client,
    store: &Arc<ObjectStore>,
    commit: Oid,
    root: Oid,
    overlay: &Path,
    g: &GitOptions,
) -> Result<()> {
    let git = upper_dir(overlay).join(".git");
    if git.join("HEAD").exists() {
        return Ok(());
    }
    for d in [
        "objects/info",
        "objects/pack",
        "refs/heads",
        "refs/tags",
        "info",
    ] {
        std::fs::create_dir_all(git.join(d))?;
    }
    std::fs::write(
        git.join("objects/info/alternates"),
        format!("{}\n", store.root().canonicalize()?.display()),
    )?;
    std::fs::write(git.join("shallow"), format!("{commit}\n"))?;
    match &g.branch {
        Some(b) => {
            std::fs::write(git.join("HEAD"), format!("ref: refs/heads/{b}\n"))?;
            let r = git.join("refs/heads").join(b);
            std::fs::create_dir_all(r.parent().unwrap())?;
            std::fs::write(r, format!("{commit}\n"))?;
        }
        None => std::fs::write(git.join("HEAD"), format!("{commit}\n"))?,
    }
    let exe = std::env::current_exe()?;
    let hook = format!(
        "'{}' fsmonitor-hook '{}'",
        exe.display(),
        overlay.canonicalize()?.display()
    );
    let mut config = format!(
        "[core]\n\trepositoryformatversion = 1\n\tfilemode = true\n\tbare = false\n\
         \tfsmonitor = \"{}\"\n\tuntrackedCache = true\n\
         [fsmonitor]\n\tallowRemote = true\n\
         [gc]\n\tauto = 0\n[maintenance]\n\tauto = false\n\
         [extensions]\n\tpartialclone = origin\n\
         [remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n\
         \tpromisor = true\n\tpartialclonefilter = blob:none\n",
        hook.replace('"', "\\\""),
        g.origin
    );
    if let Some(b) = &g.branch {
        config.push_str(&format!(
            "[branch \"{b}\"]\n\tremote = origin\n\tmerge = refs/heads/{b}\n"
        ));
    }
    std::fs::write(git.join("config"), config)?;
    std::fs::write(git.join("description"), "Wren mount\n")?;
    let (files, dirs) = all_trees(client, store, root).await?;
    let mut entries: Vec<wren_core::index::IndexEntry> = files
        .into_iter()
        .map(|(p, e)| wren_core::index::IndexEntry {
            path: p.into_bytes(),
            mode: e.mode,
            oid: e.oid,
            stat: None,
            skip_worktree: false,
        })
        .collect();
    let token = format!("wren:{}", current_seq(overlay));
    let trees: Vec<wren_core::index::CachedTree> = dirs
        .into_iter()
        .map(|(path, oid)| wren_core::index::CachedTree { path, oid })
        .collect();
    wren_core::index::write_index_with(&git.join("index"), &mut entries, Some(&token), &trees)?;
    Ok(())
}

/// `wren fsmonitor-hook OVERLAY VERSION TOKEN`: git's fsmonitor hook
/// (protocol 2). Prints the new token, NUL, then every path changed since
/// `token`, NUL-separated. An unknown token means "everything".
pub fn fsmonitor_hook(overlay: &Path, version: &str, token: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(
        version == "2",
        "only fsmonitor hook protocol 2 is supported"
    );
    let journal = std::fs::read_to_string(journal_path(overlay)).unwrap_or_default();
    let entries: Vec<(u64, &str)> = journal
        .lines()
        .filter_map(|l| {
            let (s, p) = l.split_once('\t')?;
            Some((s.parse().ok()?, p))
        })
        .collect();
    let last = entries.iter().map(|e| e.0).max().unwrap_or(0);
    let mut out = format!("wren:{last}").into_bytes();
    out.push(0);
    match token
        .strip_prefix("wren:")
        .and_then(|t| t.parse::<u64>().ok())
    {
        Some(since) if since <= last => {
            let mut seen = BTreeSet::new();
            for (s, p) in entries {
                if s > since && seen.insert(p) {
                    out.extend(p.as_bytes());
                    out.push(0);
                }
            }
        }
        _ => {
            out.extend(b"/");
            out.push(0);
        }
    }
    Ok(out)
}

/// When the commit was made, for the times the mount reports.
pub fn commit_time(seconds: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds.max(0) as u64)
}
