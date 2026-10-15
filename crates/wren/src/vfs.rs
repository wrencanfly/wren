//! `wren mount`: a virtualized checkout (M7, Linux, FUSE).
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
//! treats FUSE as a remote filesystem and turns fsmonitor off there unless
//! `fsmonitor.allowRemote` is set, which the mount does.)

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::io::Write as _;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    MountOption, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use wren_core::object::{TreeEntry, MODE_EXEC, MODE_GITLINK, MODE_SYMLINK, MODE_TREE};
use wren_core::{ObjectStore, Oid};

use crate::client::Client;

const TTL: Duration = Duration::from_secs(1);
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

/// What a path is in the union view.
#[derive(Clone, Debug)]
enum Node {
    /// From the commit (not yet changed).
    Lower(TreeEntry),
    /// In the overlay.
    Upper(std::fs::Metadata),
}

struct State {
    paths: Vec<String>,
    inos: HashMap<String, u64>,
    whiteouts: BTreeSet<String>,
    trees: HashMap<Oid, Arc<Vec<TreeEntry>>>,
    blobs: HashMap<Oid, Arc<Vec<u8>>>,
    blob_bytes: usize,
    handles: HashMap<u64, Arc<std::fs::File>>,
    next_fh: u64,
    journal: std::fs::File,
    seq: u64,
}

