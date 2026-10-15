//! Stock git against Wren's git protocol front end (M5): clone, partial
//! clone with lazy fetches, fetch, push; Wren as the source of truth with
//! the git host as a downstream mirror; protected refs, the policy hook and
//! the audit log.

use std::io::{BufRead, BufReader, Read, Write};
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

fn git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@wren.dev"])
        .args(["-c", "protocol.version=2"])
        .env("GIT_TERMINAL_PROMPT", "0");
    c
}

fn git_try(dir: &Path, args: &[&str]) -> (bool, String, String) {
    let out = git_cmd(dir).args(args).output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let (ok, out, err) = git_try(dir, args);
    assert!(ok, "git {args:?}: {err}");
    out
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

fn get(url: &str, token: &str) -> serde_json::Value {
    let out = Command::new("curl")
        .args(["-sf", "-H", &format!("Authorization: Bearer {token}"), url])
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
}

/// A policy hook that refuses refs under refs/heads/forbidden/.
fn policy_hook() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/check", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s);
            let mut len = 0;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let body = String::from_utf8_lossy(&body);
            let resp = if body.contains("refs/heads/forbidden/") {
                "HTTP/1.1 403 Forbidden\r\nContent-Length: 22\r\nConnection: close\r\n\r\nforbidden by the hook\n"
            } else {
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            };
            let _ = r.get_mut().write_all(resp.as_bytes());
        }
    });
    url
}

fn setup(root: &Path) -> std::path::PathBuf {
    let work = root.join("work");
    std::fs::create_dir_all(work.join("src")).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    for i in 0..6 {
        std::fs::write(work.join("src/lib.txt"), format!("version {i}\n")).unwrap();
        std::fs::write(
            work.join(format!("src/f{i}.txt")),
            format!("file {i}\n").repeat(100),
        )
        .unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(&work, &["tag", "-a", "v1", "-m", "release 1", "HEAD~2"]);
    git(&work, &["branch", "side", "HEAD~3"]);
    let host = root.join("host.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            host.to_str().unwrap(),
        ],
    );
    host
}

fn server(root: &Path, host: &Path, extra: &[&str]) -> (Proc, String) {
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let p = Proc(
        Command::new(WREN)
            .args(["server", "--data", root.join("data").to_str().unwrap()])
            .args(["--listen", &format!("127.0.0.1:{port}"), "--poll", "0"])
            .args(["--token", "r", "--write-token", "w"])
            .arg("--repo")
            .arg(format!("demo={}", host.display()))
            .args(extra)
            .env("WREN_LOG", "warn")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || get(&format!("{url}/api/v1/repos"), "r")[0]["changesets"] == 6,
        "ingest",
    );
    (p, url)
}

