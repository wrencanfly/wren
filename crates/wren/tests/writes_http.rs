//! End-to-end tests of the write API over HTTP: a bare git repository as the
//! host, `wren server` mirroring it with a write token, and curl as the
//! client. Every ref move must reach the host first.

use std::io::Write as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

const WREN: &str = env!("CARGO_BIN_EXE_wren");
const READ: &str = "read-t0ken";
const WRITE: &str = "write-t0ken";

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

fn git_out(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=E2E",
            "-c",
            "user.email=e2e@wren.dev",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let (ok, out) = git_out(dir, args);
    assert!(ok, "git {args:?}: {out}");
    out.trim().to_string()
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

struct Resp {
    status: u16,
    headers: String,
    body: Value,
}

impl std::fmt::Debug for Resp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, self.body)
    }
}

/// One HTTP request with curl. `body` is sent as JSON.
fn http(method: &str, url: &str, token: Option<&str>, body: Option<&Value>) -> Resp {
    let tmp = tempfile::tempdir().unwrap();
    let (hfile, bfile) = (tmp.path().join("h"), tmp.path().join("b"));
    let mut c = Command::new("curl");
    c.args(["-s", "-X", method, "-H", "Expect:", "-w", "%{http_code}"])
        .arg("-D")
        .arg(&hfile)
        .arg("-o")
        .arg(&bfile);
    if let Some(t) = token {
        c.args(["-H", &format!("Authorization: Bearer {t}")]);
    }
    if body.is_some() {
        c.args([
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
        ]);
    }
    c.arg(url).stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = c.spawn().unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        if let Some(b) = body {
            stdin.write_all(b.to_string().as_bytes()).unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    let status: u16 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    let raw = std::fs::read(&bfile).unwrap_or_default();
    Resp {
        status,
        headers: std::fs::read_to_string(&hfile)
            .unwrap_or_default()
            .to_lowercase(),
        body: serde_json::from_slice(&raw)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&raw).into_owned())),
    }
}

