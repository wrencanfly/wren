//! `wren mount` on macOS: the union view (`super::View`) served by an
//! NFSv3 server on localhost, which the system's own NFS client mounts
//! (`mount_nfs`, allowed for the user who owns the mount point). No kernel
//! extension and no root, which FUSE (macFUSE) and FSKit (a signed app
//! extension the user enables in System Settings) would need. EdenFS takes
//! the same route on macOS.
//!
//! NFS is stateless: there are no open files, so a write copies the file up
//! (if it is the commit's) and writes the overlay file at the offset. The
//! mount uses READDIRPLUS (`rdirplus`), whose entries carry attributes only
//! when they are cheap: a file of the commit whose blob is not fetched yet
//! gets none, and the client asks with GETATTR if it needs them, so listing
//! a directory fetches no blobs.
//!
//! The mount ends when it is unmounted (`wren umount`, `umount`, Finder) or
//! on Ctrl-C, which unmounts it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfsstring, nfstime3, sattr3, set_atime,
    set_mode3, set_mtime, set_size3, specdata3,
};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use super::{errno_of, join, parent_of, Attr, Kind, View};

struct WrenNfs(Arc<View>);

fn status(e: &anyhow::Error) -> nfsstat3 {
    match errno_of(e) {
        libc::ENOENT => nfsstat3::NFS3ERR_NOENT,
        libc::EEXIST => nfsstat3::NFS3ERR_EXIST,
        libc::EXDEV => nfsstat3::NFS3ERR_XDEV,
        libc::ENOTDIR => nfsstat3::NFS3ERR_NOTDIR,
        libc::EISDIR => nfsstat3::NFS3ERR_ISDIR,
        libc::EINVAL => nfsstat3::NFS3ERR_INVAL,
        libc::ENOSPC => nfsstat3::NFS3ERR_NOSPC,
        libc::ENOTEMPTY => nfsstat3::NFS3ERR_NOTEMPTY,
        libc::EACCES => nfsstat3::NFS3ERR_ACCES,
        libc::EPERM => nfsstat3::NFS3ERR_PERM,
        libc::ENAMETOOLONG => nfsstat3::NFS3ERR_NAMETOOLONG,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

fn time(t: SystemTime) -> nfstime3 {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    nfstime3 {
        seconds: d.as_secs() as u32,
        nseconds: d.subsec_nanos(),
    }
}

fn from_time(t: &nfstime3) -> SystemTime {
    UNIX_EPOCH + Duration::new(t.seconds as u64, t.nseconds)
}

fn fattr(a: &Attr) -> fattr3 {
    fattr3 {
        ftype: match a.kind {
            Kind::Dir => ftype3::NF3DIR,
            Kind::File => ftype3::NF3REG,
            Kind::Symlink => ftype3::NF3LNK,
        },
        mode: a.perm as u32,
        nlink: a.nlink,
        uid: a.uid,
        gid: a.gid,
        size: a.size,
        used: a.size,
        rdev: specdata3::default(),
        fsid: 0x7772_656e, // "wren"
        fileid: a.ino,
        atime: time(a.mtime),
        mtime: time(a.mtime),
        ctime: time(a.mtime),
    }
}

fn name(f: &filename3) -> Result<String, nfsstat3> {
    let s = std::str::from_utf8(&f.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
    if s.is_empty() || s.contains('/') || s == "." || s == ".." {
        return Err(nfsstat3::NFS3ERR_INVAL);
    }
    Ok(s.to_string())
}

impl WrenNfs {
    fn path(&self, id: fileid3) -> Result<String, nfsstat3> {
        self.0.path(id).ok_or(nfsstat3::NFS3ERR_STALE)
    }

    fn child(&self, dir: fileid3, f: &filename3) -> Result<String, nfsstat3> {
        Ok(join(&self.path(dir)?, &name(f)?))
    }

    /// Run `f` on the view off the async executor (it may fetch objects).
    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&View) -> Result<T, nfsstat3> + Send + 'static,
    ) -> Result<T, nfsstat3> {
        let v = self.0.clone();
        tokio::task::spawn_blocking(move || f(&v))
            .await
            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?
    }

    async fn stat(&self, path: String) -> Result<fattr3, nfsstat3> {
        self.run(move |v| match v.stat(&path) {
            Ok(Some(a)) => Ok(fattr(&a)),
            Ok(None) => Err(nfsstat3::NFS3ERR_NOENT),
            Err(e) => Err(status(&e)),
        })
        .await
    }
}

fn setattr_args(
    s: &sattr3,
) -> (
    Option<u32>,
    Option<u64>,
    Option<SystemTime>,
    Option<SystemTime>,
) {
    let mode = match s.mode {
        set_mode3::mode(m) => Some(m),
        set_mode3::Void => None,
    };
    let size = match s.size {
        set_size3::size(n) => Some(n),
        set_size3::Void => None,
    };
    let atime = match &s.atime {
        set_atime::SET_TO_CLIENT_TIME(t) => Some(from_time(t)),
        set_atime::SET_TO_SERVER_TIME => Some(SystemTime::now()),
        set_atime::DONT_CHANGE => None,
    };
    let mtime = match &s.mtime {
        set_mtime::SET_TO_CLIENT_TIME(t) => Some(from_time(t)),
        set_mtime::SET_TO_SERVER_TIME => Some(SystemTime::now()),
        set_mtime::DONT_CHANGE => None,
    };
    (mode, size, atime, mtime)
}

#[async_trait]
impl NFSFileSystem for WrenNfs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        1
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let dir = self.path(dirid)?;
        match filename.0.as_slice() {
            b"." => return Ok(dirid),
            b".." => return Ok(self.0.ino(parent_of(&dir))),
            _ => {}
        }
        let path = join(&dir, &name(filename)?);
        self.run(move |v| match v.stat(&path) {
            Ok(Some(a)) => Ok(a.ino),
            Ok(None) => Err(nfsstat3::NFS3ERR_NOENT),
            Err(e) => Err(status(&e)),
        })
        .await
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        self.stat(self.path(id)?).await
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let path = self.path(id)?;
        let (mode, size, atime, mtime) = setattr_args(&setattr);
        self.run(move |v| {
            v.setattr(&path, mode, size, atime, mtime)
                .map(|a| fattr(&a))
                .map_err(|e| status(&e))
        })
        .await
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let path = self.path(id)?;
        self.run(move |v| v.read(&path, offset, count).map_err(|e| status(&e)))
            .await
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let path = self.path(id)?;
        let data = data.to_vec();
        self.run(move |v| {
            v.write(&path, offset, &data)
                .map(|a| fattr(&a))
                .map_err(|e| status(&e))
        })
        .await
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let path = self.child(dirid, filename)?;
        let (mode, size, atime, mtime) = setattr_args(&attr);
        self.run(move |v| {
            let run = || -> Result<Attr> {
                v.create(&path, mode.unwrap_or(0o644), false, size == Some(0))?;
                v.setattr(&path, mode, size.filter(|s| *s != 0), atime, mtime)
            };
            run().map(|a| (a.ino, fattr(&a))).map_err(|e| status(&e))
        })
        .await
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let path = self.child(dirid, filename)?;
        self.run(move |v| {
            let run = || -> Result<u64> {
                v.create(&path, 0o644, true, false)?;
                Ok(v.ino(&path))
            };
            run().map_err(|e| status(&e))
        })
        .await
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let path = self.child(dirid, dirname)?;
        self.run(move |v| {
            v.mkdir(&path, 0o755)
                .map(|a| (a.ino, fattr(&a)))
                .map_err(|e| status(&e))
        })
        .await
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let path = self.child(dirid, filename)?;
        self.run(move |v| v.remove(&path).map_err(|e| status(&e)))
            .await
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let from = self.child(from_dirid, from_filename)?;
        let to = self.child(to_dirid, to_filename)?;
        self.run(move |v| v.rename(&from, &to).map_err(|e| status(&e)))
            .await
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let dir = self.path(dirid)?;
        self.run(move |v| {
            let list = v.readdir(&dir).map_err(|e| status(&e))?;
            // The cookie is the last entry's id; an entry renamed away in
            // between restarts the listing, which NFS clients tolerate.
            let start = match start_after {
                0 => 0,
                after => list
                    .iter()
                    .position(|e| e.ino == after)
                    .map_or(0, |i| i + 1),
            };
            let entries: Vec<DirEntry> = list[start.min(list.len())..]
                .iter()
                .take(max_entries)
                .map(|e| DirEntry {
                    fileid: e.ino,
                    name: nfsstring(e.name.as_bytes().to_vec()),
                    attr: e.attr.as_ref().map(fattr),
                })
                .collect();
            Ok(ReadDirResult {
                end: start + entries.len() >= list.len(),
                entries,
            })
        })
        .await
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let path = self.child(dirid, linkname)?;
        let target = PathBuf::from(std::ffi::OsStr::new(
            std::str::from_utf8(&symlink.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?,
        ));
        self.run(move |v| {
            v.symlink(&path, &target)
                .map(|a| (a.ino, fattr(&a)))
                .map_err(|e| status(&e))
        })
        .await
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let path = self.path(id)?;
        self.run(move |v| v.readlink(&path).map(nfsstring).map_err(|e| status(&e)))
            .await
    }
}

