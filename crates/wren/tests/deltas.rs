//! Delta frames end to end: server -> cache -> client.
//!
//! A node that checked out v1 updates to v2. The client names the v1
//! version of each changed path as a delta base; the cache, which holds v1
//! too, passes the hint upstream, stores the object whole, and computes its
//! own delta for the client. The bytes on the wire drop to roughly the size
//! of the change, and the checkout is identical to one made without deltas.

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
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@wren.dev"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn wren(args: &[&str], env: &[(&str, &str)]) -> (bool, String, String) {
    let mut c = Command::new(WREN);
    c.args(args)
        .env("WREN_LOG", "warn")
        .env_remove("WREN_NO_DELTA");
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn wait_for(mut f: impl FnMut() -> bool, what: &str) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// (objects, deltas, bytes) from a checkout's report line.
fn checkout(env: &[(&str, &str)], rev: &str, dest: &Path, store: &Path) -> (u64, u64, u64) {
    let (ok, _, err) = wren(
        &[
            "checkout",
            "--rev",
            rev,
            dest.to_str().unwrap(),
            "--store",
            store.to_str().unwrap(),
        ],
        env,
    );
    assert!(ok, "checkout {rev}: {err}");
    let num = |after: &str| -> u64 {
        let i = err
            .find(after)
            .unwrap_or_else(|| panic!("{after:?} in {err}"));
        err[..i]
            .split_whitespace()
            .last()
            .unwrap()
            .trim_start_matches('(')
            .parse()
            .unwrap()
    };
    (num(" objects ("), num(" as deltas"), num(" bytes,"))
}

fn files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if rel.starts_with(".wren") {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((rel, std::fs::read(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn incremental_update_sends_deltas() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let up = root.join("upstream");
    std::fs::create_dir_all(up.join("src/deep")).unwrap();
    git(&up, &["init", "-q", "-b", "main"]);
    let big: String = (0..3000)
        .map(|i| format!("fn f{i}() -> u32 {{ {i} }}\n"))
        .collect();
    std::fs::write(up.join("src/big.rs"), &big).unwrap();
    std::fs::write(up.join("src/deep/other.rs"), &big[..20_000]).unwrap();
    for i in 0..40 {
        std::fs::write(up.join(format!("src/f{i}.txt")), format!("file {i}\n")).unwrap();
    }
    git(&up, &["add", "-A"]);
    git(&up, &["commit", "-q", "-m", "v1"]);
    git(&up, &["tag", "v1"]);
    // v2: one line changed in each big file, one small file added.
    std::fs::write(up.join("src/big.rs"), big.replace("{ 1500 }", "{ 15000 }")).unwrap();
    std::fs::write(
        up.join("src/deep/other.rs"),
        big[..20_000].replace("{ 10 }", "{ 100 }"),
    )
    .unwrap();
    std::fs::write(up.join("src/new.txt"), "new\n").unwrap();
    git(&up, &["add", "-A"]);
    git(&up, &["commit", "-q", "-m", "v2"]);

    let sport = free_port();
    let server_url = format!("http://127.0.0.1:{sport}");
    let _server = Proc(
        Command::new(WREN)
            .args(["server", "--data", root.join("data").to_str().unwrap()])
            .args(["--listen", &format!("127.0.0.1:{sport}"), "--poll", "0"])
            .arg("--repo")
            .arg(format!("demo={}", up.display()))
            .env("WREN_LOG", "warn")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || {
            wren(&["repos", "--server", &server_url, "--json"], &[])
                .1
                .contains("\"changesets\": 2")
        },
        "initial sync",
    );
    let cport = free_port();
    let cache_url = format!("http://127.0.0.1:{cport}");
    let _cache = Proc(
        Command::new(WREN)
            .args(["cache", "--upstream", &server_url])
            .args(["--dir", root.join("cache").to_str().unwrap()])
            .args(["--listen", &format!("127.0.0.1:{cport}")])
            .env("WREN_LOG", "warn")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || wren(&["repos", "--server", &cache_url], &[]).0,
        "cache up",
    );

    let env = [("WREN_SERVER", cache_url.as_str()), ("WREN_REPO", "demo")];
    let no_delta = [
        ("WREN_SERVER", cache_url.as_str()),
        ("WREN_REPO", "demo"),
        ("WREN_NO_DELTA", "1"),
    ];

    // Baseline: the same update without delta hints.
    checkout(&no_delta, "v1", &root.join("a/v1"), &root.join("a/store"));
    let (objs_whole, deltas_whole, bytes_whole) =
        checkout(&no_delta, "main", &root.join("a/v2"), &root.join("a/store"));
    assert_eq!(deltas_whole, 0);

    // With hints, through the cache (which has v1 from the baseline run).
    checkout(&env, "v1", &root.join("b/v1"), &root.join("b/store"));
    let (objs, deltas, bytes) = checkout(&env, "main", &root.join("b/v2"), &root.join("b/store"));
    assert_eq!(objs, objs_whole, "the same objects are fetched");
    // Fetched: the commit, the root, src and src/deep trees, the two big
    // blobs and the new file. Deltas: src (42 entries) and the two big
    // blobs. The commit and the new file have no base, and the root and
    // src/deep trees (one entry each) are under 64 bytes, so they go whole.
    assert_eq!(objs, 7);
    assert_eq!(deltas, 3, "{objs} objects, {deltas} deltas");
    assert!(
        bytes * 5 < bytes_whole,
        "deltas should send far less: {bytes} vs {bytes_whole} bytes"
    );
    assert_eq!(files(&root.join("b/v2")), files(&root.join("a/v2")));

    // Directly against the server, with a fresh node: same result.
    let direct = [("WREN_SERVER", server_url.as_str()), ("WREN_REPO", "demo")];
    checkout(&direct, "v1", &root.join("c/v1"), &root.join("c/store"));
    let (_, d, b) = checkout(&direct, "main", &root.join("c/v2"), &root.join("c/store"));
    assert_eq!((d, b), (deltas, bytes));
    assert_eq!(files(&root.join("c/v2")), files(&root.join("a/v2")));
}