/// A bare host repository with one commit on main.
fn make_host(root: &Path) -> PathBuf {
    let host = root.join("host.git");
    let work = root.join("work");
    git(
        root,
        &["init", "-q", "--bare", "-b", "main", host.to_str().unwrap()],
    );
    git(
        root,
        &[
            "clone",
            "-q",
            host.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    let write = |p: &str, s: &str| {
        let p = work.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    };
    write("README.md", "# demo\n\nline one\nline two\nline three\n");
    write("src/lib.rs", "pub fn a() {}\n\npub fn b() {}\n");
    write("old.txt", "going away\n");
    write("tools/run.sh", "#!/bin/sh\necho run\n");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        work.join("tools/run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "initial import"]);
    git(&work, &["push", "-q", "origin", "main"]);
    host
}

struct Server {
    _proc: Proc,
    url: String,
}

fn start_server(root: &Path, host: &Path, write_token: Option<&str>) -> Server {
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let mut c = Command::new(WREN);
    c.args([
        "server",
        "--data",
        root.join("data").to_str().unwrap(),
        "--listen",
        &format!("127.0.0.1:{port}"),
        "--poll",
        "0",
        "--token",
        READ,
    ])
    .arg("--repo")
    .arg(format!("demo={}", host.display()))
    .env("WREN_LOG", "warn")
    .env_remove("WREN_WRITE_TOKEN");
    if let Some(t) = write_token {
        c.args(["--write-token", t]);
    }
    let proc = Proc(c.spawn().unwrap());
    wait_for(
        || {
            let r = http(
                "GET",
                &format!("{url}/api/v1/repos/demo/refs"),
                Some(READ),
                None,
            );
            r.status == 200 && r.body.to_string().contains("refs/heads/main")
        },
        "server up and synced",
    );
    Server { _proc: proc, url }
}

fn author() -> Value {
    json!({"name": "Agent", "email": "agent@wren.dev", "time": 1_700_000_000, "tz": "+0100"})
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}"))
}

#[test]
fn writes_are_disabled_without_a_write_token() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let host = make_host(&root);
    let srv = start_server(&root, &host, None);

    let caps = http(
        "GET",
        &format!("{}/v1/capabilities", srv.url),
        Some(READ),
        None,
    );
    assert_eq!(caps.status, 200);
    assert!(
        !caps.body["features"].to_string().contains("write.v1"),
        "{caps:?}"
    );

    for endpoint in ["blobs", "changesets", "land", "merge", "refs", "revert"] {
        let r = http(
            "POST",
            &format!("{}/api/v1/repos/demo/{endpoint}", srv.url),
            Some(READ),
            Some(&json!({})),
        );
        assert_eq!(r.status, 403, "{endpoint}: {r:?}");
        assert_eq!(r.body["code"], "forbidden", "{r:?}");
        assert!(s(&r.body["error"]).contains("--write-token"), "{r:?}");
        assert!(
            r.headers.contains("cache-control: no-store"),
            "{}",
            r.headers
        );
    }
    // Reads are unaffected.
    let r = http(
        "GET",
        &format!("{}/api/v1/repos/demo/refs", srv.url),
        Some(READ),
        None,
    );
    assert_eq!(r.status, 200);
}

#[test]
fn write_api_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let host = make_host(&root);
    let srv = start_server(&root, &host, Some(WRITE));
    let api = |p: &str| format!("{}/api/v1/repos/demo/{p}", srv.url);
    let post = |p: &str, body: Value| http("POST", &api(p), Some(WRITE), Some(&body));
    let host_main = || git(&host, &["rev-parse", "main"]);
    let base = host_main();

    // Capabilities advertise writes; the write token also reads.
    let caps = http(
        "GET",
        &format!("{}/v1/capabilities", srv.url),
        Some(WRITE),
        None,
    );
    assert_eq!(caps.status, 200);
    assert!(
        caps.body["features"].to_string().contains("\"write.v1\""),
        "{caps:?}"
    );
    let r = http("GET", &api("resolve?rev=main"), Some(WRITE), None);
    assert_eq!((r.status, s(&r.body["oid"])), (200, base.as_str()));

    // Auth: no token 401, read token 403, write token OK.
    let blobs = json!({"blobs": [
        {"content": "hello from an agent\n"},
        {"content_base64": base64::engine::general_purpose::STANDARD.encode([0u8, 159, 146, 150, 255])},
    ]});
    let r = http("POST", &api("blobs"), None, Some(&blobs));
    assert_eq!(
        (r.status, &r.body["code"]),
        (401, &json!("unauthorized")),
        "{r:?}"
    );
    let r = http("POST", &api("blobs"), Some(READ), Some(&blobs));
    assert_eq!(
        (r.status, &r.body["code"]),
        (403, &json!("forbidden")),
        "{r:?}"
    );
    let r = http("POST", &api("blobs"), Some("wrong"), Some(&blobs));
    assert_eq!(r.status, 401, "{r:?}");
    let r = post("blobs", blobs);
    assert_eq!(r.status, 200, "{r:?}");
    assert!(
        r.headers.contains("cache-control: no-store"),
        "{}",
        r.headers
    );
    let text_blob = s(&r.body["oids"][0]).to_string();
    let bin_blob = s(&r.body["oids"][1]).to_string();
    let hashed = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            c.stdin
                .take()
                .unwrap()
                .write_all(b"hello from an agent\n")?;
            c.wait_with_output()
        })
        .unwrap();
    assert_eq!(text_blob, String::from_utf8_lossy(&hashed.stdout).trim());

    // Create a changeset: no ref moves.
    let r = post(
        "changesets",
        json!({
            "parents": ["main"],
            "author": author(),
            "message": "Agent change",
            "changes": [
                {"path": "src/new.txt", "blob": text_blob},
                {"path": "data/bin.dat", "blob": bin_blob},
                {"path": "README.md", "content": "# demo\n\nline one\nline two\nline three\nline four\n"},
                {"path": "tools/run.sh", "content": "#!/bin/sh\necho v2\n"},
                {"path": "tools/new.sh", "content": "#!/bin/sh\n", "mode": "100755"},
                {"path": "old.txt", "delete": true},
            ],
        }),
    );
    assert_eq!(r.status, 201, "{r:?}");
    assert!(
        r.headers.contains("cache-control: no-store"),
        "{}",
        r.headers
    );
    assert!(r.body.get("land").is_none(), "{r:?}");
    let c1 = s(&r.body["oid"]).to_string();
    let tree1 = s(&r.body["tree"]).to_string();
    assert_eq!(host_main(), base, "creating a changeset moves no ref");

    // Land it: the host is updated, and Wren's ref follows.
    let r = post("land", json!({"changesets": [c1], "onto": "main"}));
    assert_eq!(r.status, 200, "{r:?}");
    assert!(
        r.headers.contains("cache-control: no-store"),
        "{}",
        r.headers
    );
    assert_eq!(r.body["result"], "landed", "{r:?}");
    assert_eq!(r.body["ref"], "refs/heads/main");
    assert_eq!(
        (s(&r.body["old"]), s(&r.body["new"])),
        (base.as_str(), c1.as_str())
    );
    assert_eq!(host_main(), c1);
    assert_eq!(git(&host, &["rev-parse", "main^{tree}"]), tree1);
    assert_eq!(
        git(&host, &["log", "-1", "--format=%s|%an|%ae|%at|%ai", "main"]),
        "Agent change|Agent|agent@wren.dev|1700000000|2023-11-14 23:13:20 +0100"
    );
    assert_eq!(git(&host, &["rev-parse", "main~1"]), base);
    assert_eq!(
        git(&host, &["show", "main:src/new.txt"]),
        "hello from an agent"
    );
    let modes = git(&host, &["ls-tree", "-r", "main"]);
    assert!(
        modes.contains("100755 blob") && modes.contains("\ttools/run.sh"),
        "{modes}"
    );
    for l in modes.lines() {
        if l.ends_with("\ttools/run.sh") || l.ends_with("\ttools/new.sh") {
            assert!(l.starts_with("100755 "), "{l}");
        }
        if l.ends_with("\tsrc/new.txt") {
            assert!(l.starts_with("100644 "), "{l}");
        }
    }
    assert!(!modes.contains("old.txt"), "{modes}");
    assert!(!git_out(&host, &["fsck", "--strict"]).1.contains("error"));
    let r = http("GET", &api("resolve?rev=main"), Some(READ), None);
    assert_eq!(s(&r.body["oid"]), c1);

    // Create and land in one call.
    let r = post(
        "changesets",
        json!({
            "parents": ["main"],
            "author": author(),
            "committer": {"name": "Lander", "email": "lander@wren.dev", "time": 1_700_000_100},
            "message": "One-shot",
            "changes": [{"path": "src/lib.rs", "content": "pub fn a() {}\n\npub fn b() {}\n\npub fn c() {}\n"}],
            "land": {"onto": "main"},
        }),
    );
    assert_eq!(r.status, 200, "{r:?}");
    let c2 = s(&r.body["oid"]).to_string();
    assert!(r.body["tree"].is_string());
    assert_eq!(r.body["land"]["result"], "landed", "{r:?}");
    assert_eq!(s(&r.body["land"]["new"]), c2);
    assert_eq!(host_main(), c2);
    assert_eq!(
        git(&host, &["log", "-1", "--format=%cn|%ci", "main"]),
        "Lander|2023-11-14 22:15:00 +0000"
    );

    // A stale changeset touching another file is rebased onto the tip.
    let r = post(
        "changesets",
        json!({
            "parents": [base],
            "author": author(),
            "message": "From an old base",
            "changes": [{"path": "notes/todo.md", "content": "- ship it\n"}],
        }),
    );
    assert_eq!(r.status, 201, "{r:?}");
    let stale = s(&r.body["oid"]).to_string();
    let r = post(
        "land",
        json!({"changesets": [stale], "onto": "refs/heads/main"}),
    );
    assert_eq!(
        (r.status, &r.body["result"]),
        (200, &json!("landed")),
        "{r:?}"
    );
    let c3 = s(&r.body["new"]).to_string();
    assert_ne!(c3, stale, "rebased");
    assert_eq!(host_main(), c3);
    assert_eq!(git(&host, &["rev-parse", "main~1"]), c2);
    assert_eq!(git(&host, &["show", "main:notes/todo.md"]), "- ship it");

    // A conflicting edit from the old base: 409 with the conflicts.
    let r = post(
        "changesets",
        json!({
            "parents": [base],
            "author": author(),
            "message": "Conflicting",
            "changes": [{"path": "README.md", "content": "# demo\n\nline one\nline two\nline three\nsomething else\n"}],
            "land": {"onto": "main"},
        }),
    );
    assert_eq!(r.status, 409, "{r:?}");
    assert!(r.body["oid"].is_string(), "the changeset is still created");
    assert_eq!(r.body["land"]["result"], "conflicts", "{r:?}");
    assert_eq!(s(&r.body["land"]["onto"]), c3);
    let conflicts = r.body["land"]["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "{r:?}");
    assert_eq!(conflicts[0]["path"], "README.md");
    assert_eq!(conflicts[0]["kind"], "content");
    let conflicting = s(&r.body["oid"]).to_string();
    let r = post("land", json!({"changesets": [conflicting], "onto": "main"}));
    assert_eq!(
        (r.status, &r.body["result"]),
        (409, &json!("conflicts")),
        "{r:?}"
    );
    assert_eq!(host_main(), c3, "nothing landed");

    // Ref compare-and-swap: a stale expectation is refused with the current value.
    let r = post("refs", json!({"name": "main", "expected": base, "new": c1}));
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["result"], "stale");
    assert_eq!(s(&r.body["current"]), c3);
    assert_eq!(host_main(), c3);
    // Create a branch (expected null), move it, delete it.
    let r = post(
        "refs",
        json!({"name": "topic", "expected": null, "new": c1}),
    );
    assert_eq!(
        (r.status, &r.body["result"]),
        (200, &json!("updated")),
        "{r:?}"
    );
    assert_eq!(git(&host, &["rev-parse", "topic"]), c1);
    let r = post(
        "refs",
        json!({"name": "topic", "expected": null, "new": c2}),
    );
    assert_eq!(
        (r.status, &r.body["result"]),
        (409, &json!("stale")),
        "{r:?}"
    );
    let r = post(
        "refs",
        json!({"name": "refs/heads/topic", "expected": c1, "new": c2}),
    );
    assert_eq!(
        (r.status, &r.body["result"]),
        (200, &json!("updated")),
        "{r:?}"
    );
    assert_eq!(git(&host, &["rev-parse", "topic"]), c2);
    let r = post(
        "refs",
        json!({"name": "topic", "expected": c2, "new": null}),
    );
    assert_eq!(
        (r.status, &r.body["result"]),
        (200, &json!("updated")),
        "{r:?}"
    );
    assert!(!git_out(&host, &["rev-parse", "--verify", "-q", "refs/heads/topic"]).0);

    // Revert without onto: a changeset on top of the reverted one, no ref moves.
    let r = post("revert", json!({"commit": c2, "author": author()}));
    assert_eq!(r.status, 201, "{r:?}");
    assert!(r.body.get("land").is_none(), "{r:?}");
    let rv = s(&r.body["oid"]).to_string();
    let r = http("GET", &api(&format!("commits/{rv}")), Some(READ), None);
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(host_main(), c3);

    // Revert with onto: lands on main.
    let r = post(
        "revert",
        json!({"commit": c1, "author": author(), "onto": "main"}),
    );
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["land"]["result"], "landed", "{r:?}");
    let c4 = s(&r.body["land"]["new"]).to_string();
    assert_eq!(host_main(), c4);
    assert!(
        git(&host, &["log", "-1", "--format=%s", "main"]).starts_with("Revert \"Agent change\"")
    );
    assert!(!git_out(&host, &["cat-file", "-e", "main:src/new.txt"]).0);
    assert_eq!(git(&host, &["show", "main:old.txt"]), "going away");
    assert_eq!(git(&host, &["show", "main:notes/todo.md"]), "- ship it");

    // The host's history is linear and Wren agrees with it.
    let log = git(&host, &["log", "--format=%H", "main"]);
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        [
            c4.as_str(),
            c3.as_str(),
            c2.as_str(),
            c1.as_str(),
            base.as_str()
        ]
    );
    let r = http("GET", &api("log?rev=main&limit=10"), Some(READ), None);
    let wren_log: Vec<&str> = r.body["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| s(&c["oid"]))
        .collect();
    assert_eq!(wren_log, log.lines().collect::<Vec<_>>());

    // Request errors.
    let r = post(
        "changesets",
        json!({"parents": ["main"], "author": author(), "message": "x",
               "changes": [{"path": "a", "content": "x", "delete": true}]}),
    );
    assert_eq!(
        (r.status, &r.body["code"]),
        (400, &json!("bad_request")),
        "{r:?}"
    );
    let r = post(
        "changesets",
        json!({"parents": [], "author": author(), "message": "x", "changes": []}),
    );
    assert_eq!(
        (r.status, &r.body["code"]),
        (400, &json!("bad_request")),
        "{r:?}"
    );
    let r = post(
        "changesets",
        json!({"parents": ["main"], "author": author(), "message": "x",
               "changes": [{"path": "../escape", "content": "x"}]}),
    );
    assert_eq!(
        (r.status, &r.body["code"]),
        (400, &json!("bad_request")),
        "{r:?}"
    );
    let r = post(
        "changesets",
        json!({"parents": ["no-such-branch"], "author": author(), "message": "x", "changes": []}),
    );
    assert_eq!(
        (r.status, &r.body["code"]),
        (404, &json!("not_found")),
        "{r:?}"
    );
    let r = post(
        "land",
        json!({"changesets": [c1], "onto": "no-such-branch"}),
    );
    assert_eq!(
        (r.status, &r.body["code"]),
        (404, &json!("not_found")),
        "{r:?}"
    );
    let r = http("POST", &api("land"), Some(WRITE), None);
    assert_eq!(
        (r.status, &r.body["code"]),
        (400, &json!("bad_request")),
        "{r:?}"
    );
    assert_eq!(host_main(), c4);

    // Bodies up to 64 MiB are accepted (well past axum's 2 MiB default);
    // larger ones are refused with a JSON error.
    let big = "x".repeat(8 << 20);
    let r = post("blobs", json!({"blobs": [{"content": big}]}));
    assert_eq!(r.status, 200, "{}", r.status);
    let huge = "x".repeat(65 << 20);
    let r = post("blobs", json!({"blobs": [{"content": huge}]}));
    assert_eq!(
        (r.status, &r.body["code"]),
        (413, &json!("too_large")),
        "{}",
        r.status
    );
}

