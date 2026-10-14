//! End-to-end: upstream git repo -> wren server -> wren cache -> wren CLI,
//! including a sparse `--git` checkout that stock git can commit and push from.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn wren(args: &[&str], env: &[(&str, &str)], cwd: Option<&Path>) -> (bool, String, String) {
    let mut c = Command::new(WREN);
    c.args(args).env("WREN_LOG", "warn");
    for (k, v) in env {
        c.env(k, v);
    }
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    let out = c.output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn wren_ok(args: &[&str], env: &[(&str, &str)], cwd: Option<&Path>) -> String {
    let (ok, out, err) = wren(args, env, cwd);
    assert!(ok, "wren {args:?} failed: {err}");
    out
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

fn make_upstream(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    let write = |p: &str, s: &str| {
        let p = dir.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    };
    write("README.md", "# demo\n");
    write(
        "services/api/main.rs",
        "fn main() {\n    println!(\"api\");\n}\n",
    );
    write("services/api/lib.rs", "pub fn api() {}\n");
    write("services/web/index.ts", "export const web = 1;\n");
    write("services/top.txt", "top of services\n");
    write("research/big/model.py", &"weights = [0.0]\n".repeat(5000));
    write("research/notes.md", "notes\n");
    write("tools/run.sh", "#!/bin/sh\necho run\n");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        dir.join("tools/run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("services/api/main.rs", dir.join("entry.rs")).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "initial import"]);
    git(dir, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(
        dir.join("services/api/lib.rs"),
        "pub fn api() {}\npub fn health() -> bool { true }\n",
    )
    .unwrap();
    git(dir, &["commit", "-q", "-am", "add health endpoint"]);
    git(dir, &["checkout", "-q", "main"]);
    std::fs::write(dir.join("research/notes.md"), "notes\nmore notes\n").unwrap();
    git(dir, &["commit", "-q", "-am", "research notes"]);
    git(
        dir,
        &["merge", "-q", "--no-ff", "-m", "Merge feature", "feature"],
    );
    git(dir, &["tag", "-a", "-m", "v1", "v1"]);
    // A run of commits for bisection: the build "breaks" at step 5.
    for i in 1..=7 {
        let state = if i >= 5 { "broken" } else { "ok" };
        std::fs::write(dir.join("status.txt"), format!("{state} {i}\n")).unwrap();
        git(dir, &["add", "status.txt"]);
        git(dir, &["commit", "-q", "-m", &format!("step {i}")]);
    }
    // Let the test push to main from a checkout.
    git(
        dir,
        &["config", "receive.denyCurrentBranch", "updateInstead"],
    );
}

fn files_under(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            let name = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
            if name == ".git" || name == ".wren" {
                continue;
            }
            let ft = e.file_type().unwrap();
            if ft.is_symlink() {
                out.push((
                    name,
                    std::fs::read_link(&p)
                        .unwrap()
                        .to_string_lossy()
                        .as_bytes()
                        .to_vec(),
                ));
            } else if ft.is_dir() {
                stack.push(p);
            } else {
                out.push((name, std::fs::read(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn end_to_end() {
    // Any git command below that needs an object Wren did not provide fails
    // instead of silently fetching it from the origin.
    std::env::set_var("GIT_NO_LAZY_FETCH", "1");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let up = root.join("upstream");
    make_upstream(&up);

    // Tier 1: server.
    let sport = free_port();
    let server_url = format!("http://127.0.0.1:{sport}");
    let _server = Proc(
        Command::new(WREN)
            .args([
                "server",
                "--data",
                root.join("data").to_str().unwrap(),
                "--listen",
                &format!("127.0.0.1:{sport}"),
                "--poll",
                "0",
                "--token",
                "s3cret-server",
            ])
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
                &[("WREN_TOKEN", "s3cret-server")],
                None,
            )
            .1
            .contains("\"changesets\": 11")
        },
        "initial sync",
    );

    // Tier 2: cache in front of the server. Everything below talks to it.
    let cport = free_port();
    let cache_url = format!("http://127.0.0.1:{cport}");
    let _cache = Proc(
        Command::new(WREN)
            .args([
                "cache",
                "--upstream",
                &server_url,
                "--dir",
                root.join("cache").to_str().unwrap(),
                "--listen",
                &format!("127.0.0.1:{cport}"),
                "--token",
                "s3cret-cache",
                "--upstream-token",
                "s3cret-server",
            ])
            .env("WREN_LOG", "warn")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || {
            wren(
                &["repos", "--server", &cache_url],
                &[("WREN_TOKEN", "s3cret-cache")],
                None,
            )
            .0
        },
        "cache up",
    );
    let env = [
        ("WREN_SERVER", cache_url.as_str()),
        ("WREN_REPO", "demo"),
        ("WREN_TOKEN", "s3cret-cache"),
    ];

    // Without the token, nothing but /healthz is served, on either tier.
    for url in [&server_url, &cache_url] {
        let (ok, _, err) = wren(
            &["repos", "--server", url],
            &[("WREN_TOKEN", "wrong")],
            None,
        );
        assert!(!ok && err.contains("401"), "{err}");
    }
    let health = Command::new("curl")
        .args(["-s", &format!("{server_url}/healthz")])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&health.stdout), "ok");

    // The mirror only keeps refs: fetched packs were absorbed and deleted.
    let packs = std::fs::read_dir(root.join("data/repos/demo/mirror.git/objects/pack"))
        .unwrap()
        .count();
    assert_eq!(packs, 0, "mirror should hold no packfiles");

    // Full checkout matches git's own working tree byte for byte.
    let store1 = root.join("node1-store");
    let full = root.join("full");
    wren_ok(
        &[
            "checkout",
            full.to_str().unwrap(),
            "--store",
            store1.to_str().unwrap(),
        ],
        &env,
        None,
    );
    let mut expect = files_under(&up);
    expect.retain(|(n, _)| !n.starts_with(".git"));
    assert_eq!(files_under(&full), expect);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(full.join("tools/run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111
    );

    // Sparse --git checkout: only services/api is fetched and written.
    let store2 = root.join("node2-store");
    let ws = root.join("ws");
    let (_, _, report) = {
        let r = wren(
            &[
                "checkout",
                ws.to_str().unwrap(),
                "--git",
                "--sparse",
                "services/api",
                "--store",
                store2.to_str().unwrap(),
            ],
            &env,
            None,
        );
        assert!(r.0, "{}", r.2);
        r
    };
    assert!(report.contains("checked out"), "{report}");
    assert!(ws.join("services/api/main.rs").exists());
    assert!(
        ws.join("services/top.txt").exists(),
        "cone mode includes parent-directory files"
    );
    assert!(ws.join("README.md").exists());
    assert!(!ws.join("research").exists());
    assert!(!ws.join("services/web").exists());
    // The big research blob never left the server.
    let model_oid = git(&up, &["rev-parse", "HEAD:research/big/model.py"]);
    let model_oid = model_oid.trim();
    assert!(!store2.join(&model_oid[..2]).join(&model_oid[2..]).exists());

    // Stock git sees a clean, sparse working tree…
    assert_eq!(git(&ws, &["status", "--porcelain"]), "");
    assert_eq!(
        git(&ws, &["rev-parse", "HEAD"]).trim(),
        git(&up, &["rev-parse", "main"]).trim()
    );
    assert_eq!(
        git(&ws, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    // A sparse index: whole directories outside the cone are single entries.
    let ls = git(&ws, &["ls-files", "--sparse", "-t"]);
    assert!(ls.contains("S research/\n"), "{ls}");
    assert!(ls.contains("S services/web/\n"), "{ls}");
    assert!(ls.contains("H services/api/main.rs"), "{ls}");
    // …can commit and push back upstream…
    std::fs::write(
        ws.join("services/api/main.rs"),
        "fn main() {\n    println!(\"api v2\");\n}\n",
    )
    .unwrap();
    assert_eq!(
        git(&ws, &["status", "--porcelain"]),
        " M services/api/main.rs\n"
    );
    git(&ws, &["commit", "-q", "-am", "agent: bump api"]);
    git(&ws, &["push", "-q", "origin", "main"]);
    // …and the research blob still was never fetched.
    assert!(!store2.join(&model_oid[..2]).join(&model_oid[2..]).exists());

    // The server picks up the push.
    wren_ok(&["sync"], &env, None);
    let log = wren_ok(&["log", "-n", "3"], &env, None);
    assert!(
        log.lines().next().unwrap().contains("agent: bump api"),
        "{log}"
    );

    // Queries without a checkout.
    let blame = wren_ok(&["blame", "services/api/main.rs"], &env, None);
    assert!(blame.lines().nth(1).unwrap().contains("api v2"), "{blame}");
    let path_log = wren_ok(&["log", "--", "services/api/lib.rs"], &env, None);
    assert_eq!(path_log.lines().count(), 2, "{path_log}");
    let feature = git(&up, &["rev-parse", "feature"]);
    let landed = wren_ok(&["landed", feature.trim(), "--into", "main"], &env, None);
    let merge = git(&up, &["rev-parse", "v1^{commit}"]);
    assert_eq!(landed.trim(), merge.trim());
    let (anc, _, _) = wren(&["is-ancestor", "v1", "main"], &env, None);
    assert!(anc);
    let (anc, _, _) = wren(&["is-ancestor", "main", "v1"], &env, None);
    assert!(!anc);
    assert_eq!(
        wren_ok(&["cat", "README.md", "--rev", "v1"], &env, None),
        "# demo\n"
    );
    let diff = wren_ok(&["diff", "main"], &env, None);
    assert!(diff.contains("+    println!(\"api v2\");"), "{diff}");
    let mb = wren_ok(&["merge-base", "feature", "main~2"], &env, None);
    assert_eq!(
        mb.trim(),
        git(&up, &["merge-base", "feature", "main~2"]).trim()
    );

    // Commands inside a checkout find server and repo from .wren/config.json.
    let refs = wren_ok(&["refs"], &[("WREN_TOKEN", "s3cret-cache")], Some(&ws));
    assert!(refs.contains("refs/tags/v1"), "{refs}");

    // A second node checking out the same commit is served from the cache.
    let before: serde_json::Value = serde_json::from_slice(
        &Command::new("curl")
            .args([
                "-s",
                "-H",
                "Authorization: Bearer s3cret-cache",
                &format!("{cache_url}/v1/stats"),
            ])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    wren_ok(
        &[
            "checkout",
            root.join("full2").to_str().unwrap(),
            "--store",
            root.join("node3-store").to_str().unwrap(),
            "--rev",
            // What the first full checkout fetched (main before the agent's push).
            "main~1",
        ],
        &env,
        None,
    );
    let after: serde_json::Value = serde_json::from_slice(
        &Command::new("curl")
            .args([
                "-s",
                "-H",
                "Authorization: Bearer s3cret-cache",
                &format!("{cache_url}/v1/stats"),
            ])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(after["hits"].as_u64().unwrap() > before["hits"].as_u64().unwrap());
    assert_eq!(
        after["misses"], before["misses"],
        "those objects were all cached already"
    );

    // Below the sparse cone boundary nothing was ever fetched, not even trees.
    let deep_tree = git(&up, &["rev-parse", "v1:research/big"]);
    let deep_tree = deep_tree.trim();
    assert!(!store2.join(&deep_tree[..2]).join(&deep_tree[2..]).exists());

    // --full-index: a classic index (every tree fetched, still no blobs).
    let ws_full = root.join("ws-full");
    wren_ok(
        &[
            "checkout",
            ws_full.to_str().unwrap(),
            "--git",
            "--full-index",
            "--sparse",
            "services/api",
            "--store",
            store2.to_str().unwrap(),
        ],
        &env,
        None,
    );
    assert_eq!(git(&ws_full, &["status", "--porcelain"]), "");
    assert!(git(&ws_full, &["ls-files", "-t"]).contains("S research/big/model.py"));

    // Capabilities and protocol version, identical shape on both tiers.
    for url in [&server_url, &cache_url] {
        let out = Command::new("curl")
            .args([
                "-s",
                "-i",
                "-H",
                if url == &server_url {
                    "Authorization: Bearer s3cret-server"
                } else {
                    "Authorization: Bearer s3cret-cache"
                },
                &format!("{url}/v1/capabilities"),
            ])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
        assert!(text.contains("wren-protocol: 1"), "{text}");
        assert!(text.contains("\"protocol\":1"), "{text}");
    }

    // Freshness is visible to clients.
    let repos = wren_ok(&["repos", "--json"], &env, None);
    assert!(repos.contains("\"last_sync\": 1"), "{repos}");

    // Queries pinned to full commit ids are immutable: the cache keeps them.
    let main_oid = git(&up, &["rev-parse", "main"]);
    let main_oid = main_oid.trim();
    let stats = |k: &str| -> u64 {
        let out = Command::new("curl")
            .args([
                "-s",
                "-H",
                "Authorization: Bearer s3cret-cache",
                &format!("{cache_url}/v1/stats"),
            ])
            .output()
            .unwrap();
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()[k]
            .as_u64()
            .unwrap()
    };
    let hits0 = stats("api_hits");
    let b1 = wren_ok(
        &["blame", "services/api/lib.rs", "--rev", main_oid],
        &env,
        None,
    );
    let b2 = wren_ok(
        &["blame", "services/api/lib.rs", "--rev", main_oid],
        &env,
        None,
    );
    assert_eq!(b1, b2);
    assert!(
        stats("api_hits") > hits0,
        "second pinned blame should be served by the cache"
    );
    let hits1 = stats("api_hits");
    wren_ok(
        &["blame", "services/api/lib.rs", "--rev", "main"],
        &env,
        None,
    );
    wren_ok(
        &["blame", "services/api/lib.rs", "--rev", "main"],
        &env,
        None,
    );
    assert_eq!(
        stats("api_hits"),
        hits1,
        "branch-relative queries are never cached"
    );

    // Bisect without a clone: each step is a fresh sparse checkout on the
    // shared node store, the search runs over main's first-parent history.
    let (ok, out, err) = wren(
        &[
            "bisect",
            "--good",
            "v1",
            "--bad",
            main_oid,
            "--store",
            store1.to_str().unwrap(),
            "--",
            "sh",
            "-c",
            "grep -q ^ok status.txt",
        ],
        &env,
        None,
    );
    assert!(ok, "{err}");
    let step5 = git(&up, &["log", "--format=%H", "--grep=^step 5$", "main"]);
    assert!(
        out.starts_with(step5.trim()),
        "bisect said {out}, expected step 5 {step5}\n{err}"
    );

    // MCP: an agent reads history and files with no checkout at all.
    let mut mcp = Command::new(WREN)
        .args(["mcp"])
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = mcp.stdin.take().unwrap();
    let mut stdout = BufReader::new(mcp.stdout.take().unwrap());
    let mut rpc = |msg: serde_json::Value| -> serde_json::Value {
        writeln!(stdin, "{msg}").unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let init = rpc(
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "wren");
    let tools = rpc(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    assert!(tools["result"]["tools"].as_array().unwrap().len() >= 8);
    let file = rpc(
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"wren_read_file","arguments":{"path":"services/api/lib.rs"}}}),
    );
    assert!(file["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("health"));
    let bad = rpc(
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"wren_read_file","arguments":{"path":"nope"}}}),
    );
    assert_eq!(bad["result"]["isError"], true);
    drop(stdin);
    let _ = mcp.wait();
    let _ = PathBuf::new();
}
