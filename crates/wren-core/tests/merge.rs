//! Server-side merges against `git merge-tree --write-tree` (git's own
//! merge, renames on) as the oracle.
//!
//! Two long-lived branches take random commits (line edits, adds, deletes,
//! renames within and across directories, whole-directory renames, mode
//! changes, files turning into directories) and are merged into each other
//! with git along the way, so histories become criss-cross. Every round,
//! merging one branch into the other must give:
//!
//! * git's tree, when git merges cleanly, unless Wren refused (a rename git
//!   would carry across, or conflicting virtual merge bases); refusals are
//!   counted and bounded;
//! * conflicts, whenever git has conflicts (Wren never merges what git
//!   would not).
//!
//! Rebasing a branch's last commit onto the other (a cherry-pick) is held
//! to the same rules, against `git merge-tree --merge-base`.
//!
//! Some rounds also merge through `merge_into`, which moves the branch on
//! the git host; the merge commit must have the id `git commit-tree` gives
//! the same tree, parents and message.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use wren_core::merge::{MergeRequest, Merged, TreeMerge};
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
        (self.next() % n.max(1) as u64) as usize
    }
}

const SIG: &str = "A U Thor <author@wren.dev> 1700000000 +0000";

fn git_out(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_AUTHOR_NAME", "A U Thor")
        .env("GIT_AUTHOR_EMAIL", "author@wren.dev")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_NAME", "A U Thor")
        .env("GIT_COMMITTER_EMAIL", "author@wren.dev")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let (ok, out) = git_out(dir, args);
    assert!(ok, "git {args:?} failed: {out}");
    out
}

fn oid(s: &str) -> Oid {
    s.trim().parse().unwrap()
}

struct Gen<'a> {
    work: &'a Path,
    rng: Rng,
    n: usize,
}