#[test]
fn caches_refuse_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let host = make_host(&root);
    let srv = start_server(&root, &host, Some(WRITE));
    let base = git(&host, &["rev-parse", "main"]);

    // Worst case: the cache holds the write token for its upstream.
    let port = free_port();
    let cache = format!("http://127.0.0.1:{port}");
    let _cache = Proc(
        Command::new(WREN)
            .args([
                "cache",
                "--upstream",
                &srv.url,
                "--dir",
                root.join("cache").to_str().unwrap(),
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--upstream-token",
                WRITE,
            ])
            .env("WREN_LOG", "warn")
            .env_remove("WREN_TOKEN")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || {
            http(
                "GET",
                &format!("{cache}/api/v1/repos/demo/refs"),
                None,
                None,
            )
            .status
                == 200
        },
        "cache up",
    );
    let caps = http("GET", &format!("{cache}/v1/capabilities"), None, None);
    assert!(
        !caps.body["features"].to_string().contains("write.v1"),
        "{caps:?}"
    );

    let change = json!({
        "parents": ["main"],
        "author": author(),
        "message": "via cache",
        "changes": [{"path": "x.txt", "content": "x\n"}],
        "land": {"onto": "main"},
    });
    for (method, endpoint, body) in [
        ("POST", "changesets", change.clone()),
        ("POST", "blobs", json!({"blobs": [{"content": "x"}]})),
        (
            "POST",
            "land",
            json!({"changesets": [base], "onto": "main"}),
        ),
        (
            "POST",
            "refs",
            json!({"name": "main", "expected": base, "new": null}),
        ),
        (
            "POST",
            "revert",
            json!({"commit": base, "author": author()}),
        ),
        ("PUT", "refs", json!({})),
        ("DELETE", "refs", json!({})),
    ] {
        let r = http(
            method,
            &format!("{cache}/api/v1/repos/demo/{endpoint}"),
            Some(WRITE),
            Some(&body),
        );
        assert_eq!(r.status, 403, "{method} {endpoint}: {r:?}");
        assert_eq!(r.body["code"], "forbidden");
        assert!(
            s(&r.body["error"]).contains("directly to the server"),
            "{r:?}"
        );
    }
    assert_eq!(git(&host, &["rev-parse", "main"]), base);

    // Reads and the sync trigger still pass through.
    let r = http(
        "POST",
        &format!("{cache}/api/v1/repos/demo/sync"),
        None,
        None,
    );
    assert_eq!(r.status, 200, "{r:?}");
    let r = http(
        "GET",
        &format!("{cache}/api/v1/repos/demo/resolve?rev=main"),
        None,
        None,
    );
    assert_eq!((r.status, s(&r.body["oid"])), (200, base.as_str()));

    // The same write sent directly to the server works.
    let r = http(
        "POST",
        &format!("{}/api/v1/repos/demo/changesets", srv.url),
        Some(WRITE),
        Some(&change),
    );
    assert_eq!(
        (r.status, &r.body["land"]["result"]),
        (200, &json!("landed")),
        "{r:?}"
    );
    assert_eq!(git(&host, &["rev-parse", "main"]), s(&r.body["oid"]));
}

