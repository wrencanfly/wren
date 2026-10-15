//! Renames against git as the oracle: random linear histories with pure
//! renames, renames with small edits, and renames with rewrites (which git
//! must not pair), checked against `git diff-tree -M` (pairs and
//! similarity), `git log --follow` and `git blame`.
//!
//! Each commit renames at most one file and touches no other file, so every
//! rename has a single candidate and git's answer is unambiguous.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use wren_core::renames::{Copies, CopyKind};
use wren_core::{ObjectStore, Oid, Repo};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@wren.dev"])
        .args(args)
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

struct Gen<'a> {
    dir: &'a Path,
    rng: Rng,
    n: usize,
}

impl Gen<'_> {
    fn line(&mut self) -> String {
        self.n += 1;
        format!("line {} {:x}\n", self.n, self.rng.next())
    }
    fn files(&self) -> Vec<String> {
        git(self.dir, &["ls-files"])
            .lines()
            .map(str::to_string)
            .collect()
    }
    fn commit(&mut self, msg: &str) {
        git(self.dir, &["add", "-A"]);
        git(self.dir, &["commit", "-q", "--allow-empty", "-m", msg]);
    }
    fn step(&mut self, i: usize) {
        let files = self.files();
        let pick = if files.is_empty() {
            0
        } else {
            self.rng.below(10)
        };
        match pick {
            // Rename, maybe with an edit.
            0..=3 if !files.is_empty() => {
                let from = files[self.rng.below(files.len())].clone();
                let dir = ["a", "b", "a/c"][self.rng.below(3)];
                let to = format!("{dir}/r{i}.txt");
                if Path::new(&to) == Path::new(&from) {
                    return;
                }
                std::fs::create_dir_all(self.dir.join(dir)).unwrap();
                let mut lines: Vec<String> = std::fs::read_to_string(self.dir.join(&from))
                    .unwrap()
                    .lines()
                    .map(|l| format!("{l}\n"))
                    .collect();
                match self.rng.below(3) {
                    0 => {} // pure
                    1 => {
                        // small edit: well above 50%
                        let at = self.rng.below(lines.len() + 1);
                        let l = self.line();
                        lines.insert(at, l);
                    }
                    _ => {
                        // rewrite: well below 50%
                        for l in lines.iter_mut().skip(1) {
                            *l = format!("rewritten {} {:x}\n", self.n, self.rng.next());
                            self.n += 1;
                        }
                        lines.push(self.line());
                    }
                }
                std::fs::remove_file(self.dir.join(&from)).unwrap();
                std::fs::write(self.dir.join(&to), lines.concat()).unwrap();
            }
            // Edit a file.
            0..=7 if !files.is_empty() => {
                let f = files[self.rng.below(files.len())].clone();
                let mut lines: Vec<String> = std::fs::read_to_string(self.dir.join(&f))
                    .unwrap()
                    .lines()
                    .map(|l| format!("{l}\n"))
                    .collect();
                let at = self.rng.below(lines.len());
                if self.rng.below(2) == 0 {
                    lines[at] = self.line();
                } else {
                    let l = self.line();
                    lines.insert(at, l);
                }
                std::fs::write(self.dir.join(&f), lines.concat()).unwrap();
            }
            // A new file.
            _ => {
                let dir = ["a", "b", "a/c"][self.rng.below(3)];
                std::fs::create_dir_all(self.dir.join(dir)).unwrap();
                let n = 8 + self.rng.below(30);
                let content: String = (0..n).map(|_| self.line()).collect();
                std::fs::write(self.dir.join(format!("{dir}/n{i}.txt")), content).unwrap();
            }
        }
        self.commit(&format!("step {i}"));
    }
}

fn run(seed: u64) {
    let tmp = tempfile::tempdir().unwrap();
    let up = tmp.path().join("up");
    std::fs::create_dir_all(&up).unwrap();
    git(&up, &["init", "-q", "-b", "main"]);
    let mut g = Gen {
        dir: &up,
        rng: Rng(seed.wrapping_mul(0x9e3779b97f4a7c15) | 1),
        n: 0,
    };
    for i in 0..60 {
        g.step(i);
    }
    let store = Arc::new(ObjectStore::open(tmp.path().join("objects")).unwrap());
    let repo = Repo::open(
        tmp.path().join("repo"),
        "t",
        store,
        Some(up.to_str().unwrap()),
    )
    .unwrap();
    repo.sync().unwrap();
    let head = repo.resolve("main").unwrap();
    let commits: Vec<Oid> = git(&up, &["rev-list", "main"])
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();

    // Renames per commit, with similarity.
    let mut renames_seen = 0;
    for c in &commits {
        let out = git(
            &up,
            &[
                "diff-tree",
                "-r",
                "-M",
                "--root",
                "--name-status",
                "--no-commit-id",
                &c.to_hex(),
            ],
        );
        let mut expect: Vec<(String, String, u32)> = out
            .lines()
            .filter(|l| l.starts_with('R'))
            .map(|l| {
                let parts: Vec<&str> = l.split('\t').collect();
                (
                    parts[1].to_string(),
                    parts[2].to_string(),
                    parts[0][1..].parse().unwrap(),
                )
            })
            .collect();
        expect.sort();
        let mut ours: Vec<(String, String, u32)> = repo
            .derived::<Copies>(c)
            .unwrap()
            .0
            .iter()
            .filter(|x| x.kind == CopyKind::Rename)
            .map(|x| (x.from.clone(), x.to.clone(), x.score))
            .collect();
        ours.sort();
        assert_eq!(ours, expect, "seed {seed}: renames in {c}");
        renames_seen += expect.len();
    }
    assert!(renames_seen > 0, "seed {seed}: no renames generated");

    // log --follow and blame for every file at main.
    for f in git(&up, &["ls-files"]).lines() {
        let expect: Vec<Oid> = git(&up, &["log", "--follow", "--format=%H", "main", "--", f])
            .lines()
            .map(|l| l.parse().unwrap())
            .collect();
        let ours: Vec<Oid> = repo
            .log_follow(&head, f, 1000)
            .unwrap()
            .into_iter()
            .map(|x| x.0)
            .collect();
        assert_eq!(ours, expect, "seed {seed}: log --follow {f}");

        let porcelain = git(
            &up,
            &["blame", "--first-parent", "--porcelain", "main", "--", f],
        );
        let expect: Vec<(Oid, u32)> = porcelain
            .lines()
            .filter_map(|l| {
                let p: Vec<&str> = l.split(' ').collect();
                (p.len() >= 3 && p[0].len() == 40 && p[0].bytes().all(|b| b.is_ascii_hexdigit()))
                    .then(|| (p[0].parse().unwrap(), p[1].parse().unwrap()))
            })
            .collect();
        let ours: Vec<(Oid, u32)> = repo
            .blame(&head, f)
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|l| (l.commit, l.orig_line))
            .collect();
        assert_eq!(ours, expect, "seed {seed}: blame {f}");
    }
}

#[test]
fn renames_agree_with_git() {
    let n: u64 = std::env::var("WREN_RENAME_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    for seed in 1..=n {
        run(seed);
    }
}