impl Gen<'_> {
    fn content(&mut self, lines: usize) -> String {
        (0..lines)
            .map(|_| {
                self.n += 1;
                format!("line {} {:x}\n", self.n, self.rng.next() % 0xffff)
            })
            .collect()
    }

    fn files(&self) -> Vec<String> {
        git(self.work, &["ls-files"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn dirs(&self) -> Vec<String> {
        let mut d: BTreeSet<String> = BTreeSet::new();
        for f in self.files() {
            let mut p = f.as_str();
            while let Some(i) = p.rfind('/') {
                p = &p[..i];
                d.insert(p.to_string());
            }
        }
        d.into_iter().collect()
    }

    /// Write a file, unless a file stands where a directory should be.
    fn write(&self, path: &str, content: &str) {
        let p = self.work.join(path);
        if p.is_dir() || std::fs::create_dir_all(p.parent().unwrap()).is_err() {
            return;
        }
        std::fs::write(p, content).unwrap();
    }

    fn edit_lines(&mut self, path: &str) {
        let p = self.work.join(path);
        let Ok(text) = std::fs::read_to_string(&p) else {
            return;
        };
        let mut ls: Vec<String> = text.lines().map(|l| format!("{l}\n")).collect();
        for _ in 0..1 + self.rng.below(2) {
            let i = self.rng.below(ls.len());
            self.n += 1;
            let new = format!("edit {}\n", self.n);
            match self.rng.below(3) {
                0 if ls.len() > 1 => {
                    ls.remove(i);
                }
                1 => ls.insert(i, new),
                _ if !ls.is_empty() => ls[i] = new,
                _ => ls.push(new),
            }
        }
        std::fs::write(p, ls.concat()).unwrap();
    }

    fn step(&mut self) {
        let files = self.files();
        let pick = |g: &mut Self| files[g.rng.below(files.len())].clone();
        let dirs = ["d0", "d1", "d1/sub", "d2", ""];
        let r = self.rng.below(100);
        match r {
            _ if files.is_empty() => {
                let c = self.content(10);
                self.write("seed", &c);
            }
            0..=44 => {
                let f = pick(self);
                self.edit_lines(&f);
            }
            45..=52 => {
                let f = pick(self);
                git(self.work, &["rm", "-q", &f]);
            }
            53..=62 => {
                let d = dirs[self.rng.below(dirs.len())];
                self.n += 1;
                let name = format!("{d}/n{}", self.n);
                let len = 8 + self.rng.below(8);
                let c = self.content(len);
                self.write(name.trim_start_matches('/'), &c);
            }
            63..=74 => {
                // Rename, maybe across directories, maybe with an edit.
                let f = pick(self);
                let d = dirs[self.rng.below(dirs.len())];
                self.n += 1;
                let to = format!("{d}/r{}", self.n);
                let to = to.trim_start_matches('/').to_string();
                if std::fs::create_dir_all(self.work.join(&to).parent().unwrap()).is_ok()
                    && git_out(self.work, &["mv", &f, &to]).0
                    && self.rng.below(2) == 0
                {
                    self.edit_lines(&to);
                }
            }
            75..=79 => {
                // A whole directory moves.
                let ds = self.dirs();
                if !ds.is_empty() {
                    let d = ds[self.rng.below(ds.len())].clone();
                    self.n += 1;
                    let to = format!("moved{}", self.n);
                    git(self.work, &["mv", &d, &to]);
                }
            }
            80..=86 => {
                let f = pick(self);
                let p = self.work.join(&f);
                if !p.is_symlink() {
                    use std::os::unix::fs::PermissionsExt;
                    let m = std::fs::metadata(&p).unwrap().permissions().mode();
                    let m = if m & 0o111 != 0 { 0o644 } else { 0o755 };
                    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(m)).unwrap();
                }
            }
            87..=94 => {
                // The same new path on both branches, likely: add/add.
                let k = self.rng.below(3);
                let c = self.content(3);
                self.write(&format!("d0/common{k}"), &c);
            }
            _ => {
                // A file becomes a directory, or a directory a file.
                let f = pick(self);
                git(self.work, &["rm", "-q", &f]);
                if self.rng.below(2) == 0 {
                    let c = self.content(5);
                    let _ = std::fs::remove_file(self.work.join(&f));
                    self.write(&format!("{f}/inside"), &c);
                } else if let Some(i) = f.rfind('/') {
                    let dir = f[..i].to_string();
                    let _ = git_out(self.work, &["rm", "-rq", &dir]);
                    let c = self.content(5);
                    self.write(&dir, &c);
                }
            }
        }
        git(self.work, &["add", "-A"]);
        self.n += 1;
        git(
            self.work,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("c{}", self.n),
            ],
        );
    }

    /// Merge `other` into the current branch with git, taking ours on
    /// conflicts, so that histories cross.
    fn git_merge(&self, other: &str) {
        let (ok, _) = git_out(self.work, &["merge", "-q", "--no-edit", "--no-ff", other]);
        if !ok {
            // (A merge git gave up on can leave its lock behind.)
            let _ = std::fs::remove_file(self.work.join(".git/index.lock"));
            git(self.work, &["reset", "-q", "--hard", "HEAD"]);
            git(self.work, &["clean", "-qfd"]);
            git(
                self.work,
                &["merge", "-q", "--no-edit", "--no-ff", "-s", "ours", other],
            );
        }
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    work: PathBuf,
    repo: Arc<Repo>,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let host = tmp.path().join("host.git");
    let work = tmp.path().join("work");
    git(
        tmp.path(),
        &["init", "-q", "--bare", "-b", "a", host.to_str().unwrap()],
    );
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            host.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    for (k, v) in [("gc.auto", "0"), ("maintenance.auto", "false")] {
        git(&work, &["config", k, v]);
    }
    let store = Arc::new(ObjectStore::open(tmp.path().join("objects")).unwrap());
    let repo = Repo::open(
        tmp.path().join("repo"),
        "t",
        store,
        Some(host.to_str().unwrap()),
    )
    .unwrap();
    Fixture {
        _tmp: tmp,
        work,
        repo: Arc::new(repo),
    }
}

#[derive(Default, Debug)]
struct Tally {
    clean: usize,
    conflicted: usize,
    refused: usize,
    criss_cross: usize,
    landed: usize,
    rebased: usize,
    rebase_refused: usize,
}

fn run(seed: u64, t: &mut Tally) {
    let f = fixture();
    let (repo, work) = (&f.repo, f.work.as_path());
    let mut g = Gen {
        work,
        rng: Rng(seed.wrapping_mul(0x9e3779b97f4a7c15) | 1),
        n: 0,
    };
    for i in 0..6 {
        let c = g.content(10);
        g.write(["d0/a", "d0/b", "d1/c", "d1/sub/d", "d2/e", "top"][i], &c);
    }
    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "-m", "root"]);
    git(work, &["branch", "b"]);
    for round in 0..30 {
        for br in ["a", "b"] {
            git(work, &["checkout", "-q", br]);
            for _ in 0..1 + g.rng.below(2) {
                g.step();
            }
        }
        // Cross the histories now and then: each branch merges the other's
        // tip as it was before either merge, which makes criss-crosses.
        let (a0, b0) = (
            git(work, &["rev-parse", "a"]),
            git(work, &["rev-parse", "b"]),
        );
        if g.rng.below(3) == 0 {
            git(work, &["checkout", "-q", "a"]);
            g.git_merge(b0.trim());
        }
        if g.rng.below(3) == 0 {
            git(work, &["checkout", "-q", "b"]);
            g.git_merge(a0.trim());
        }
        git(work, &["push", "-q", "-f", "origin", "a", "b"]);
        repo.sync().unwrap();
        let a = oid(&git(work, &["rev-parse", "a"]));
        let b = oid(&git(work, &["rev-parse", "b"]));
        let bases = git_out(work, &["merge-base", "--all", "a", "b"])
            .1
            .lines()
            .count();
        if bases > 1 {
            t.criss_cross += 1;
        }

        let (git_clean, out) = git_out(
            work,
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                "a",
                "b",
            ],
        );
        let git_tree = oid(out.lines().next().unwrap());
        let ctx = format!("seed {seed} round {round} ({bases} merge bases)");
        match repo.merge_commits(&a, &b).unwrap() {
            TreeMerge::Clean(edits) => {
                assert!(git_clean, "{ctx}: git conflicts ({out}), Wren merged");
                // The tree the merge commit gets.
                let probe = repo
                    .create_changeset(&wren_core::write::NewChangeset {
                        parents: vec![a, b],
                        author: SIG.as_bytes().to_vec(),
                        committer: None,
                        message: b"probe\n".to_vec(),
                        edits,
                    })
                    .unwrap();
                let tree = repo.tree_of(&probe).unwrap();
                assert_eq!(tree, git_tree, "{ctx}: merged trees differ");
                t.clean += 1;
            }
            TreeMerge::Conflicts(c) => {
                let kinds: BTreeSet<&str> = c.iter().map(|c| c.kind.as_str()).collect();
                if git_clean && !kinds.iter().all(|k| ["rename", "criss-cross"].contains(k)) {
                    let mb = git(work, &["merge-base", "a", "b"]);
                    eprintln!(
                        "ours since the base:\n{}\ntheirs since the base:\n{}",
                        git(
                            work,
                            &["diff", "--no-renames", "--name-status", mb.trim(), "a"]
                        ),
                        git(
                            work,
                            &["diff", "--no-renames", "--name-status", mb.trim(), "b"]
                        ),
                    );
                }
                if git_clean {
                    assert!(
                        kinds.iter().all(|k| ["rename", "criss-cross"].contains(k)),
                        "{ctx}: Wren conflicts {c:?}, git merged"
                    );
                    t.refused += 1;
                } else {
                    t.conflicted += 1;
                }
            }
        }

        // Rebasing b's last commit onto a: a cherry-pick, which git also
        // merges with renames on.
        let parents = git(work, &["log", "-1", "--format=%P", "b"]);
        if parents.split_whitespace().count() == 1 {
            let (ok, out) = git_out(
                work,
                &[
                    "merge-tree",
                    "--write-tree",
                    "--name-only",
                    "--no-messages",
                    &format!("--merge-base={}", parents.trim()),
                    "a",
                    "b",
                ],
            );
            let git_tree = oid(out.lines().next().unwrap());
            match repo.rebase(&[b], a, None).unwrap() {
                wren_core::write::Rebased::Done { changesets, .. } => {
                    if !ok {
                        let p = parents.trim();
                        eprintln!(
                            "commit:\n{}\nonto since its parent:\n{}\ngit:\n{}",
                            git(work, &["diff", "-M", "--name-status", p, "b"]),
                            git(work, &["diff", "-M", "--name-status", p, "a"]),
                            git_out(
                                work,
                                &[
                                    "merge-tree",
                                    "--write-tree",
                                    &format!("--merge-base={p}"),
                                    "a",
                                    "b"
                                ]
                            )
                            .1,
                        );
                    }
                    assert!(ok, "{ctx}: rebase: git conflicts ({out}), Wren rebased");
                    let tip = changesets.last().copied().unwrap_or(a);
                    assert_eq!(
                        repo.tree_of(&tip).unwrap(),
                        git_tree,
                        "{ctx}: rebased trees differ"
                    );
                    t.rebased += 1;
                }
                wren_core::write::Rebased::Conflicts(c) => {
                    if ok
                        && !c.iter().all(|c| {
                            ["rename", "delete/modify", "modify/delete"].contains(&c.kind.as_str())
                        })
                    {
                        let p = parents.trim();
                        eprintln!(
                            "commit:\n{}\nonto since its parent:\n{}\n",
                            git(work, &["diff", "-M", "--name-status", p, "b"]),
                            git(work, &["diff", "-M", "--name-status", p, "a"]),
                        );
                    }
                    if ok {
                        // Where git follows a rename the commit's change
                        // does not see (rebase detects renames only when
                        // a directory vanished), it is a delete/modify.
                        assert!(
                            c.iter().all(|c| {
                                ["rename", "delete/modify", "modify/delete"]
                                    .contains(&c.kind.as_str())
                            }),
                            "{ctx}: rebase: Wren conflicts {c:?}, git merged"
                        );
                        t.rebase_refused += 1;
                    }
                }
            }
        }

        // Now and then merge for real, through the branch on the host.
        if git_clean && g.rng.below(4) == 0 {
            let r = repo
                .merge_into(&MergeRequest {
                    into: "a".into(),
                    commit: b,
                    author: SIG.as_bytes().to_vec(),
                    committer: None,
                    message: Some(format!("merge round {round}\n").into_bytes()),
                    fast_forward: false,
                })
                .unwrap();
            match r {
                Merged::Merged { new, .. } => {
                    assert_eq!(repo.tree_of(&new).unwrap(), git_tree, "{ctx}");
                    let expect = git(
                        work,
                        &[
                            "commit-tree",
                            &git_tree.to_hex(),
                            "-p",
                            "a",
                            "-p",
                            "b",
                            "-m",
                            &format!("merge round {round}"),
                        ],
                    );
                    assert_eq!(new, oid(&expect), "{ctx}: merge commit id");
                    git(work, &["checkout", "-q", "a"]);
                    git(work, &["fetch", "-q", "origin"]);
                    git(work, &["reset", "-q", "--hard", "origin/a"]);
                    assert_eq!(oid(&git(work, &["rev-parse", "a"])), new);
                    t.landed += 1;
                }
                // Wren refused what git merges: counted above.
                Merged::Conflicts { .. } => {}
                Merged::UpToDate { .. } => assert!(repo.is_ancestor(&b, &a).unwrap(), "{ctx}"),
                other => panic!("{ctx}: {other:?}"),
            }
        }
    }
}

