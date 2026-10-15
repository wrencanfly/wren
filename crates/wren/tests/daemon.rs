//! The node tier: server → region cache → `wren daemon` → checkouts over a
//! unix socket. Strict tiering, snapshot seeding, the budget and leases.

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
    c.args(args).env("WREN_LOG", "warn");
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

fn get_json(url: &str, sock: Option<&Path>) -> serde_json::Value {
    let mut c = Command::new("curl");
    c.arg("-sf");
    if let Some(s) = sock {
        c.arg("--unix-socket").arg(s);
    }
    let out = c.arg(url).output().unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
}

/// Deterministic, incompressible bytes.
fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = 0x9e3779b97f4a7c15u64 ^ seed;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[test]
fn node_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    // A repository whose blobs are large next to the node's budget: ten
    // commits, each replacing three 100 KB files.
    let up = root.join("upstream");
    std::fs::create_dir_all(up.join("big")).unwrap();
    std::fs::create_dir_all(up.join("src")).unwrap();
    git(&up, &["init", "-q", "-b", "main"]);
    for i in 0..10u64 {
        for f in 0..3u64 {
            std::fs::write(up.join(format!("big/f{f}.bin")), noise(i * 10 + f, 100_000)).unwrap();
        }
        std::fs::write(up.join("src/lib.txt"), format!("version {i}\n")).unwrap();
        git(&up, &["add", "-A"]);
        git(&up, &["commit", "-q", "-m", &format!("c{i}")]);
    }

    let sport = free_port();
    let server_url = format!("http://127.0.0.1:{sport}");
    let _server = Proc(
        Command::new(WREN)
            .args(["server", "--data", root.join("data").to_str().unwrap()])
            .args(["--listen", &format!("127.0.0.1:{sport}"), "--poll", "0"])
            .args(["--write-token", "w"])
            .arg("--repo")
            .arg(format!("demo={}", up.display()))
            .env("WREN_LOG", "warn")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || {
            wren(
                &["repos", "--server", &server_url, "--json"],
                &[("WREN_TOKEN", "w")],
            )
            .1
            .contains("\"changesets\": 10")
        },
        "ingest",
    );
    // Publish a snapshot of main (every tree; blobs of src/ only).
    let out = Command::new("curl")
        .args(["-sf", "-X", "POST", "-H", "Authorization: Bearer w"])
        .arg(format!("{server_url}/api/v1/repos/demo/snapshot"))
        .output()
        .unwrap();
    let snap: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(snap["objects"].as_u64().unwrap() >= 5, "{snap}");

    // A node may not point at the server directly.
    let store = root.join("node");
    let (ok, _, err) = wren(
        &[
            "daemon",
            "--store",
            store.to_str().unwrap(),
            "--upstream",
            &server_url,
            "--upstream-timeout",
            "5",
        ],
        &[],
    );
    assert!(!ok && err.contains("must point at a region"), "{err}");

    let rport = free_port();
    let region_url = format!("http://127.0.0.1:{rport}");
    let _region = Proc(
        Command::new(WREN)
            .args(["cache", "--upstream", &server_url])
            .args(["--dir", root.join("region").to_str().unwrap()])
            .args(["--listen", &format!("127.0.0.1:{rport}")])
            .env("WREN_LOG", "warn")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || get_json(&format!("{region_url}/v1/capabilities"), None)["tier"] == "region",
        "region cache",
    );
    // Nor at another node... nor may a region point at a region.
    let (ok, _, err) = wren(
        &[
            "cache",
            "--upstream",
            &region_url,
            "--dir",
            root.join("r2").to_str().unwrap(),
            "--listen",
            &format!("127.0.0.1:{}", free_port()),
        ],
        &[],
    );
    assert!(!ok && err.contains("must point at a server"), "{err}");

    // The daemon: 700 KB budget, seeded from the snapshot.
    let sock = store.join("daemon.sock");
    let _daemon = Proc(
        Command::new(WREN)
            .args(["daemon", "--store", store.to_str().unwrap()])
            .args(["--upstream", &region_url, "--budget", "700KiB"])
            .args(["--snapshot", "demo"])
            .env("WREN_LOG", "warn")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || get_json("http://d/v1/capabilities", Some(&sock))["tier"] == "node",
        "daemon",
    );
    let stats = get_json("http://d/v1/stats", Some(&sock));
    assert_eq!(stats["snapshot_downloads"], 2, "{stats}");
    let rstats = get_json(&format!("{region_url}/v1/stats"), None);
    assert_eq!(rstats["snapshot_downloads"], 2, "{rstats}");

    let env = [
        ("WREN_SERVER", region_url.as_str()),
        ("WREN_REPO", "demo"),
        ("WREN_OBJECTS", &format!("unix:{}", sock.display())),
    ];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, *v)).collect();
    // A sparse checkout of src/ needs nothing beyond the snapshot.
    let before = get_json("http://d/v1/stats", Some(&sock));
    let ws = root.join("ws-src");
    let (ok, out, err) = wren(
        &[
            "checkout",
            ws.to_str().unwrap(),
            "--sparse",
            "src",
            "--store",
            store.to_str().unwrap(),
        ],
        &env,
    );
    assert!(ok, "{out}{err}");
    let after = get_json("http://d/v1/stats", Some(&sock));
    assert_eq!(after["misses"], before["misses"], "{after}");
    assert_eq!(
        std::fs::read_to_string(ws.join("src/lib.txt")).unwrap(),
        "version 9\n"
    );

    // Full checkouts of three revisions: 900 KB of blobs each, more than
    // the budget in total. The latest is a git checkout and holds a lease.
    for (i, rev) in ["main~4", "main~2", "main"].iter().enumerate() {
        let ws = root.join(format!("ws{i}"));
        let mut args = vec![
            "checkout",
            ws.to_str().unwrap(),
            "--rev",
            rev,
            "--store",
            store.to_str().unwrap(),
        ];
        if *rev == "main" {
            args.push("--git");
        }
        let (ok, out, err) = wren(&args, &env);
        assert!(ok, "{out}{err}");
    }
    // Maintenance runs as objects arrive; wait for it to settle under the
    // budget.
    wait_for(
        || {
            let s = get_json("http://d/v1/stats", Some(&sock));
            s["store_bytes"].as_u64().unwrap_or(u64::MAX) <= 700 << 10
        },
        "store under budget",
    );
    // The leased git checkout still reads every blob through alternates
    // (with lazy fetching off, so nothing is quietly fetched back).
    let gitws = root.join("ws2");
    assert_eq!(git(&gitws, &["status", "--porcelain"]), "");
    for f in 0..3 {
        let out = Command::new("git")
            .arg("-C")
            .arg(&gitws)
            .args(["cat-file", "blob", &format!("HEAD:big/f{f}.bin")])
            .env("GIT_NO_LAZY_FETCH", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, noise(90 + f, 100_000));
    }
    // Something was evicted: the blobs of main~4 are no longer there.
    let stats = get_json("http://d/v1/stats", Some(&sock));
    let gone = Command::new("git")
        .arg("-C")
        .arg(&gitws)
        .args(["cat-file", "-e", "HEAD~4:big/f0.bin"])
        .env("GIT_NO_LAZY_FETCH", "1")
        .output()
        .unwrap();
    assert!(!gone.status.success(), "{stats}");
    // An evicted blob comes back on demand.
    let old = root.join("ws-again");
    let (ok, out, err) = wren(
        &[
            "checkout",
            old.to_str().unwrap(),
            "--rev",
            "main~4",
            "--store",
            store.to_str().unwrap(),
        ],
        &env,
    );
    assert!(ok, "{out}{err}");
    assert_eq!(
        std::fs::read(old.join("big/f1.bin")).unwrap(),
        noise(51, 100_000)
    );
}
