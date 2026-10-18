//! M6 through the server and the CLI: log --follow, rename-aware diffs,
//! path search, directory ACLs for restricted tokens, and rate limits.

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
        .env("GIT_TERMINAL_PROMPT", "0")
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

/// (status, body)
fn get(url: &str, token: &str) -> (u16, serde_json::Value) {
    let out = Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}"])
        .args(["-H", &format!("Authorization: Bearer {token}"), url])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = text.rsplit_once('\n').unwrap();
    (
        code.parse().unwrap(),
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    )
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

fn setup(root: &Path) -> std::path::PathBuf {
    let up = root.join("up");
    for d in ["docs", "src/public", "src/secret"] {
        std::fs::create_dir_all(up.join(d)).unwrap();
    }
    git(&up, &["init", "-q", "-b", "main"]);
    let body: String = (0..30)
        .map(|i| format!("line {i} of the guide\n"))
        .collect();
    std::fs::write(up.join("docs/guide.md"), &body).unwrap();
    std::fs::write(up.join("src/public/api.rs"), "// TOKEN public\n").unwrap();
    std::fs::write(up.join("src/secret/keys.rs"), "// TOKEN secret\n").unwrap();
    git(&up, &["add", "-A"]);
    git(&up, &["commit", "-q", "-m", "start"]);
    std::fs::write(up.join("docs/guide.md"), format!("{body}one more\n")).unwrap();
    git(&up, &["commit", "-qam", "edit the guide"]);
    // Rename with a small edit.
    git(&up, &["mv", "docs/guide.md", "docs/manual.md"]);
    std::fs::write(
        up.join("docs/manual.md"),
        format!("{body}one more\nand another\n"),
    )
    .unwrap();
    git(&up, &["commit", "-qam", "rename the guide"]);
    std::fs::write(up.join("src/secret/keys.rs"), "// TOKEN secret 2\n").unwrap();
    std::fs::write(up.join("docs/manual.md"), format!("{body}changed\n")).unwrap();
    git(&up, &["commit", "-qam", "touch both"]);
    up
}