#[test]
fn merges_agree_with_git_merge_tree() {
    let n: u64 = std::env::var("WREN_MERGE_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let mut t = Tally::default();
    // WREN_MERGE_SEED=k reruns one seed.
    let one: Option<u64> = std::env::var("WREN_MERGE_SEED")
        .ok()
        .and_then(|v| v.parse().ok());
    // WREN_MERGE_SEED_BASE shifts the seeds (the nightly run takes fresh ones).
    let base: u64 = std::env::var("WREN_MERGE_SEED_BASE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    for seed in one.map_or(base + 1..=base + n, |k| k..=k) {
        run(seed, &mut t);
    }
    if one.is_some() {
        return;
    }
    eprintln!("merge vs merge-tree: {t:?}");
    let total = t.clean + t.conflicted + t.refused;
    assert!(t.clean > total / 3, "{t:?}");
    assert!(
        t.conflicted > 0 && t.landed > 0 && t.criss_cross > 0,
        "{t:?}"
    );
    assert!(t.refused * 4 < total, "too many refusals: {t:?}");
    assert!(
        t.rebase_refused * 4 < t.rebased,
        "too many rebase refusals: {t:?}"
    );
}

#[test]
fn merge_into_a_branch() {
    let f = fixture();
    let (repo, work) = (&f.repo, f.work.as_path());
    std::fs::write(work.join("a"), "a\n").unwrap();
    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "-m", "root"]);
    git(work, &["checkout", "-q", "-b", "side"]);
    std::fs::write(work.join("b"), "b\n").unwrap();
    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "-m", "side"]);
    git(work, &["push", "-q", "origin", "a", "side"]);
    repo.sync().unwrap();
    let side = oid(&git(work, &["rev-parse", "side"]));
    let root = oid(&git(work, &["rev-parse", "a"]));
    let req = |ff: bool| MergeRequest {
        into: "a".into(),
        commit: side,
        author: SIG.as_bytes().to_vec(),
        committer: None,
        message: None,
        fast_forward: ff,
    };
    // A fast-forward, when allowed.
    match repo.merge_into(&req(true)).unwrap() {
        Merged::Merged {
            new, fast_forward, ..
        } => assert!(fast_forward && new == side),
        other => panic!("{other:?}"),
    }
    match repo.merge_into(&req(false)).unwrap() {
        Merged::UpToDate { tip, .. } => assert_eq!(tip, side),
        other => panic!("{other:?}"),
    }
    // Otherwise a merge commit, with git's default-looking message.
    assert_eq!(
        repo.update_ref("refs/heads/a", Some(side), Some(root))
            .unwrap(),
        wren_core::write::RefUpdate::Updated
    );
    match repo.merge_into(&req(false)).unwrap() {
        Merged::Merged {
            new, fast_forward, ..
        } => {
            assert!(!fast_forward);
            let c = repo.commit(&new).unwrap();
            assert_eq!(c.parents, vec![root, side]);
            assert_eq!(repo.tree_of(&new).unwrap(), repo.tree_of(&side).unwrap());
            assert!(c.summary().starts_with("Merge commit '"), "{}", c.summary());
        }
        other => panic!("{other:?}"),
    }
}
