//! End-to-end writes: a bare git host -> wren server (write proxy) <- MCP
//! `wren_commit` and the `wren commit` / `land` / `revert` CLI. Every write
//! must reach the git host, with no checkout anywhere.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WREN: &str = env!("CARGO_BIN_EXE_wren");
const WRITE_TOKEN: &str = "s3cret-write";
const AUTHOR: &str = "Agent Smith <agent@example.com>";

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

/// Run wren; returns (exit code, stdout, stderr).
fn wren(args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut c = Command::new(WREN);
    c.args(args).env("WREN_LOG", "warn");
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn wren_ok(args: &[&str], env: &[(&str, &str)]) -> String {
    let (code, out, err) = wren(args, env);
    assert_eq!(code, 0, "wren {args:?} failed: {err}");
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

/// A bare host repository with a little history on main.
fn make_host(root: &Path) -> std::path::PathBuf {
    let seed = root.join("seed");
    std::fs::create_dir_all(seed.join("src")).unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("README.md"), "# demo\n").unwrap();
    std::fs::write(seed.join("src/lib.rs"), "pub fn one() -> u32 {\n    1\n}\n").unwrap();
    std::fs::write(seed.join("obsolete.txt"), "remove me\n").unwrap();
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-q", "-m", "initial import"]);
    std::fs::write(seed.join("notes.md"), "notes\n").unwrap();
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-q", "-m", "notes"]);
    let host = root.join("host.git");
    let out = Command::new("git")
        .args(["clone", "-q", "--bare"])
        .arg(&seed)
        .arg(&host)
        .output()
        .unwrap();
    assert!(out.status.success());
    host
}

fn subjects(host: &Path) -> Vec<String> {
    git(host, &["log", "--format=%s", "main"])
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn writes_through_mcp_and_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let host = make_host(&root);
    let initial_tip = git(&host, &["rev-parse", "main"]).trim().to_string();

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
                "--write-token",
                WRITE_TOKEN,
            ])
            .arg("--repo")
            .arg(format!("demo={}", host.display()))
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
    let env = [
        ("WREN_SERVER", server_url.as_str()),
        ("WREN_REPO", "demo"),
        ("WREN_WRITE_TOKEN", WRITE_TOKEN),
        ("WREN_AUTHOR", AUTHOR),
    ];

    // Without the write token, writes are refused and nothing moves.
    let local = root.join("local");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("x.txt"), "x\n").unwrap();
    let x = format!("x.txt={}", local.join("x.txt").display());
    let (code, _, err) = wren(
        &["commit", "-m", "nope", "--land", "main", "--set", &x],
        &[
            ("WREN_SERVER", server_url.as_str()),
            ("WREN_REPO", "demo"),
            ("WREN_AUTHOR", AUTHOR),
        ],
    );
    assert_ne!(code, 0);
    assert!(err.contains("WREN_WRITE_TOKEN"), "{err}");
    assert_eq!(git(&host, &["rev-parse", "main"]).trim(), initial_tip);

    // MCP: an agent commits and lands with no checkout.
    let mut mcp = Command::new(WREN)
        .args(["mcp"])
        .envs(env)
        .env("WREN_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = mcp.stdin.take().unwrap();
    let mut stdout = BufReader::new(mcp.stdout.take().unwrap());
    let mut id = 0;
    let mut rpc = |method: &str, params: serde_json::Value| -> serde_json::Value {
        id += 1;
        let msg =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(stdin, "{msg}").unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let mut tool = |name: &str, args: serde_json::Value| -> (String, bool) {
        let r = rpc(
            "tools/call",
            serde_json::json!({"name": name, "arguments": args}),
        );
        (
            r["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string(),
            r["result"]["isError"] == true,
        )
    };
    let (text, failed) = tool(
        "wren_commit",
        serde_json::json!({
            "message": "agent: add hello",
            "changes": [
                {"path": "hello.txt", "content": "hello\n"},
                {"path": "src/lib.rs", "content": "pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    2\n}\n"},
            ],
            "onto": "main",
        }),
    );
    assert!(!failed, "{text}");
    assert!(text.contains("Landed"), "{text}");
    assert_eq!(subjects(&host)[0], "agent: add hello");
    assert_eq!(git(&host, &["show", "main:hello.txt"]), "hello\n");
    assert_eq!(
        git(&host, &["log", "-1", "--format=%an <%ae>", "main"]).trim(),
        AUTHOR
    );
    let tip = git(&host, &["rev-parse", "main"]).trim().to_string();
    assert!(text.contains(&tip), "{text}");
    // Read-your-writes through the query tools.
    let (text, failed) = tool(
        "wren_read_file",
        serde_json::json!({"path": "hello.txt", "rev": "main"}),
    );
    assert!(!failed && text.contains("hello"), "{text}");

    // A stale edit of the same lines conflicts; nothing lands, and the
    // agent is told where to look. (Both sides add `two` with a different
    // body; `git merge-file` reports a conflict. Edits separated by an
    // unchanged line, like `one`'s body and a new function, merge cleanly
    // in git and therefore in Wren.)
    let (text, failed) = tool(
        "wren_commit",
        serde_json::json!({
            "message": "agent: stale edit",
            "parent": initial_tip,
            "changes": [{"path": "src/lib.rs", "content": "pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    22\n}\n"}],
            "onto": "main",
        }),
    );
    assert!(failed, "{text}");
    for want in ["NOT landed", "src/lib.rs", "wren_read_file", &tip] {
        assert!(text.contains(want), "missing {want:?} in {text}");
    }
    assert_eq!(git(&host, &["rev-parse", "main"]).trim(), tip);

    // A non-conflicting commit on the old parent is rebased and lands.
    let (text, failed) = tool(
        "wren_commit",
        serde_json::json!({
            "message": "agent: readme",
            "parent": initial_tip,
            "changes": [{"path": "README.md", "content": "# demo\n\nNow with writes.\n"}],
            "onto": "main",
        }),
    );
    assert!(!failed, "{text}");
    assert_eq!(subjects(&host)[..2], ["agent: readme", "agent: add hello"]);
    drop(stdin);
    let _ = mcp.wait();

    // CLI: commit and land in one call, with an executable, a symlink, a
    // file big enough to be uploaded on its own, and a delete.
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(local.join("run.sh"), "#!/bin/sh\necho run\n").unwrap();
    std::fs::set_permissions(local.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("src/lib.rs", local.join("link")).unwrap();
    let big = "0123456789abcdef\n".repeat(20_000);
    std::fs::write(local.join("big.txt"), &big).unwrap();
    let set = |repo: &str, file: &str| format!("{repo}={}", local.join(file).display());
    let out = wren_ok(
        &[
            "commit",
            "-m",
            "cli: tools",
            "--land",
            "main",
            "--set",
            &set("tools/run.sh", "run.sh"),
            "--set",
            &set("entry.rs", "link"),
            "--set",
            &set("data/big.txt", "big.txt"),
            "--delete",
            "obsolete.txt",
        ],
        &env,
    );
    assert!(out.contains("landed refs/heads/main"), "{out}");
    assert_eq!(subjects(&host)[0], "cli: tools");
    let ls = git(&host, &["ls-tree", "-r", "main"]);
    assert!(
        ls.contains("100755 blob") && ls.contains("\ttools/run.sh"),
        "{ls}"
    );
    assert!(
        ls.contains("120000 blob") && ls.contains("\tentry.rs"),
        "{ls}"
    );
    assert!(!ls.contains("obsolete.txt"), "{ls}");
    assert_eq!(git(&host, &["show", "main:data/big.txt"]), big);
    assert_eq!(git(&host, &["show", "main:entry.rs"]), "src/lib.rs");

    // CLI: commit without landing, then land it.
    std::fs::write(local.join("feature.txt"), "feature\n").unwrap();
    let out = wren_ok(
        &[
            "commit",
            "-m",
            "cli: feature",
            "--parent",
            "main",
            "--set",
            &set("feature.txt", "feature.txt"),
        ],
        &env,
    );
    let feature = out.trim().to_string();
    assert_eq!(feature.len(), 40, "{out}");
    assert_eq!(
        subjects(&host)[0],
        "cli: tools",
        "commit alone moves nothing"
    );
    let out = wren_ok(&["land", "--onto", "main", &feature], &env);
    assert!(out.contains("landed refs/heads/main"), "{out}");
    assert_eq!(git(&host, &["rev-parse", "main"]).trim(), feature);

    // CLI: revert it.
    let out = wren_ok(&["revert", &feature, "--onto", "main"], &env);
    assert!(out.contains("landed refs/heads/main"), "{out}");
    assert_eq!(subjects(&host)[0], "Revert \"cli: feature\"");
    assert!(!git(&host, &["ls-tree", "-r", "--name-only", "main"]).contains("feature.txt"));

    // CLI: a conflicting land exits 1 with a readable conflict list.
    // (Adds `two` with another body than main's: a real conflict for git.)
    std::fs::write(
        local.join("lib.rs"),
        "pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    3\n}\n",
    )
    .unwrap();
    let before = git(&host, &["rev-parse", "main"]);
    let (code, _, err) = wren(
        &[
            "commit",
            "-m",
            "cli: stale",
            "--parent",
            &initial_tip,
            "--land",
            "main",
            "--set",
            &set("src/lib.rs", "lib.rs"),
        ],
        &env,
    );
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("not landed") && err.contains("src/lib.rs"),
        "{err}"
    );
    assert_eq!(git(&host, &["rev-parse", "main"]), before);

    // History on the host is linear and every object is valid.
    let parents = git(&host, &["log", "--format=%P", "main"]);
    assert!(
        parents.lines().all(|p| p.split(' ').count() <= 1),
        "{parents}"
    );
    git(&host, &["fsck", "--strict"]);

    // Caches refuse writes, and the error says where writes go.
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
            ])
            .env("WREN_LOG", "warn")
            .spawn()
            .unwrap(),
    );
    wait_for(
        || wren(&["repos", "--server", &cache_url], &[]).0 == 0,
        "cache up",
    );
    let (code, _, err) = wren(
        &["commit", "-m", "via cache", "--land", "main", "--set", &x],
        &[
            ("WREN_SERVER", cache_url.as_str()),
            ("WREN_REPO", "demo"),
            ("WREN_WRITE_TOKEN", WRITE_TOKEN),
            ("WREN_AUTHOR", AUTHOR),
        ],
    );
    assert_ne!(code, 0);
    assert!(
        err.contains("403") && err.contains("WREN_WRITE_SERVER"),
        "{err}"
    );
}
