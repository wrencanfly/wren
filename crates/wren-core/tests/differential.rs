//! Differential tests: git is the oracle. A random repository with branches
//! and merges is generated with the git CLI, ingested by Wren, and every query
//! Wren answers is compared against git's own answer.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

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
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Wren Test",
            "-c",
            "user.email=test@wren.dev",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap()
        .success()
}

/// Builds a random history. Each branch only touches its own directory, so
/// merges never conflict; deletions happen in commits that add nothing, so
/// git's blame rename-following never kicks in (Wren does not follow renames).
struct Gen {
    dir: PathBuf,
    rng: Rng,
    counter: usize,
    tick: u64,
}

impl Gen {
    fn files_in(&self, sub: &str) -> Vec<String> {
        let out = git(&self.dir, &["ls-files", "--", sub]);
        out.lines().map(|s| s.to_string()).collect()
    }

    fn commit(&mut self, msg: &str) {
        self.tick += 1;
        let date = format!("{} +0000", 1_700_000_000 + self.tick * 60);
        let st = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args([
                "-c",
                "user.name=Wren Test",
                "-c",
                "user.email=test@wren.dev",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(["commit", "-q", "--allow-empty", "-m", msg])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .status()
            .unwrap();
        assert!(st.success());
    }

    fn line(&mut self) -> String {
        self.counter += 1;
        format!("line-{} {:x}\n", self.counter, self.rng.next())
    }

    fn edit(&mut self, area: &str) {
        let files = self.files_in(area);
        if files.is_empty() || self.rng.chance(30) {
            let depth = self.rng.below(3);
            let mut p = PathBuf::from(area);
            for _ in 0..depth {
                p.push(format!("d{}", self.rng.below(3)));
            }
            p.push(format!("f{}.txt", self.counter));
            let n = if self.rng.chance(20) {
                50 + self.rng.below(150)
            } else {
                1 + self.rng.below(8)
            };
            let content: String = (0..n).map(|_| self.line()).collect();
            std::fs::create_dir_all(self.dir.join(p.parent().unwrap())).unwrap();
            std::fs::write(self.dir.join(&p), content).unwrap();
        } else {
            let f = files[self.rng.below(files.len())].clone();
            let path = self.dir.join(&f);
            let mut lines: Vec<String> = std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(|l| format!("{l}\n"))
                .collect();
            // Large edits (big inserted, deleted or rewritten blocks) are what
            // real refactors look like, and what exposed a blame diff bug on
            // git.git. Lines are unique, so every minimal diff is the same
            // and git's attribution is unambiguous. (Block *moves* are left
            // out on purpose: they admit several minimal diffs.)
            if self.rng.chance(15) && lines.len() > 4 {
                let a = self.rng.below(lines.len());
                let b = (a + 1 + self.rng.below(lines.len() - a)).min(lines.len());
                match self.rng.below(3) {
                    0 => {
                        let block: Vec<String> =
                            (0..20 + self.rng.below(60)).map(|_| self.line()).collect();
                        lines.splice(a..a, block);
                    }
                    1 => {
                        lines.drain(a..b);
                    }
                    _ => {
                        let block: Vec<String> = (0..(b - a) + self.rng.below(30))
                            .map(|_| self.line())
                            .collect();
                        lines.splice(a..b, block);
                    }
                }
            }
            for _ in 0..1 + self.rng.below(3) {
                match self.rng.below(3) {
                    0 => {
                        let at = self.rng.below(lines.len() + 1);
                        let l = self.line();
                        lines.insert(at, l);
                    }
                    1 if lines.len() > 1 => {
                        let at = self.rng.below(lines.len());
                        lines.remove(at);
                    }
                    _ => {
                        let at = self.rng.below(lines.len().max(1));
                        let l = self.line();
                        if lines.is_empty() {
                            lines.push(l)
                        } else {
                            lines[at] = l
                        }
                    }
                }
            }
            if self.rng.chance(10) {
                // Exercise a mode change too.
                git(&self.dir, &["update-index", "--chmod=+x", &f]);
            }
            std::fs::write(&path, lines.concat()).unwrap();
        }
        git(&self.dir, &["add", "-A"]);
    }

    fn delete(&mut self, area: &str) {
        let files = self.files_in(area);
        if files.len() > 3 {
            let f = files[self.rng.below(files.len())].clone();
            git(&self.dir, &["rm", "-q", &f]);
        }
    }
}

fn build_history(dir: &Path, seed: u64, rounds: usize, g: &mut Option<Gen>) {
    if g.is_none() {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        *g = Some(Gen {
            dir: dir.to_path_buf(),
            rng: Rng(seed),
            counter: 0,
            tick: 0,
        });
        let g = g.as_mut().unwrap();
        g.edit("core");
        g.commit("root");
    }
    let g = g.as_mut().unwrap();
    for round in 0..rounds {
        let r = g.rng.below(10);
        if r < 5 {
            g.edit("core");
            let msg = format!("main work {round}");
            g.commit(&msg);
        } else if r < 6 {
            g.delete("core");
            g.commit("delete a file");
        } else {
            // Feature branch off a recent main commit, merged back with --no-ff
            // (or left unmerged sometimes).
            let name = format!("feature-{}-{round}", g.rng.below(1000));
            let back = g.rng.below(3);
            let base = format!("main~{}", back.min(g_main_depth(&g.dir)));
            git(&g.dir, &["checkout", "-q", "-b", &name, &base]);
            let area = format!("feat{}/{name}", g.rng.below(3));
            for i in 0..1 + g.rng.below(4) {
                g.edit(&area);
                let msg = format!("{name} step {i}");
                g.commit(&msg);
            }
            git(&g.dir, &["checkout", "-q", "main"]);
            if g.rng.chance(80) {
                git(
                    &g.dir,
                    &[
                        "-c",
                        "user.name=Wren Test",
                        "-c",
                        "user.email=t@wren.dev",
                        "merge",
                        "-q",
                        "--no-ff",
                        "-m",
                        &format!("Merge {name}"),
                        &name,
                    ],
                );
            }
        }
        if g.rng.chance(5) {
            let tag = format!("v{}", g.tick);
            git(&g.dir, &["tag", "-a", "-m", &tag, &tag]);
        }
    }
}

fn g_main_depth(dir: &Path) -> usize {
    git(dir, &["rev-list", "--first-parent", "--count", "main"])
        .trim()
        .parse::<usize>()
        .unwrap()
        - 1
}

fn oid(s: &str) -> Oid {
    s.trim().parse().unwrap()
}

fn check_everything(up: &Path, repo: &Repo, rng: &mut Rng) {
    // Refs.
    let git_refs: BTreeMap<String, Oid> = git(
        up,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/tags",
        ],
    )
    .lines()
    .map(|l| {
        let (n, o) = l.split_once(' ').unwrap();
        (n.to_string(), oid(o))
    })
    .collect();
    let wren_refs: BTreeMap<String, Oid> = repo
        .meta
        .refs()
        .unwrap()
        .into_iter()
        .map(|r| (r.name, r.target))
        .collect();
    assert_eq!(git_refs, wren_refs, "refs differ");

    let all: Vec<Oid> = git(up, &["rev-list", "--all"]).lines().map(oid).collect();
    // Every reachable commit is a changeset. (Wren may hold more: commits
    // orphaned by a force-push stay in the immutable log.)
    for c in &all {
        assert!(repo.meta.has_changeset(c).unwrap(), "missing changeset {c}");
    }
    // The mirror keeps refs only; fetched packs were absorbed and deleted.
    let packs = std::fs::read_dir(repo.dir().join("mirror.git/objects/pack"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(packs, 0, "mirror holds packfiles");

    // Ancestry and merge bases on random pairs.
    for _ in 0..150 {
        let a = all[rng.below(all.len())];
        let b = all[rng.below(all.len())];
        let expect = git_ok(
            up,
            &["merge-base", "--is-ancestor", &a.to_hex(), &b.to_hex()],
        );
        assert_eq!(
            repo.is_ancestor(&a, &b).unwrap(),
            expect,
            "is_ancestor({a}, {b})"
        );
        let out = Command::new("git")
            .arg("-C")
            .arg(up)
            .args(["merge-base", "--all", &a.to_hex(), &b.to_hex()])
            .output()
            .unwrap();
        let mut expect: Vec<Oid> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(oid)
            .collect();
        expect.sort();
        assert_eq!(
            repo.merge_bases(&a, &b).unwrap(),
            expect,
            "merge_bases({a}, {b})"
        );
    }

    // Revision syntax.
    for rev in ["main", "main~1", "main~3", "main^", "main^1~2", "HEAD~2"] {
        let expect = git(up, &["rev-parse", rev]);
        assert_eq!(repo.resolve(rev).unwrap(), oid(&expect), "resolve {rev}");
    }
    let merges: Vec<Oid> = git(up, &["rev-list", "--merges", "main"])
        .lines()
        .map(oid)
        .collect();
    if let Some(m) = merges.first() {
        let rev = format!("{}^2", m.short());
        assert_eq!(
            repo.resolve(&rev).unwrap(),
            oid(&git(up, &["rev-parse", &rev]))
        );
    }

    // First-parent log, whole and path-limited.
    let main = repo.resolve("main").unwrap();
    let expect: Vec<Oid> = git(up, &["log", "--first-parent", "--format=%H", "main"])
        .lines()
        .map(oid)
        .collect();
    assert_eq!(repo.log(&main, None, usize::MAX).unwrap(), expect);
    let files: Vec<String> = git(up, &["ls-tree", "-r", "--name-only", "main"])
        .lines()
        .map(String::from)
        .collect();
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for f in files.iter().take(40) {
        paths.insert(f.clone());
        if let Some(i) = f.rfind('/') {
            paths.insert(f[..i].to_string());
        }
    }
    paths.insert("feat0".into());
    paths.insert("does/not/exist".into());
    for p in &paths {
        let expect: Vec<Oid> = git(
            up,
            &["log", "--first-parent", "--format=%H", "main", "--", p],
        )
        .lines()
        .map(oid)
        .collect();
        assert_eq!(
            repo.log(&main, Some(p), usize::MAX).unwrap(),
            expect,
            "log -- {p}"
        );
        let limited = repo.log(&main, Some(p), 2).unwrap();
        assert_eq!(limited, expect.iter().take(2).copied().collect::<Vec<_>>());
    }

    // Blame (first-parent) for every file at main and at an older commit.
    let older = repo
        .resolve(&format!("main~{}", g_main_depth(up) / 2))
        .unwrap();
    for rev in [main, older] {
        let files: Vec<String> = git(up, &["ls-tree", "-r", "--name-only", &rev.to_hex()])
            .lines()
            .map(String::from)
            .collect();
        for f in &files {
            let porcelain = git(
                up,
                &[
                    "blame",
                    "--first-parent",
                    "--porcelain",
                    &rev.to_hex(),
                    "--",
                    f,
                ],
            );
            let expect: Vec<(Oid, u32)> = porcelain
                .lines()
                .filter_map(|l| {
                    let parts: Vec<&str> = l.split(' ').collect();
                    if parts.len() >= 3
                        && parts[0].len() == 40
                        && parts[0].bytes().all(|b| b.is_ascii_hexdigit())
                    {
                        Some((oid(parts[0]), parts[1].parse().unwrap()))
                    } else {
                        None
                    }
                })
                .collect();
            let ours: Vec<(Oid, u32)> = repo
                .blame(&rev, f)
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|l| (l.commit, l.orig_line))
                .collect();
            assert_eq!(ours, expect, "blame {f} @ {rev}");
        }
    }

    // Grep (patterns that mean the same in POSIX ERE and Rust regex), over
    // the whole tree and under a directory, at main and an older commit.
    for rev in [main, older] {
        let files: Vec<String> = git(up, &["ls-tree", "-r", "--name-only", &rev.to_hex()])
            .lines()
            .map(String::from)
            .collect();
        let dir = files
            .get(rng.below(files.len().max(1)))
            .and_then(|f| f.split('/').next())
            .unwrap_or("")
            .to_string();
        for (pattern, ignore_case) in [
            ("line-1[0-9]* ", false),
            ("^line-[0-9]*7 ", false),
            ("[0-9a-f]{3}E$", true),
            ("no such text", false),
        ] {
            for path in ["", dir.as_str()] {
                let mut args = vec!["grep", "-n", "-E"];
                if ignore_case {
                    args.push("-i");
                }
                let hex = rev.to_hex();
                args.extend([pattern, hex.as_str()]);
                if !path.is_empty() {
                    args.extend(["--", path]);
                }
                let out = Command::new("git")
                    .current_dir(up)
                    .args(&args)
                    .output()
                    .unwrap();
                let expect: Vec<(String, usize)> = String::from_utf8(out.stdout)
                    .unwrap()
                    .lines()
                    .map(|l| {
                        let l = l.strip_prefix(&format!("{hex}:")).unwrap();
                        let mut it = l.splitn(3, ':');
                        let p = it.next().unwrap().to_string();
                        (p, it.next().unwrap().parse().unwrap())
                    })
                    .collect();
                let ours = repo
                    .grep(&rev, pattern, path, ignore_case, 100_000)
                    .unwrap();
                let got: Vec<(String, usize)> =
                    ours.matches.into_iter().map(|m| (m.path, m.line)).collect();
                assert_eq!(got, expect, "grep {pattern:?} -- {path:?} @ {rev}");
                // The limit keeps a prefix of the same answer.
                let cut = repo.grep(&rev, pattern, path, ignore_case, 3).unwrap();
                assert_eq!(cut.truncated, expect.len() > 3);
                assert_eq!(cut.matches.len(), expect.len().min(3));
            }
        }
    }

    // Diffs between random pairs.
    for _ in 0..40 {
        let a = all[rng.below(all.len())];
        let b = all[rng.below(all.len())];
        let mut expect: Vec<(char, String)> = git(
            up,
            &[
                "diff",
                "--name-status",
                "--no-renames",
                &a.to_hex(),
                &b.to_hex(),
            ],
        )
        .lines()
        .map(|l| {
            let (s, p) = l.split_once('\t').unwrap();
            (s.chars().next().unwrap(), p.to_string())
        })
        .collect();
        expect.sort_by(|x, y| x.1.cmp(&y.1));
        let ours: Vec<(char, String)> = repo
            .diff(&a, &b)
            .unwrap()
            .into_iter()
            .map(|c| (c.status.letter(), c.path))
            .collect();
        assert_eq!(ours, expect, "diff {a} {b}");
    }

    // Changesets match `git diff-tree` against the first parent.
    for c in all.iter().take(60) {
        let mut expect: Vec<(char, String)> = git(
            up,
            &[
                "diff-tree",
                "-r",
                "--no-renames",
                "--name-status",
                "--root",
                "-m",
                "--first-parent",
                "--no-commit-id",
                &c.to_hex(),
            ],
        )
        .lines()
        .map(|l| {
            let (s, p) = l.split_once('\t').unwrap();
            (s.chars().next().unwrap(), p.to_string())
        })
        .collect();
        expect.sort_by(|x, y| x.1.cmp(&y.1));
        let ours: Vec<(char, String)> = repo
            .changeset_changes(c)
            .unwrap()
            .into_iter()
            .map(|c| (c.status.letter(), c.path))
            .collect();
        assert_eq!(ours, expect, "changeset {c}");
    }

    // Revision syntax edge cases: peeling suffixes, and strictness.
    for rev in ["main^{commit}", "main~2^{}", "HEAD~0", "main^0"] {
        let expect = git(up, &["rev-parse", rev]);
        assert_eq!(repo.resolve(rev).unwrap(), oid(&expect), "resolve {rev}");
    }
    for bad in [
        "main~1x",
        "main^{tree}",
        "main~99999",
        "0000000000000000000000000000000000000000",
    ] {
        assert!(repo.resolve(bad).is_err(), "{bad} should not resolve");
    }

    // First-parent depth agrees with git.
    for _ in 0..20 {
        let c = all[rng.below(all.len())];
        let count: u32 = git(up, &["rev-list", "--first-parent", "--count", &c.to_hex()])
            .trim()
            .parse()
            .unwrap();
        assert_eq!(repo.node(&c).unwrap().depth, count - 1, "depth of {c}");
    }

    // Bisection converges to what git says. "Bad" = contains commit X, which
    // is monotone, so ancestors of good commits are good as bisect assumes.
    // `good` is any commit without X, often off main's first-parent line.
    let main_tip = repo.resolve("main").unwrap();
    for _ in 0..6 {
        let x = all[rng.below(all.len())];
        if !repo.is_ancestor(&x, &main_tip).unwrap() {
            continue;
        }
        let Some(good) = (0..50).map(|_| all[rng.below(all.len())]).find(|c| {
            !git_ok(
                up,
                &["merge-base", "--is-ancestor", &x.to_hex(), &c.to_hex()],
            )
        }) else {
            continue;
        };
        // git: the oldest first-parent commit of main containing X, among
        // those newer than the newest first-parent commit that good contains.
        let fp: Vec<Oid> = git(up, &["rev-list", "--first-parent", "main"])
            .lines()
            .map(oid)
            .collect();
        let containing: BTreeSet<Oid> =
            git(up, &["rev-list", "--ancestry-path", &format!("{x}..main")])
                .lines()
                .map(oid)
                .chain([x])
                .collect();
        let expect = fp.iter().rev().find(|c| containing.contains(c)).copied();
        let Some(base_ok) = fp.iter().find(|c| {
            git_ok(
                up,
                &["merge-base", "--is-ancestor", &c.to_hex(), &good.to_hex()],
            )
        }) else {
            continue; // good shares no first-parent history with main
        };
        let _ = base_ok;
        let (mut g, mut b) = (good, main_tip);
        let mut steps = 0;
        let found = loop {
            let step = repo.bisect_step(&g, &b).unwrap();
            match step.next {
                None => break step.first_bad.unwrap(),
                Some(n) => {
                    if repo.is_ancestor(&x, &n).unwrap() {
                        b = n
                    } else {
                        g = n
                    }
                }
            }
            steps += 1;
            assert!(steps < 64, "bisect did not converge");
        };
        assert_eq!(Some(found), expect, "bisect for X={x} good={good}");
    }

    // landed(): brute force against git on a few commits.
    let fp: Vec<Oid> = git(up, &["rev-list", "--first-parent", "--reverse", "main"])
        .lines()
        .map(oid)
        .collect();
    for _ in 0..15 {
        let c = all[rng.below(all.len())];
        let expect = fp
            .iter()
            .find(|m| {
                git_ok(
                    up,
                    &["merge-base", "--is-ancestor", &c.to_hex(), &m.to_hex()],
                )
            })
            .copied();
        assert_eq!(repo.landed(&c, &main).unwrap(), expect, "landed {c}");
    }
}

/// Seeds from WREN_FUZZ_SEEDS (comma-separated, decimal or 0x-hex), so CI can
/// run one quickly and a nightly job can run many.
fn seeds() -> Vec<u64> {
    let spec = std::env::var("WREN_FUZZ_SEEDS").unwrap_or_else(|_| "0x5eedf1".into());
    spec.split(',')
        .map(|s| {
            let s = s.trim();
            match s.strip_prefix("0x") {
                Some(h) => u64::from_str_radix(h, 16).unwrap(),
                None => s.parse().unwrap(),
            }
        })
        .collect()
}

#[test]
fn wren_agrees_with_git() {
    for seed in seeds() {
        eprintln!("seed {seed:#x}");
        scenario(seed);
    }
}

fn scenario(seed: u64) {
    let tmp = tempfile::tempdir().unwrap();
    let up = tmp.path().join("upstream");
    let mut gen = None;
    build_history(&up, seed, 120, &mut gen);

    let store = Arc::new(ObjectStore::open(tmp.path().join("objects")).unwrap());
    let repo = Repo::open(
        tmp.path().join("repo"),
        "test",
        store.clone(),
        Some(up.to_str().unwrap()),
    )
    .unwrap();
    let report = repo.sync().unwrap();
    assert!(report.new_changesets > 100, "{report:?}");
    let mut rng = Rng(seed ^ 42);
    check_everything(&up, &repo, &mut rng);

    // Incremental ingest after more upstream work, with blame/derived data
    // partially persisted from the first round.
    build_history(&up, 0, 60, &mut gen);
    let report = repo.sync().unwrap();
    assert!(report.new_changesets > 0);
    check_everything(&up, &repo, &mut rng);

    // History rewrite: main is force-moved back and diverges; a branch and a
    // tag are deleted. Refs must follow upstream exactly.
    git(&up, &["reset", "-q", "--hard", "main~3"]);
    let branches = git(&up, &["branch", "--format=%(refname:short)"]);
    if let Some(b) = branches.lines().find(|b| *b != "main") {
        git(&up, &["branch", "-q", "-D", b]);
    }
    if let Some(t) = git(&up, &["tag"]).lines().next() {
        git(&up, &["tag", "-d", t]);
    }
    build_history(&up, 0, 20, &mut gen);
    repo.sync().unwrap();
    check_everything(&up, &repo, &mut rng);

    // A fresh process (empty memo) over the same data agrees too.
    drop(repo);
    let repo = Repo::open(tmp.path().join("repo"), "test", store, None).unwrap();
    check_everything(&up, &repo, &mut rng);
}
