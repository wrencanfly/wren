//! `wren mount` on Linux: the union view (`super::View`) served through
//! FUSE.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    MountOption, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow, WriteFlags,
};

use super::{errno_of, join, parent_of, Attr, Kind, View};

const TTL: Duration = Duration::from_secs(1);

pub struct WrenFs {
    view: View,
    /// Open overlay files, by FUSE file handle (0: a file of the commit,
    /// read through the view).
    handles: Mutex<(u64, HashMap<u64, Arc<std::fs::File>>)>,
}

fn errno(e: &anyhow::Error) -> Errno {
    Errno::from_i32(errno_of(e))
}

fn file_type(k: Kind) -> FileType {
    match k {
        Kind::Dir => FileType::Directory,
        Kind::File => FileType::RegularFile,
        Kind::Symlink => FileType::Symlink,
    }
}

fn fuse_attr(a: &Attr) -> FileAttr {
    FileAttr {
        ino: INodeNo(a.ino),
        size: a.size,
        blocks: a.size.div_ceil(512),
        atime: a.mtime,
        mtime: a.mtime,
        ctime: a.mtime,
        crtime: a.mtime,
        kind: file_type(a.kind),
        perm: a.perm,
        nlink: a.nlink,
        uid: a.uid,
        gid: a.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

impl WrenFs {
    pub fn new(view: View) -> WrenFs {
        WrenFs {
            view,
            handles: Mutex::new((1, HashMap::new())),
        }
    }

    fn path(&self, ino: INodeNo) -> Option<String> {
        self.view.path(ino.0)
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> Option<String> {
        Some(join(&self.path(parent)?, &name.to_string_lossy()))
    }

    fn entry_reply(&self, path: &str, reply: ReplyEntry) {
        match self.view.stat(path) {
            Ok(Some(a)) => reply.entry(&TTL, &fuse_attr(&a), Generation(0)),
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn handle(&self, file: std::fs::File) -> u64 {
        let mut h = self.handles.lock().unwrap();
        let fh = h.0;
        h.0 += 1;
        h.1.insert(fh, Arc::new(file));
        fh
    }

    fn get_handle(&self, fh: FileHandle) -> Option<Arc<std::fs::File>> {
        self.handles.lock().unwrap().1.get(&fh.0).cloned()
    }
}

impl Filesystem for WrenFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(path) = self.child(parent, name) else {
            return reply.error(Errno::ENOENT);
        };
        self.entry_reply(&path, reply);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(path) = self.path(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match self.view.stat(&path) {
            Ok(Some(a)) => reply.attr(&TTL, &fuse_attr(&a)),
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
        let t = |v: Option<TimeOrNow>| match v {
            Some(TimeOrNow::SpecificTime(t)) => Some(t),
            Some(TimeOrNow::Now) => Some(SystemTime::now()),
            None => None,
        };
        match self.view.setattr(&path, mode, size, t(atime), t(mtime)) {
            Ok(a) => reply.attr(&TTL, &fuse_attr(&a)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some(path) = self.path(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match self.view.readlink(&path) {
            Ok(t) => reply.data(&t),
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
        let Some(path) = self.child(parent, name) else {
            return reply.error(Errno::ENOENT);
        };
        match self.view.mkdir(&path, mode & !umask) {
            Ok(a) => reply.entry(&TTL, &fuse_attr(&a), Generation(0)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(path) = self.child(parent, name) else {
            return reply.error(Errno::ENOENT);
        };
        match self.view.unlink(&path) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(path) = self.child(parent, name) else {
            return reply.error(Errno::ENOENT);
        };
        match self.view.rmdir(&path) {
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
        let Some(path) = self.child(parent, link_name) else {
            return reply.error(Errno::ENOENT);
        };
        match self.view.symlink(&path, target) {
            Ok(a) => reply.entry(&TTL, &fuse_attr(&a), Generation(0)),
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
        let (Some(from), Some(to)) = (self.child(parent, name), self.child(newparent, newname))
        else {
            return reply.error(Errno::ENOENT);
        };
        match self.view.rename(&from, &to) {
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
        let truncate = flags.0 & libc::O_TRUNC != 0;
        let write = flags.acc_mode() != OpenAccMode::O_RDONLY || truncate;
        let run = || -> Result<u64> {
            if write {
                return Ok(self.handle(self.view.open_write(&path, truncate)?));
            }
            Ok(match self.view.open_upper(&path)? {
                Some(f) => self.handle(f),
                None => 0,
            })
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
            return match super::read_at(&f, offset, size, u64::MAX) {
                Ok((data, _)) => reply.data(&data),
                Err(e) => reply.error(errno(&e)),
            };
        }
        let Some(path) = self.path(ino) else {
            return reply.error(Errno::ENOENT);
        };
        // A file of the commit (or one copied up by someone else since).
        match self.view.read(&path, offset, size) {
            Ok((data, _)) => reply.data(&data),
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
                    self.view.record(&[&p]);
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
        self.handles.lock().unwrap().1.remove(&fh.0);
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
        let list = match self.view.readdir(&path) {
            Ok(l) => l,
            Err(e) => return reply.error(errno(&e)),
        };
        let parent_ino = self.view.ino(parent_of(&path));
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino.0, FileType::Directory, ".".into()),
            (parent_ino, FileType::Directory, "..".into()),
        ];
        entries.extend(list.into_iter().map(|e| (e.ino, file_type(e.kind), e.name)));
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
        let Some(path) = self.child(parent, name) else {
            return reply.error(Errno::ENOENT);
        };
        let run = || -> Result<(Attr, u64)> {
            let f = self.view.create(
                &path,
                mode & !umask,
                flags & libc::O_EXCL != 0,
                flags & libc::O_TRUNC != 0,
            )?;
            let fh = self.handle(f);
            let a = self.view.stat(&path)?.context("vanished")?;
            Ok((a, fh))
        };
        match run() {
            Ok((a, fh)) => reply.created(
                &TTL,
                &fuse_attr(&a),
                Generation(0),
                FileHandle(fh),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(errno(&e)),
        }
    }
}

/// Mount `view` at `mountpoint` until it is unmounted (`fusermount3 -u`,
/// or Ctrl-C).
pub fn run(view: View, mountpoint: &Path) -> Result<()> {
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        MountOption::FSName("wren".into()),
        MountOption::Subtype("wren".into()),
        MountOption::DefaultPermissions,
    ];
    config.n_threads = Some(4);
    fuser::mount(WrenFs::new(view), mountpoint, &config)
        .context("mounting (is fuse3 installed?)")?;
    Ok(())
}
