//! A throwaway Postgres for tests: `initdb` in a temporary directory, a
//! server listening only on a Unix socket, stopped when dropped.
//!
//! Binaries are found through `WREN_PG_BIN`, then `PATH`, then the Debian /
//! Ubuntu layout (`/usr/lib/postgresql/<version>/bin`). Without them,
//! [`TestPg::start`] returns `None` and tests skip, unless `WREN_REQUIRE_PG`
//! is set (CI sets it), in which case it panics.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct TestPg {
    dir: tempfile::TempDir,
    bin: PathBuf,
    url: String,
}

fn find_bin() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("WREN_PG_BIN") {
        return Some(PathBuf::from(d));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&path) {
            if d.join("initdb").exists() && d.join("pg_ctl").exists() {
                return Some(d);
            }
        }
    }
    let mut versions: Vec<PathBuf> = std::fs::read_dir("/usr/lib/postgresql")
        .ok()?
        .flatten()
        .map(|e| e.path().join("bin"))
        .filter(|b| b.join("initdb").exists())
        .collect();
    versions.sort();
    versions.pop()
}

fn run(bin: &Path, tool: &str, args: &[&str]) {
    let out = Command::new(bin.join(tool)).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{tool} {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

impl TestPg {
    pub fn start() -> Option<TestPg> {
        let Some(bin) = find_bin() else {
            let required = std::env::var("WREN_REQUIRE_PG").is_ok_and(|v| !v.is_empty());
            assert!(
                !required,
                "WREN_REQUIRE_PG is set but no Postgres binaries were found"
            );
            eprintln!("skipping: no Postgres binaries (set WREN_PG_BIN)");
            return None;
        };
        // Unix socket paths are short (about 100 bytes): keep it under /tmp.
        let dir = tempfile::Builder::new()
            .prefix("wren-pg")
            .tempdir_in("/tmp")
            .unwrap();
        let data = dir.path().join("data");
        let data_s = data.to_str().unwrap();
        run(
            &bin,
            "initdb",
            &[
                "-D",
                data_s,
                "-U",
                "wren",
                "-A",
                "trust",
                "-E",
                "UTF8",
                "--locale=C",
                "--no-sync",
            ],
        );
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let sock = dir.path().to_str().unwrap().to_string();
        let opts = format!(
            "-k {sock} -p {port} -c listen_addresses='' -c fsync=off -c max_connections=200"
        );
        let log = dir.path().join("log");
        run(
            &bin,
            "pg_ctl",
            &[
                "-D",
                data_s,
                "-l",
                log.to_str().unwrap(),
                "-w",
                "-o",
                &opts,
                "start",
            ],
        );
        let url = format!("host={sock} port={port} user=wren dbname=postgres");
        Some(TestPg { dir, bin, url })
    }

    /// A connection string for `wren server --meta` / `MetaConfig::Postgres`.
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for TestPg {
    fn drop(&mut self) {
        let data = self.dir.path().join("data");
        let _ = Command::new(self.bin.join("pg_ctl"))
            .args(["-D", data.to_str().unwrap(), "-m", "immediate", "stop"])
            .output();
    }
}