#[test]
fn stock_git_with_wren_as_source_of_truth() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let host = setup(&root);
    let hook = policy_hook();
    let (_server, url) = server(
        &root,
        &host,
        &[
            "--source-of-truth",
            "wren",
            "--protect",
            "refs/heads/main",
            "--policy-hook",
            &hook,
            "--export-every-ms",
            "100",
        ],
    );
    let port = url.rsplit(':').next().unwrap();
    let read_url = format!("http://x:r@127.0.0.1:{port}/git/demo.git");
    let write_url = format!("http://x:w@127.0.0.1:{port}/git/demo");

    // Clone: refs, history, the annotated tag.
    git(&root, &["clone", "-q", &read_url, "clone"]);
    let clone = root.join("clone");
    assert_eq!(
        git(&clone, &["rev-parse", "HEAD"]),
        git(&host, &["rev-parse", "main"])
    );
    assert_eq!(git(&clone, &["log", "--format=%s"]).lines().count(), 6);
    assert_eq!(git(&clone, &["cat-file", "-t", "v1"]).trim(), "tag");
    assert!(git(&clone, &["branch", "-r"]).contains("origin/side"));
    git(&clone, &["fsck", "--no-dangling"]);

    // A partial clone fetches blobs lazily.
    git(
        &root,
        &["clone", "-q", "--filter=blob:none", &read_url, "partial"],
    );
    let partial = root.join("partial");
    assert_eq!(
        std::fs::read_to_string(partial.join("src/lib.txt")).unwrap(),
        "version 5\n"
    );
    let old = git(&partial, &["show", "HEAD~3:src/lib.txt"]);
    assert_eq!(old, "version 2\n");

    // Push a commit; Wren takes it and the host follows.
    git(
        &clone,
        &["remote", "set-url", "--push", "origin", &write_url],
    );
    std::fs::write(clone.join("src/pushed.txt"), "pushed\n").unwrap();
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "-q", "-m", "pushed through wren"]);
    git(&clone, &["push", "-q", "origin", "main"]);
    let pushed = git(&clone, &["rev-parse", "HEAD"]).trim().to_string();
    let resolved = get(&format!("{url}/api/v1/repos/demo/resolve?rev=main"), "r");
    assert_eq!(resolved["oid"], pushed.as_str());
    wait_for(
        || git(&host, &["rev-parse", "main"]).trim() == pushed,
        "the host to follow",
    );
    git(&host, &["fsck", "--no-dangling"]);

    // A server-side land, then an incremental fetch.
    let out = Command::new("curl")
        .args(["-sf", "-X", "POST", "-H", "Authorization: Bearer w"])
        .args(["-H", "Content-Type: application/json"])
        .arg(format!("{url}/api/v1/repos/demo/changesets"))
        .arg("-d")
        .arg(
            serde_json::json!({
                "parents": ["main"],
                "author": {"name": "Agent", "email": "a@wren.dev"},
                "message": "landed by an agent",
                "changes": [{"path": "src/agent.txt", "content": "agent\n"}],
                "land": {"onto": "main"},
            })
            .to_string(),
        )
        .output()
        .unwrap();
    let landed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(landed["land"]["result"], "landed", "{landed}");
    git(&clone, &["fetch", "-q", "origin"]);
    assert_eq!(
        git(&clone, &["log", "-1", "--format=%s", "origin/main"]).trim(),
        "landed by an agent"
    );
    git(&clone, &["merge", "-q", "--ff-only", "origin/main"]);
    git(&partial, &["pull", "-q", "--ff-only"]);
    assert_eq!(
        std::fs::read_to_string(partial.join("src/agent.txt")).unwrap(),
        "agent\n"
    );

    // Protected main: no force push, no deletion.
    let (ok, _, err) = git_try(&clone, &["push", "-f", "origin", "HEAD~2:main"]);
    assert!(!ok && err.contains("protected"), "{err}");
    let (ok, _, err) = git_try(&clone, &["push", "origin", ":main"]);
    assert!(!ok && err.contains("protected"), "{err}");
    // The policy hook.
    let (ok, _, err) = git_try(&clone, &["push", "origin", "HEAD:refs/heads/forbidden/x"]);
    assert!(!ok && err.contains("forbidden by the hook"), "{err}");
    // Other branches: create, then delete.
    git(
        &clone,
        &["push", "-q", "origin", "HEAD~1:refs/heads/feature"],
    );
    wait_for(
        || git_try(&host, &["rev-parse", "--verify", "-q", "feature"]).0,
        "the new branch on the host",
    );
    git(&clone, &["push", "-q", "origin", ":feature"]);
    wait_for(
        || !git_try(&host, &["rev-parse", "--verify", "-q", "feature"]).0,
        "the deletion on the host",
    );
    // The read token cannot push.
    let (ok, _, err) = git_try(
        &clone,
        &[
            "push",
            &format!("http://x:r@127.0.0.1:{port}/git/demo"),
            "HEAD:refs/heads/other",
        ],
    );
    assert!(!ok && err.contains("403"), "{err}");

    // Every move is in the audit log.
    let audit = get(&format!("{url}/api/v1/repos/demo/audit"), "r");
    let vias: Vec<&str> = audit
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["via"].as_str().unwrap())
        .collect();
    assert!(vias.contains(&"push") && vias.contains(&"land"), "{audit}");

    // Export lag stays well under ten seconds.
    let info = get(&format!("{url}/api/v1/repos"), "r");
    let export = &info[0]["export"];
    assert_eq!(export["authoritative"], true);
    assert_eq!(export["pending"], 0, "{export}");
    assert!(export["lag_ms_p99"].as_u64().unwrap() < 10_000, "{export}");

    // Someone pushes to the host behind Wren's back: never overwritten,
    // reported instead.
    let rogue = root.join("rogue");
    git(&root, &["clone", "-q", host.to_str().unwrap(), "rogue"]);
    std::fs::write(rogue.join("rogue.txt"), "x\n").unwrap();
    git(&rogue, &["add", "-A"]);
    git(&rogue, &["commit", "-q", "-m", "behind wren's back"]);
    git(&rogue, &["push", "-q", "origin", "main"]);
    let rogue_tip = git(&rogue, &["rev-parse", "HEAD"]);
    std::fs::write(clone.join("src/next.txt"), "next\n").unwrap();
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "-q", "-m", "next"]);
    git(&clone, &["push", "-q", "origin", "main"]);
    wait_for(
        || {
            get(&format!("{url}/api/v1/repos"), "r")[0]["export"]["error"]
                .as_str()
                .is_some_and(|e| e.contains("pushed to it directly"))
        },
        "the export error",
    );
    assert_eq!(git(&host, &["rev-parse", "main"]), rogue_tip);
}

#[test]
fn push_in_write_proxy_mode_goes_to_the_host_first() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let host = setup(&root);
    let (_server, url) = server(&root, &host, &[]);
    let port = url.rsplit(':').next().unwrap();
    git(
        &root,
        &[
            "clone",
            "-q",
            &format!("http://x:w@127.0.0.1:{port}/git/demo"),
            "clone",
        ],
    );
    let clone = root.join("clone");
    std::fs::write(clone.join("proxied.txt"), "p\n").unwrap();
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "-q", "-m", "proxied"]);
    git(&clone, &["push", "-q", "origin", "main"]);
    // Synchronously on the host: it is still the source of truth.
    assert_eq!(
        git(&host, &["rev-parse", "main"]),
        git(&clone, &["rev-parse", "HEAD"])
    );
    let info = get(&format!("{url}/api/v1/repos"), "r");
    assert_eq!(info[0]["export"]["authoritative"], false);
    // A push the host refuses (stale) is refused by Wren too.
    let (ok, _, err) = git_try(&clone, &["push", "origin", "HEAD~3:main"]);
    assert!(!ok, "{err}");
}