fn is_mounted(mountpoint: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(m), Some(parent)) = (std::fs::metadata(mountpoint), mountpoint.parent()) else {
        return false;
    };
    std::fs::metadata(parent).is_ok_and(|p| p.dev() != m.dev())
}

/// Serve `view` and mount it at `mountpoint` until it is unmounted.
pub fn run(view: View, mountpoint: &Path) -> Result<()> {
    if is_mounted(mountpoint) {
        bail!("{} is already a mount point", mountpoint.display());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    let mp = mountpoint.to_path_buf();
    // The view owns a runtime, which must not be dropped by an async task:
    // it is let go of here, after the server's runtime is gone.
    let view = Arc::new(view);
    let keep = view.clone();
    let r = rt.block_on(async move {
        let listener = NFSTcpListener::bind("127.0.0.1:0", WrenNfs(view))
            .await
            .context("starting the NFS server")?;
        let port = listener.get_listen_port();
        let server = tokio::spawn(async move { listener.handle_forever().await });
        let opts = format!(
            "nolocks,locallocks,vers=3,tcp,rdirplus,nobrowse,soft,intr,timeo=600,\
             rsize=1048576,wsize=1048576,actimeo=1,port={port},mountport={port}"
        );
        let out = tokio::process::Command::new("/sbin/mount_nfs")
            .args(["-o", &opts, "127.0.0.1:/"])
            .arg(&mp)
            .output()
            .await
            .context("running mount_nfs")?;
        if !out.status.success() {
            bail!(
                "mount_nfs failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        tracing::debug!(port, "nfs mount up");
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        loop {
            tokio::select! {
                _ = interrupt.recv() => break,
                _ = terminate.recv() => break,
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    let mp = mp.clone();
                    if !tokio::task::spawn_blocking(move || is_mounted(&mp)).await? {
                        server.abort();
                        return Ok(());
                    }
                }
            }
        }
        // Asked to stop: unmount (forcibly if busy), then stop serving.
        let st = tokio::process::Command::new("/sbin/umount")
            .arg(&mp)
            .status()
            .await?;
        if !st.success() {
            let _ = tokio::process::Command::new("/sbin/umount")
                .arg("-f")
                .arg(&mp)
                .status()
                .await;
        }
        server.abort();
        Ok(())
    });
    drop(rt);
    drop(keep);
    r
}