pub struct WrenFs {
    rt: tokio::runtime::Runtime,
    client: Client,
    store: Arc<ObjectStore>,
    root: Oid,
    time: SystemTime,
    overlay: PathBuf,
    upper: PathBuf,
    uid: u32,
    gid: u32,
    state: Mutex<State>,
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

fn errno(e: &anyhow::Error) -> Errno {
    match e.downcast_ref::<std::io::Error>() {
        Some(io) => Errno::from(std::io::Error::new(io.kind(), io.to_string())),
        None => Errno::EIO,
    }
}

impl WrenFs {
    pub fn new(
        client: Client,
        store: Arc<ObjectStore>,
        root: Oid,
        time: SystemTime,
        overlay: PathBuf,
    ) -> Result<WrenFs> {
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
        Ok(WrenFs {
            rt: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?,
            client,
            store,
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
                handles: HashMap::new(),
                next_fh: 1,
                journal,
                seq,
            }),
        })
    }

    fn ino(&self, path: &str) -> u64 {
        let mut s = self.state.lock().unwrap();
        if let Some(&i) = s.inos.get(path) {
            return i;
        }
        s.paths.push(path.to_string());
        let i = s.paths.len() as u64;
        s.inos.insert(path.to_string(), i);
        i
    }

    fn path(&self, ino: INodeNo) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .paths
            .get(ino.0 as usize - 1)
            .cloned()
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

    fn upper_path(&self, path: &str) -> PathBuf {
        self.upper.join(path)
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

    fn attr(&self, path: &str, node: &Node) -> Result<FileAttr> {
        let ino = INodeNo(self.ino(path));
        let (kind, perm, size, mtime, nlink) = match node {
            // A submodule shows as an empty directory, as git checks it out
            // when it is not initialized.
            Node::Lower(e) if e.is_tree() || e.mode == MODE_GITLINK => {
                (FileType::Directory, 0o755, 4096, self.time, 2)
            }
            Node::Lower(e) => {
                let size = self.blob(&e.oid)?.len() as u64;
                if e.mode == MODE_SYMLINK {
                    (FileType::Symlink, 0o777, size, self.time, 1)
                } else if e.mode == MODE_EXEC {
                    (FileType::RegularFile, 0o755, size, self.time, 1)
                } else {
                    (FileType::RegularFile, 0o644, size, self.time, 1)
                }
            }
            Node::Upper(m) => (
                FileType::from_std(m.file_type()).unwrap_or(FileType::RegularFile),
                (m.mode() & 0o7777) as u16,
                m.len(),
                m.modified().unwrap_or(self.time),
                m.nlink() as u32,
            ),
        };
        Ok(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
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

    fn record(&self, paths: &[&str]) {
        let mut s = self.state.lock().unwrap();
        for p in paths {
            if *p == ".git" || p.starts_with(".git/") {
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
            anyhow::bail!(std::io::Error::from(std::io::ErrorKind::NotFound));
        };
        if e.is_tree() || e.mode == MODE_GITLINK {
            self.upper_dirs(path)?;
            return Ok(());
        }
        self.upper_dirs(parent_of(path))?;
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

    fn entry_reply(&self, path: &str, reply: ReplyEntry) {
        match self.node(path) {
            Ok(Some(n)) => match self.attr(path, &n) {
                Ok(a) => reply.entry(&TTL, &a, Generation(0)),
                Err(e) => reply.error(errno(&e)),
            },
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn handle(&self, file: std::fs::File) -> u64 {
        let mut s = self.state.lock().unwrap();
        let fh = s.next_fh;
        s.next_fh += 1;
        s.handles.insert(fh, Arc::new(file));
        fh
    }

    fn get_handle(&self, fh: FileHandle) -> Option<Arc<std::fs::File>> {
        self.state.lock().unwrap().handles.get(&fh.0).cloned()
    }
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

impl Filesystem for WrenFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(dir) = self.path(parent) else {
            return reply.error(Errno::ENOENT);
        };
        let path = join(&dir, &name.to_string_lossy());
        self.entry_reply(&path, reply);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(path) = self.path(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match self.node(&path) {
            Ok(Some(n)) => match self.attr(&path, &n) {
                Ok(a) => reply.attr(&TTL, &a),
                Err(e) => reply.error(errno(&e)),
            },
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Some(path) = self.path(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let run = || -> Result<()> {
            if mode.is_none() && size.is_none() && atime.is_none() && mtime.is_none() {
                return Ok(());
            }
            self.copy_up(&path)?;
            let p = self.upper_path(&path);
            if let Some(m) = mode {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(m & 0o7777))?;
            }
            if let Some(sz) = size {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&p)?
                    .set_len(sz)?;
            }
            if atime.is_some() || mtime.is_some() {
                let t = |v: Option<TimeOrNow>| match v {
                    Some(TimeOrNow::SpecificTime(t)) => Some(t),
                    Some(TimeOrNow::Now) => Some(SystemTime::now()),
                    None => None,
                };
                let mut times = std::fs::FileTimes::new();
                if let Some(a) = t(atime) {
                    times = times.set_accessed(a);
                }
                if let Some(m) = t(mtime) {
                    times = times.set_modified(m);
                }
                std::fs::File::options()
                    .write(true)
                    .open(&p)?
                    .set_times(times)?;
            }
            self.record(&[&path]);
            Ok(())
        };
        match run().and_then(|_| {
            let n = self.node(&path)?.context("vanished")?;
            self.attr(&path, &n)
        }) {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some(path) = self.path(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match self.node(&path) {
            Ok(Some(Node::Lower(e))) => match self.blob(&e.oid) {
                Ok(b) => reply.data(&b),
                Err(e) => reply.error(errno(&e)),
            },
            Ok(Some(Node::Upper(_))) => match std::fs::read_link(self.upper_path(&path)) {
                Ok(t) => reply.data(t.as_os_str().as_encoded_bytes()),
                Err(e) => reply.error(e.into()),
            },
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(dir) = self.path(parent) else {
            return reply.error(Errno::ENOENT);
        };
        let path = join(&dir, &name.to_string_lossy());
        let run = || -> Result<()> {
            if self.node(&path)?.is_some() {
                anyhow::bail!(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            self.upper_dirs(&dir)?;
            std::fs::create_dir(self.upper_path(&path))?;
            std::fs::set_permissions(
                self.upper_path(&path),
                std::fs::Permissions::from_mode(mode & !umask & 0o7777),
            )?;
            self.record(&[&path]);
            Ok(())
        };
        match run() {
            Ok(()) => self.entry_reply(&path, reply),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(dir) = self.path(parent) else {
            return reply.error(Errno::ENOENT);
        };
        let path = join(&dir, &name.to_string_lossy());
        let run = || -> Result<()> {
            match self.node(&path)? {
                None => anyhow::bail!(std::io::Error::from(std::io::ErrorKind::NotFound)),
                Some(Node::Upper(_)) => std::fs::remove_file(self.upper_path(&path))?,
                Some(Node::Lower(e)) if e.is_tree() || e.mode == MODE_GITLINK => {
                    anyhow::bail!(std::io::Error::from_raw_os_error(libc::EISDIR))
                }
                Some(Node::Lower(_)) => {}
            }
            if !self.whited(&path) && self.lower(&path)?.is_some() {
                self.whiteout(&path)?;
            }
            self.record(&[&path]);
            Ok(())
        };
        match run() {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(dir) = self.path(parent) else {
            return reply.error(Errno::ENOENT);
        };
        let path = join(&dir, &name.to_string_lossy());
        let run = || -> Result<()> {
            if self.node(&path)?.is_none() {
                anyhow::bail!(std::io::Error::from(std::io::ErrorKind::NotFound));
            }
            if !self.list(&path)?.is_empty() {
                anyhow::bail!(std::io::Error::from_raw_os_error(libc::ENOTEMPTY));
            }
            let _ = std::fs::remove_dir(self.upper_path(&path));
            if !self.whited(&path) && self.lower(&path)?.is_some() {
                self.whiteout(&path)?;
            }
            self.record(&[&path]);
            Ok(())
        };
        match run() {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let Some(dir) = self.path(parent) else {
            return reply.error(Errno::ENOENT);
        };
        let path = join(&dir, &link_name.to_string_lossy());
        let run = || -> Result<()> {
            if self.node(&path)?.is_some() {
                anyhow::bail!(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            self.upper_dirs(&dir)?;
            std::os::unix::fs::symlink(target, self.upper_path(&path))?;
            self.record(&[&path]);
            Ok(())
        };
        match run() {
            Ok(()) => self.entry_reply(&path, reply),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (Some(dir), Some(newdir)) = (self.path(parent), self.path(newparent)) else {
            return reply.error(Errno::ENOENT);
        };
        let from = join(&dir, &name.to_string_lossy());
        let to = join(&newdir, &newname.to_string_lossy());
        let run = || -> Result<()> {
            let lower_from = !self.whited(&from) && self.lower(&from)?.is_some();
            match self.node(&from)? {
                None => anyhow::bail!(std::io::Error::from(std::io::ErrorKind::NotFound)),
                Some(Node::Lower(e)) if e.is_tree() => {
                    anyhow::bail!(std::io::Error::from_raw_os_error(libc::EXDEV))
                }
                Some(Node::Upper(m)) if m.is_dir() && lower_from => {
                    anyhow::bail!(std::io::Error::from_raw_os_error(libc::EXDEV))
                }
                _ => {}
            }
            self.copy_up(&from)?;
            // A file of the commit being replaced: whiteout, then the move.
            if let Some(Node::Lower(e)) = self.node(&to)? {
                if e.is_tree() {
                    anyhow::bail!(std::io::Error::from_raw_os_error(libc::EISDIR));
                }
                self.whiteout(&to)?;
            }
            self.upper_dirs(&newdir)?;
            std::fs::rename(self.upper_path(&from), self.upper_path(&to))?;
            if lower_from {
                self.whiteout(&from)?;
            }
            // The moved path keeps its inode; the old name gets a new one.
            {
                let mut s = self.state.lock().unwrap();
                if let Some(i) = s.inos.remove(&from) {
                    s.paths[i as usize - 1] = to.clone();
                    if let Some(old) = s.inos.insert(to.clone(), i) {
                        s.paths[old as usize - 1] = format!("\0stale{old}");
                    }
                }
            }
            self.record(&[&from, &to]);
            Ok(())
        };
        match run() {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    /// No extended attributes.
    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: fuser::ReplyXattr,
    ) {
        reply.error(Errno::NO_XATTR);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: fuser::ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    /// Hard links are not supported (git falls back to renaming).
    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EPERM);
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some(path) = self.path(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let write = flags.acc_mode() != OpenAccMode::O_RDONLY || flags.0 & libc::O_TRUNC != 0;
        let run = || -> Result<u64> {
            let node = self.node(&path)?.context("no such file")?;
            if !write {
                if let Node::Lower(_) = node {
                    return Ok(0);
                }
                return Ok(self.handle(std::fs::File::open(self.upper_path(&path))?));
            }
            let truncate = flags.0 & libc::O_TRUNC != 0;
            match (&node, truncate) {
                // Truncated anyway: no need to fetch the old contents.
                (Node::Lower(e), true) if !e.is_tree() && e.mode != MODE_SYMLINK => {
                    self.upper_dirs(parent_of(&path))?;
                    std::fs::write(self.upper_path(&path), b"")?;
                    let mode = if e.mode == MODE_EXEC { 0o755 } else { 0o644 };
                    std::fs::set_permissions(
                        self.upper_path(&path),
                        std::fs::Permissions::from_mode(mode),
                    )?;
                }
                _ => self.copy_up(&path)?,
            }
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .truncate(truncate)
                .open(self.upper_path(&path))?;
            self.record(&[&path]);
            Ok(self.handle(f))
        };
        match run() {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        if fh.0 != 0 {
            let Some(f) = self.get_handle(fh) else {
                return reply.error(Errno::EBADF);
            };
            let mut buf = vec![0u8; size as usize];
            let mut n = 0;
            while n < buf.len() {
                match f.read_at(&mut buf[n..], offset + n as u64) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(e) => return reply.error(e.into()),
                }
            }
            return reply.data(&buf[..n]);
        }
        let Some(path) = self.path(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match self.node(&path) {
            Ok(Some(Node::Lower(e))) => match self.blob(&e.oid) {
                Ok(b) => {
                    let start = (offset as usize).min(b.len());
                    let end = (start + size as usize).min(b.len());
                    reply.data(&b[start..end])
                }
                Err(e) => reply.error(errno(&e)),
            },
            Ok(Some(Node::Upper(_))) => {
                // Opened before a copy-up by someone else: read the overlay.
                match std::fs::File::open(self.upper_path(&path)) {
                    Ok(f) => {
                        let mut buf = vec![0u8; size as usize];
                        let n = f.read_at(&mut buf, offset).unwrap_or(0);
                        reply.data(&buf[..n])
                    }
                    Err(e) => reply.error(e.into()),
                }
            }
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let Some(f) = self.get_handle(fh) else {
            return reply.error(Errno::EBADF);
        };
        match f.write_all_at(data, offset) {
            Ok(()) => {
                if let Some(p) = self.path(ino) {
                    self.record(&[&p]);
                }
                reply.written(data.len() as u32)
            }
            Err(e) => reply.error(e.into()),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(f) = self.get_handle(fh) {
            let _ = f.sync_data();
        }
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.state.lock().unwrap().handles.remove(&fh.0);
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(path) = self.path(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let list = match self.list(&path) {
            Ok(l) => l,
            Err(e) => return reply.error(errno(&e)),
        };
        let parent_ino = self.ino(parent_of(&path));
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino.0, FileType::Directory, ".".into()),
            (parent_ino, FileType::Directory, "..".into()),
        ];
        for (name, node) in list {
            let kind = match &node {
                Node::Lower(e) if e.is_tree() || e.mode == MODE_GITLINK => FileType::Directory,
                Node::Lower(e) if e.mode == MODE_SYMLINK => FileType::Symlink,
                Node::Lower(_) => FileType::RegularFile,
                Node::Upper(m) => {
                    FileType::from_std(m.file_type()).unwrap_or(FileType::RegularFile)
                }
            };
            entries.push((self.ino(&join(&path, &name)), kind, name));
        }
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(dir) = self.path(parent) else {
            return reply.error(Errno::ENOENT);
        };
        let path = join(&dir, &name.to_string_lossy());
        let run = || -> Result<(FileAttr, u64)> {
            let exists = self.node(&path)?.is_some();
            if exists && flags & libc::O_EXCL != 0 {
                anyhow::bail!(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            if exists {
                self.copy_up(&path)?;
            } else {
                self.upper_dirs(&dir)?;
            }
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(flags & libc::O_TRUNC != 0)
                .open(self.upper_path(&path))?;
            if !exists {
                f.set_permissions(std::fs::Permissions::from_mode(mode & !umask & 0o7777))?;
            }
            self.record(&[&path]);
            let fh = self.handle(f);
            let n = self.node(&path)?.context("vanished")?;
            Ok((self.attr(&path, &n)?, fh))
        };
        match run() {
            Ok((a, fh)) => {
                reply.created(&TTL, &a, Generation(0), FileHandle(fh), FopenFlags::empty())
            }
            Err(e) => reply.error(errno(&e)),
        }
    }
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
/// cache, promisor remote), alternates to the node store, and a full index
/// marked valid for fsmonitor.
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

/// Mount `fs` at `mountpoint` until it is unmounted (`fusermount3 -u`, or
/// Ctrl-C).
pub fn run(fs: WrenFs, mountpoint: &Path) -> Result<()> {
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        MountOption::FSName("wren".into()),
        MountOption::Subtype("wren".into()),
        MountOption::DefaultPermissions,
    ];
    config.n_threads = Some(4);
    fuser::mount(fs, mountpoint, &config).context("mounting (is fuse3 installed?)")?;
    Ok(())
}

/// When the commit was made, for the times the mount reports.
pub fn commit_time(seconds: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds.max(0) as u64)
}
