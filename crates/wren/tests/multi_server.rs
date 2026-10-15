//! Three stateless servers on one Postgres, sharing objects either through
//! one data directory or (no shared filesystem) through a bucket.
//!
//! They start together (their initial syncs serialize on an advisory lock),
//! two of them land commits concurrently, and the third answers queries and
//! checkouts about those commits without syncing: refs, changesets and
//! derived data are in the shared database, objects in the shared store.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use wren_core::testpg::TestPg;

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

#[test]
fn three_servers_share_one_data_directory() {
    three_servers(false);
}

#[test]
fn three_servers_share_a_bucket() {
    three_servers(true);
}

fn three_servers(bucket: bool) {
    let Some(pg) = TestPg::start() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let up = root.join("upstream");
    std::fs::create_dir_all(up.join("src")).unwrap();
    git(&up, &["init", "-q", "-b", "main"]);
    for i in 0..5 {
        std::fs::write(up.join(format!("src/f{i}.txt")), format!("{i}\n")).unwrap();
        git(&up, &["add", "-A"]);
        git(&up, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    // Pushes from Wren need a bare upstream.
    let host = root.join("host.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            up.to_str().unwrap(),
            host.to_str().unwrap(),
        ],
    );

    let segments = format!("file://{}", root.join("bucket").display());
    let ports: Vec<u16> = (0..3).map(|_| free_port()).collect();
    let urls: Vec<String> = ports
        .iter()
        .map(|p| format!("http://127.0.0.1:{p}"))
        .collect();
    let _servers: Vec<Proc> = ports
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let data = match bucket {
                true => root.join(format!("data{i}")),
                false => root.join("data"),
            };
            let mut c = Command::new(WREN);
            c.args(["server", "--data", data.to_str().unwrap()]);
            if bucket {
                c.args(["--segments", &segments]);
            }
            Proc(
                c.args(["--listen", &format!("127.0.0.1:{p}"), "--poll", "0"])
                    .args(["--meta", pg.url(), "--write-token", "w"])
                    .arg("--repo")
                    .arg(format!("demo={}", host.display()))
                    .env("WREN_LOG", "warn")
                    .spawn()
                    .unwrap(),
            )
        })
        .collect();
    for u in &urls {
        wait_for(
            || {
                wren(&["repos", "--server", u, "--json"], &[("WREN_TOKEN", "w")])
                    .1
                    .contains("\"changesets\": 5")
            },
            "every server sees the ingest",
        );
    }

    // Two agents commit and land at the same time, through servers 2 and 3.
    let files = root.join("files");
    std::fs::create_dir_all(&files).unwrap();
    let handles: Vec<_> = [1usize, 2]
        .into_iter()
        .map(|i| {
            let url = urls[i].clone();
            let local = files.join(format!("agent{i}"));
            std::fs::write(&local, format!("from server {}\n", i + 1)).unwrap();
            std::thread::spawn(move || {
                let set = format!("agents/{i}={}", local.display());
                let env = [
                    ("WREN_SERVER", url.as_str()),
                    ("WREN_WRITE_SERVER", url.as_str()),
                    ("WREN_REPO", "demo"),
                    ("WREN_TOKEN", "w"),
                    ("WREN_WRITE_TOKEN", "w"),
                    ("WREN_AUTHOR", "Agent <agent@wren.dev>"),
                ];
                let msg = format!("agent via server {}", i + 1);
                let (ok, out, err) = wren(
                    &["commit", "-m", &msg, "--land", "main", "--set", &set],
                    &env,
                );
                assert!(ok, "server {}: {out}{err}", i + 1);
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    // Server 1 never synced after startup, yet it has both commits.
    let env1 = [
        ("WREN_SERVER", urls[0].as_str()),
        ("WREN_REPO", "demo"),
        ("WREN_TOKEN", "w"),
    ];
    let (ok, log, err) = wren(&["log", "-n", "3"], &env1);
    assert!(ok, "{err}");
    for want in ["agent via server 2", "agent via server 3", "c4"] {
        assert!(log.contains(want), "missing {want:?} in {log}");
    }
    let host_main = git(&host, &["rev-parse", "main"]);
    let (_, show, _) = wren(&["show", host_main.trim()], &env1);
    assert!(show.contains("agent via server"), "{show}");

    // And it serves their objects: a checkout through server 1.
    let ws = root.join("ws");
    let (ok, _, err) = wren(
        &[
            "checkout",
            ws.to_str().unwrap(),
            "--store",
            root.join("store").to_str().unwrap(),
        ],
        &env1,
    );
    assert!(ok, "{err}");
    assert_eq!(
        std::fs::read_to_string(ws.join("agents/1")).unwrap(),
        "from server 2\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("agents/2")).unwrap(),
        "from server 3\n"
    );

    if bucket {
        // The ingest's pack plus one sealed segment per landed commit, at
        // least; the servers' data directories share nothing.
        let segments = std::fs::read_dir(root.join("bucket"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "idx"))
            .count();
        assert!(segments >= 3, "{segments} segments in the bucket");
    }

    // The host has a linear history with both.
    let parents = git(&host, &["log", "--format=%P", "main"]);
    assert!(parents.lines().all(|l| !l.contains(' ')), "{parents}");
    git(&host, &["fsck", "--no-dangling"]);
}
