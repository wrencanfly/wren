//! `wren mount` (M7): a virtual checkout through FUSE. Linux only, with
//! `--features vfs` and fuse3; skipped elsewhere unless WREN_REQUIRE_VFS is
//! set.

#![cfg(all(feature = "vfs", target_os = "linux"))]

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const WREN: &str = env!("CARGO_BIN_EXE_wren");

struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn git(dir: &Path, args: &[&str]) -> String {
    git_env(dir, args, &[])
}

fn git_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@wren.dev"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn wait_for(mut f: impl FnMut() -> bool, what: &str) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn fuse_available() -> bool {
    let ok = Path::new("/dev/fuse").exists()
        && Command::new("fusermount3")
            .arg("-V")
            .output()
            .is_ok_and(|o| o.status.success());
    if !ok {
        assert!(
            std::env::var("WREN_REQUIRE_VFS")
                .unwrap_or_default()
                .is_empty(),
            "WREN_REQUIRE_VFS is set but FUSE is not available"
        );
        eprintln!("skipping: no FUSE");
    }
    ok
}

/// Is the blob `oid` in the node store (loose or in a segment)?
fn stored(store: &Path, oid: &str) -> bool {
    if store.join(&oid[..2]).join(&oid[2..]).exists() {
        return true;
    }
    let s = wren_core::ObjectStore::open(store).unwrap();
    s.has(&oid.parse().unwrap())
}

fn mounted(mnt: &Path) -> bool {
    std::fs::read_to_string("/proc/mounts")
        .unwrap_or_default()
        .lines()
        .any(|l| l.split(' ').nth(1) == Some(mnt.to_str().unwrap()))
}

#[test]
fn virtual_checkout() {
    if !fuse_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let up = root.join("up");
    for d in ["src", "docs", "big"] {
        std::fs::create_dir_all(up.join(d)).unwrap();
    }
    git(&up, &["init", "-q", "-b", "main"]);
    std::fs::write(up.join("README"), "hello\n").unwrap();
    std::fs::write(up.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(up.join("src/old.rs"), "old\n").unwrap();
    std::fs::write(up.join("docs/guide.md"), "# guide\n").unwrap();
    std::fs::write(up.join("big/blob.bin"), vec![7u8; 300_000]).unwrap();
    std::os::unix::fs::symlink("src/lib.rs", up.join("link")).unwrap();
    git(&up, &["add", "-A"]);
    git(&up, &["commit", "-q", "-m", "one"]);
    let big = git(&up, &["rev-parse", "HEAD:big/blob.bin"])
        .trim()
        .to_string();
    let readme = git(&up, &["rev-parse", "HEAD:README"]).trim().to_string();

    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let _server = Proc(
        Command::new(WREN)
            .args(["server", "--data", root.join("data").to_str().unwrap()])
            .args(["--listen", &format!("127.0.0.1:{port}"), "--poll", "0"])
            .arg("--repo")
            .arg(format!("demo={}", up.display()))
            .env("WREN_LOG", "warn")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || {
            Command::new("curl")
                .args(["-sf", &format!("{url}/api/v1/repos")])
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("\"changesets\":1"))
        },
        "ingest",
    );

    let mnt = root.join("mnt");
    let store = root.join("store");
    let overlay = root.join("overlay");
    let mount = |extra: &[&str]| {
        Proc(
            Command::new(WREN)
                .arg("mount")
                .arg(&mnt)
                .args(["--store", store.to_str().unwrap()])
                .args(["--overlay", overlay.to_str().unwrap()])
                .args(extra)
                .env("WREN_SERVER", &url)
                .env("WREN_REPO", "demo")
                .env("WREN_LOG", "warn")
                .spawn()
                .unwrap(),
        )
    };
    let m = mount(&["--git"]);
    wait_for(|| mounted(&mnt), "the mount");

    // Everything is there; nothing has been read.
    let mut names: Vec<String> = std::fs::read_dir(&mnt)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, [".git", "README", "big", "docs", "link", "src"]);
    assert!(!stored(&store, &big) && !stored(&store, &readme));
    // Reading fetches.
    assert_eq!(
        std::fs::read_to_string(mnt.join("README")).unwrap(),
        "hello\n"
    );
    assert!(stored(&store, &readme));
    assert_eq!(
        std::fs::read_link(mnt.join("link")).unwrap().to_str(),
        Some("src/lib.rs")
    );
    assert_eq!(
        std::fs::read_to_string(mnt.join("link")).unwrap(),
        "pub fn a() {}\n"
    );

    // git sees a clean tree without reading anything else (and without
    // fetching anything behind the mount's back).
    let local = [("GIT_NO_LAZY_FETCH", "1")];
    assert_eq!(git_env(&mnt, &["status", "--porcelain"], &local), "");
    assert!(!stored(&store, &big), "git status read an untouched file");

    // Writes go to the overlay.
    std::fs::write(mnt.join("src/lib.rs"), "pub fn a() {}\npub fn b() {}\n").unwrap();
    std::fs::write(mnt.join("new.txt"), "new\n").unwrap();
    std::fs::remove_file(mnt.join("src/old.rs")).unwrap();
    std::fs::create_dir(mnt.join("notes")).unwrap();
    std::fs::write(mnt.join("notes/a.md"), "a\n").unwrap();
    std::fs::rename(mnt.join("docs/guide.md"), mnt.join("docs/manual.md")).unwrap();
    assert!(std::fs::rename(mnt.join("docs"), mnt.join("documentation")).is_err());
    assert!(!mnt.join("src/old.rs").exists());
    assert_eq!(
        std::fs::read_to_string(mnt.join("docs/manual.md")).unwrap(),
        "# guide\n"
    );
    let status = git_env(
        &mnt,
        &["status", "--porcelain", "--untracked-files=all"],
        &local,
    );
    let mut lines: Vec<&str> = status.lines().collect();
    lines.sort();
    assert_eq!(
        lines,
        [
            " D docs/guide.md",
            " D src/old.rs",
            " M src/lib.rs",
            "?? docs/manual.md",
            "?? new.txt",
            "?? notes/a.md",
        ],
        "{status}"
    );
    assert!(!stored(&store, &big), "git status read an untouched file");

    // Commit, as in any working tree: the cache tree spares git from
    // checking (or fetching) the blobs of directories nobody touched.
    git_env(&mnt, &["add", "-A"], &local);
    git_env(&mnt, &["commit", "-q", "-m", "two"], &local);
    assert_eq!(git_env(&mnt, &["status", "--porcelain"], &local), "");
    let stat = git(&mnt, &["show", "--stat", "--format=%s", "HEAD"]);
    assert!(stat.contains("two") && stat.contains("new.txt"), "{stat}");
    assert!(!stored(&store, &big));

    // The overlay outlives the mount.
    Command::new(WREN).arg("umount").arg(&mnt).status().unwrap();
    wait_for(|| !mounted(&mnt), "unmount");
    drop(m);
    let _m = mount(&[]);
    wait_for(|| mounted(&mnt), "the second mount");
    assert_eq!(
        std::fs::read_to_string(mnt.join("new.txt")).unwrap(),
        "new\n"
    );
    assert!(!mnt.join("src/old.rs").exists());
    assert_eq!(
        std::fs::read_to_string(mnt.join("big/blob.bin"))
            .unwrap()
            .len(),
        300_000
    );
    Command::new(WREN).arg("umount").arg(&mnt).status().unwrap();
}