#[test]
fn follow_renames_search_and_acls() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let up = setup(&root);
    let acl = root.join("acl.json");
    std::fs::write(
        &acl,
        serde_json::json!({"tokens": [
            {"name": "contractor", "token": "c", "repos": {"demo": ["docs", "src/public"]}}
        ]})
        .to_string(),
    )
    .unwrap();
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let _server = Proc(
        Command::new(WREN)
            .args(["server", "--data", root.join("data").to_str().unwrap()])
            .args(["--listen", &format!("127.0.0.1:{port}"), "--poll", "0"])
            .args(["--token", "r", "--acl", acl.to_str().unwrap()])
            .arg("--repo")
            .arg(format!("demo={}", up.display()))
            .env("WREN_LOG", "warn")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || get(&format!("{url}/api/v1/repos"), "r").1[0]["changesets"] == 4,
        "ingest",
    );
    let env = [
        ("WREN_SERVER", url.as_str()),
        ("WREN_REPO", "demo"),
        ("WREN_TOKEN", "r"),
    ];

    // log --follow crosses the rename, like git's.
    let (ok, out, err) = wren(&["log", "--follow", "--", "docs/manual.md"], &env);
    assert!(ok, "{err}");
    let ours: Vec<&str> = out
        .lines()
        .filter(|l| !l.starts_with("  "))
        .map(|l| l.split(' ').next().unwrap())
        .collect();
    let theirs: Vec<String> = git(
        &up,
        &["log", "--follow", "--format=%h", "--", "docs/manual.md"],
    )
    .lines()
    .map(|h| h.to_string())
    .collect();
    assert_eq!(ours.len(), theirs.len(), "{out}");
    for (o, t) in ours.iter().zip(&theirs) {
        assert!(
            o.starts_with(t.as_str()) || t.starts_with(o),
            "{out} vs {theirs:?}"
        );
    }
    assert!(out.contains("earlier: docs/guide.md"), "{out}");

    // diff -M names the rename with git's score.
    let (ok, out, _) = wren(&["diff", "main~1", "main~2", "--name-status", "-M"], &env);
    assert!(ok);
    let expect = git(&up, &["diff", "-M", "--name-status", "main~2", "main~1"]);
    assert_eq!(out.trim(), expect.trim());

    // Path search.
    let (ok, out, _) = wren(&["find", "KEYS"], &env);
    assert!(ok);
    assert_eq!(out.trim(), "src/secret/keys.rs");
    let (ok, out, _) = wren(&["find", "src"], &env);
    assert!(ok);
    assert!(
        out.contains("src/public/") && out.contains("src/secret/keys.rs"),
        "{out}"
    );

    // A restricted token.
    let api = format!("{url}/api/v1/repos/demo");
    let (code, v) = get(&format!("{url}/api/v1/repos"), "c");
    assert_eq!(code, 200);
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(
        get(&format!("{api}/file?rev=main&path=docs/manual.md"), "c").0,
        200
    );
    assert_eq!(
        get(&format!("{api}/file?rev=main&path=src/secret/keys.rs"), "c").0,
        404
    );
    assert_eq!(
        get(
            &format!("{api}/blame?rev=main&path=src/secret/keys.rs"),
            "c"
        )
        .0,
        404
    );
    let (code, root_tree) = get(&format!("{api}/tree?rev=main&path="), "c");
    assert_eq!(code, 200);
    let names: Vec<&str> = root_tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["docs", "src"]);
    let (_, src) = get(&format!("{api}/tree?rev=main&path=src"), "c");
    let names: Vec<&str> = src["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["public"]);
    assert_eq!(
        get(&format!("{api}/tree?rev=main&path=src/secret"), "c").0,
        404
    );
    let (_, grep) = get(&format!("{api}/grep?rev=main&pattern=TOKEN"), "c");
    let paths: Vec<&str> = grep["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["src/public/api.rs"]);
    let (code, diff) = get(&format!("{api}/diff?head=main"), "c");
    assert_eq!(code, 200);
    let changed: Vec<&str> = diff["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(changed, ["docs/manual.md"]);
    // The unrestricted token sees both.
    let (_, diff) = get(&format!("{api}/diff?head=main"), "r");
    assert_eq!(diff["changes"].as_array().unwrap().len(), 2);
    assert_eq!(get(&format!("{api}/log?rev=main"), "c").0, 403);
    assert_eq!(
        get(&format!("{api}/log?rev=main&path=src/secret"), "c").0,
        403
    );
    assert_eq!(get(&format!("{api}/log?rev=main&path=docs"), "c").0, 200);
    let (_, found) = get(&format!("{api}/paths?q=rs"), "c");
    let found: Vec<&str> = found["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["path"].as_str().unwrap())
        .collect();
    assert_eq!(found, ["src/public/api.rs"]);
    // Objects only with a scope, and only inside the directories at it.
    let objects = |body: serde_json::Value| -> String {
        let out = Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}"])
            .args([
                "-H",
                "Authorization: Bearer c",
                "-H",
                "Content-Type: application/json",
            ])
            .args(["-d", &body.to_string()])
            .arg(format!("{url}/v1/objects"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let head = git(&up, &["rev-parse", "main"]).trim().to_string();
    let oid = |spec: &str| {
        git(&up, &["rev-parse", &format!("main:{spec}")])
            .trim()
            .to_string()
    };
    let scope = serde_json::json!({"repo": "demo", "commit": head});
    assert_eq!(objects(serde_json::json!({"oids": []})), "403");
    for (spec, code) in [
        ("docs/manual.md", "200"),
        ("docs", "200"),
        ("src/public/api.rs", "200"),
        ("src/secret/keys.rs", "403"),
        ("src", "403"),
        ("", "403"),
    ] {
        assert_eq!(
            objects(serde_json::json!({"oids": [oid(spec)], "scope": scope})),
            code,
            "{spec:?}"
        );
    }
    assert_eq!(
        objects(serde_json::json!({"oids": [head], "scope": scope})),
        "200"
    );
    // A base outside the directories is ignored, not used for a delta.
    assert_eq!(
        objects(serde_json::json!({
            "oids": [oid("docs/manual.md")],
            "bases": [oid("src/secret/keys.rs")],
            "scope": scope,
        })),
        "200"
    );
    let one = |o: &str, q: &str| {
        let out = Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}"])
            .args(["-H", "Authorization: Bearer c"])
            .arg(format!("{url}/v1/objects/{o}{q}"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let q = format!("?repo=demo&commit={head}");
    assert_eq!(one(&oid("docs/manual.md"), ""), "403");
    assert_eq!(one(&oid("docs/manual.md"), &q), "200");
    assert_eq!(one(&oid("src/secret/keys.rs"), &q), "403");
    // So a restricted token can check out its directories.
    let renv = [
        ("WREN_SERVER", url.as_str()),
        ("WREN_REPO", "demo"),
        ("WREN_TOKEN", "c"),
    ];
    let co = root.join("co");
    let store = root.join("store");
    let (ok, _, err) = wren(
        &[
            "checkout",
            co.to_str().unwrap(),
            "--rev",
            "main",
            "--store",
            store.to_str().unwrap(),
        ],
        &renv,
    );
    assert!(ok, "{err}");
    assert_eq!(
        std::fs::read_to_string(co.join("docs/manual.md")).unwrap(),
        git(&up, &["show", "main:docs/manual.md"])
    );
    assert!(co.join("src/public/api.rs").exists());
    assert!(!co.join("src/secret").exists());
    let (ok, _, err) = wren(
        &[
            "checkout",
            root.join("co2").to_str().unwrap(),
            "--rev",
            "main",
            "--sparse",
            "src/secret",
            "--store",
            store.to_str().unwrap(),
        ],
        &renv,
    );
    assert!(!ok && err.contains("may not read"), "{err}");
    // Not git.
    let clone = Command::new("git")
        .args(["-c", "protocol.version=2", "clone", "-q"])
        .arg(format!("http://x:c@127.0.0.1:{port}/git/demo"))
        .arg(root.join("clone"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(!clone.status.success());
}

#[test]
fn rate_limits_per_client() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let up = setup(&root);
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let _server = Proc(
        Command::new(WREN)
            .args(["server", "--data", root.join("data").to_str().unwrap()])
            .args(["--listen", &format!("127.0.0.1:{port}"), "--poll", "0"])
            .args(["--rate-limit", "5"])
            .arg("--repo")
            .arg(format!("demo={}", up.display()))
            .env("WREN_LOG", "warn")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || {
            Command::new("curl")
                .args(["-sf", &format!("{url}/healthz")])
                .status()
                .unwrap()
                .success()
        },
        "server",
    );
    std::thread::sleep(Duration::from_millis(500));
    // A burst of 30 from one client: about 10 pass (the burst), the rest
    // are told when to come back.
    // (Retry-After is checked on the refusals themselves: a separate request
    // afterwards may find the bucket refilled on a slow machine.)
    let replies: Vec<String> = (0..30)
        .map(|_| {
            let out = Command::new("curl")
                .args(["-s", "-i", &format!("{url}/api/v1/repos")])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).to_lowercase()
        })
        .collect();
    let limited: Vec<&String> = replies
        .iter()
        .filter(|r| r.starts_with("http/1.1 429"))
        .collect();
    assert!((10..30).contains(&limited.len()), "{replies:?}");
    assert!(
        limited.iter().all(|r| r.contains("retry-after")),
        "{limited:?}"
    );
    // Another client (a token) has its own bucket.
    let out = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}"])
        .args(["-H", "Authorization: Bearer someone-else"])
        .arg(format!("{url}/api/v1/repos"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "200");
}