#[test]
fn merges_over_http_and_the_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let host = make_host(&root);
    let srv = start_server(&root, &host, Some(WRITE));
    let api = |p: &str| format!("{}/api/v1/repos/demo/{p}", srv.url);
    let post = |p: &str, body: Value| http("POST", &api(p), Some(WRITE), Some(&body));
    let commit = |parent: &str, path: &str, content: &str| -> String {
        let r = post(
            "changesets",
            json!({"parents": [parent], "author": author(), "message": format!("edit {path}"),
                   "changes": [{"path": path, "content": content}]}),
        );
        assert_eq!(r.status, 201, "{r:?}");
        s(&r.body["oid"]).to_string()
    };
    let base = git(&host, &["rev-parse", "main"]);
    // A topic branch and main both move, touching different lines.
    let t1 = commit(
        &base,
        "README.md",
        "# demo\n\nline one\nline two\nline three, topic\n",
    );
    let r = post(
        "refs",
        json!({"name": "refs/heads/topic", "expected": null, "new": t1}),
    );
    assert_eq!(r.status, 200, "{r:?}");
    let m1 = commit(
        &base,
        "README.md",
        "# demo, main\n\nline one\nline two\nline three\n",
    );
    let r = post("land", json!({"changesets": [m1], "onto": "main"}));
    assert_eq!(r.status, 200, "{r:?}");

    let r = post(
        "merge",
        json!({"commit": "topic", "into": "main", "author": author()}),
    );
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["result"], "merged", "{r:?}");
    assert_eq!(r.body["fast_forward"], false, "{r:?}");
    let merged = s(&r.body["new"]).to_string();
    // The host has it: two parents, both edits, git's merge.
    assert_eq!(git(&host, &["rev-parse", "main"]), merged);
    assert_eq!(
        git(&host, &["log", "-1", "--format=%P", "main"]),
        format!("{m1} {t1}")
    );
    assert_eq!(
        git(&host, &["show", "main:README.md"]),
        "# demo, main\n\nline one\nline two\nline three, topic"
    );
    let (ok, expect) = git_out(&host, &["merge-tree", "--write-tree", &m1, &t1]);
    assert!(ok);
    assert_eq!(git(&host, &["rev-parse", "main^{tree}"]), expect.trim());
    // Merging it again changes nothing.
    let r = post(
        "merge",
        json!({"commit": t1, "into": "main", "author": author()}),
    );
    assert_eq!(
        (r.status, &r.body["result"]),
        (200, &json!("up_to_date")),
        "{r:?}"
    );

    // Conflicts: nothing moves, the path is named.
    let t2 = commit(&merged, "src/lib.rs", "pub fn a() { 1 }\n\npub fn b() {}\n");
    let m2 = commit(&merged, "src/lib.rs", "pub fn a() { 2 }\n\npub fn b() {}\n");
    let r = post("land", json!({"changesets": [m2], "onto": "main"}));
    assert_eq!(r.status, 200, "{r:?}");
    let r = post(
        "merge",
        json!({"commit": t2, "into": "main", "author": author()}),
    );
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["result"], "conflicts", "{r:?}");
    assert_eq!(r.body["conflicts"][0]["path"], "src/lib.rs", "{r:?}");
    assert_eq!(git(&host, &["rev-parse", "main"]), m2);

    // The CLI, fast-forwarding when allowed.
    let ahead = commit(&m2, "notes.txt", "later\n");
    let env = [
        ("WREN_SERVER", srv.url.as_str()),
        ("WREN_REPO", "demo"),
        ("WREN_TOKEN", READ),
        ("WREN_WRITE_TOKEN", WRITE),
        ("WREN_AUTHOR", "Agent <agent@wren.dev>"),
        ("WREN_LOG", "warn"),
    ];
    let out = Command::new(WREN)
        .args(["merge", &ahead, "--into", "main", "--ff"])
        .envs(env)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("(fast-forward)"));
    assert_eq!(git(&host, &["rev-parse", "main"]), ahead);
    let out = Command::new(WREN)
        .args(["merge", &t2, "--into", "main", "-m", "try again"])
        .envs(env)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("src/lib.rs"));
}
