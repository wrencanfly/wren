//! Server-side writes, checked against git.
//!
//! * A changeset created on the server has the same commit id as the commit
//!   `git commit` makes from the same edits.
//! * Rebasing a changeset agrees with `git merge-tree --merge-base`: the same
//!   tree when the merge is clean, the same conflicted paths when it is not.
//! * Concurrent lands onto one branch all land, linearly, and the git host
//!   (the source of truth in write-proxy mode) ends up with Wren's history.
//! * A revert landed on a branch gives the tree git's revert would.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use wren_core::changes::FileVersion;
use wren_core::object::{Kind, MODE_BLOB, MODE_EXEC, MODE_SYMLINK};
use wren_core::write::{signature, FileEdit, Landed, NewChangeset, Rebased};
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

/// A bare "host" repository, a working clone of it, and a Wren repo synced
/// from the host.
struct Fixture {
    _tmp: tempfile::TempDir,
    host: PathBuf,
    work: PathBuf,
    repo: Arc<Repo>,
}

fn fixture(files: &[(&str, String)]) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let host = tmp.path().join("host.git");
    let work = tmp.path().join("work");
    git(
        tmp.path(),
        &["init", "-q", "--bare", "-b", "main", host.to_str().unwrap()],
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
    for (p, content) in files {
        let path = work.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "--allow-empty", "-m", "root"]);
    git(&work, &["push", "-q", "origin", "main"]);
    let store = Arc::new(ObjectStore::open(tmp.path().join("objects")).unwrap());
    let repo = Repo::open(
        tmp.path().join("repo"),
        "t",
        store,
        Some(host.to_str().unwrap()),
    )
    .unwrap();
    repo.sync().unwrap();
    Fixture {
        _tmp: tmp,
        host,
        work,
        repo: Arc::new(repo),
    }
}

fn put_blob(repo: &Repo, content: &[u8]) -> Oid {
    repo.store.put(Kind::Blob, content).unwrap()
}

fn lines(tag: &str, n: usize) -> String {
    (0..n).map(|i| format!("{tag} line {i}\n")).collect()
}

// ---- 1. created changesets are git commits ---------------------------------

#[derive(Clone)]
struct FileState {
    mode: u32,
    content: Vec<u8>,
}

#[test]
fn created_changesets_match_git_commits() {
    let f = fixture(&[("README", "hello\n".into())]);
    let (repo, work) = (&f.repo, &f.work);
    let mut files: BTreeMap<String, FileState> = BTreeMap::new();
    files.insert(
        "README".into(),
        FileState {
            mode: MODE_BLOB,
            content: b"hello\n".to_vec(),
        },
    );
    let mut rng = Rng(0x5eed);
    let mut parent = repo.resolve("main").unwrap();
    let names = ["a", "b", "c.txt", "d/e", "d/f/g", "x", "x/y", "z.sh"];
    for round in 0..60 {
        let mut edits: BTreeMap<String, Option<FileState>> = BTreeMap::new();
        for _ in 0..1 + rng.below(4) {
            let existing: Vec<String> = files.keys().cloned().collect();
            match rng.below(6) {
                // modify
                0 | 1 if !existing.is_empty() => {
                    let p = existing[rng.below(existing.len())].clone();
                    let mut st = files[&p].clone();
                    st.content = format!("v{round} {}\n", rng.next()).into_bytes();
                    edits.insert(p, Some(st));
                }
                // delete
                2 if existing.len() > 1 => {
                    let p = existing[rng.below(existing.len())].clone();
                    edits.insert(p, None);
                }
                // toggle the executable bit
                3 if !existing.is_empty() => {
                    let p = existing[rng.below(existing.len())].clone();
                    let mut st = files[&p].clone();
                    if st.mode != MODE_SYMLINK {
                        st.mode = if st.mode == MODE_EXEC {
                            MODE_BLOB
                        } else {
                            MODE_EXEC
                        };
                        edits.insert(p, Some(st));
                    }
                }
                // add (a file or a symlink), deleting files in the way
                _ => {
                    let p = names[rng.below(names.len())].to_string();
                    let mut prefix = String::new();
                    for part in p.split('/').collect::<Vec<_>>().split_last().unwrap().1 {
                        prefix = if prefix.is_empty() {
                            part.to_string()
                        } else {
                            format!("{prefix}/{part}")
                        };
                        if files.contains_key(&prefix) {
                            edits.insert(prefix.clone(), None);
                        }
                    }
                    let under: Vec<String> = files
                        .keys()
                        .filter(|k| k.starts_with(&format!("{p}/")))
                        .cloned()
                        .collect();
                    for k in under {
                        edits.insert(k, None);
                    }
                    let st = if rng.chance(20) {
                        FileState {
                            mode: MODE_SYMLINK,
                            content: b"README".to_vec(),
                        }
                    } else {
                        FileState {
                            mode: MODE_BLOB,
                            content: format!("new {round}\n").into_bytes(),
                        }
                    };
                    edits.insert(p, Some(st));
                }
            }
        }
        // Drop edits that conflict with each other (a file added below a
        // path another edit adds as a file).
        let adds: Vec<String> = edits
            .iter()
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| k.clone())
            .collect();
        for a in &adds {
            if adds.iter().any(|b| a.starts_with(&format!("{b}/"))) {
                edits.remove(a);
            }
        }

        // git
        for (p, st) in &edits {
            let path = work.join(p);
            match st {
                None => {
                    git(work, &["rm", "-q", "--", p]);
                }
                Some(st) => {
                    if path.is_dir() {
                        std::fs::remove_dir_all(&path).unwrap();
                    }
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    let _ = std::fs::remove_file(&path);
                    if st.mode == MODE_SYMLINK {
                        std::os::unix::fs::symlink(
                            std::str::from_utf8(&st.content).unwrap(),
                            &path,
                        )
                        .unwrap();
                    } else {
                        std::fs::write(&path, &st.content).unwrap();
                        use std::os::unix::fs::PermissionsExt;
                        let mode = if st.mode == MODE_EXEC { 0o755 } else { 0o644 };
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                            .unwrap();
                    }
                }
            }
        }
        git(work, &["add", "-A"]);
        let msg = format!("round {round}");
        git(work, &["commit", "-q", "--allow-empty", "-m", &msg]);
        let expected = oid(&git(work, &["rev-parse", "HEAD"]));

        // Wren
        let cs = NewChangeset {
            parents: vec![parent],
            author: SIG.as_bytes().to_vec(),
            committer: None,
            message: msg.into_bytes(),
            edits: edits
                .iter()
                .map(|(p, st)| FileEdit {
                    path: p.clone(),
                    new: st.as_ref().map(|st| FileVersion {
                        mode: st.mode,
                        oid: put_blob(repo, &st.content),
                    }),
                })
                .collect(),
        };
        let got = repo.create_changeset(&cs).unwrap();
        assert_eq!(got, expected, "round {round}: {:?}", edits.keys());
        // Creating it again is idempotent.
        assert_eq!(repo.create_changeset(&cs).unwrap(), expected);
        parent = got;
        for (p, st) in edits {
            match st {
                Some(st) => files.insert(p, st),
                None => files.remove(&p),
            };
        }
    }
}

#[test]
fn invalid_edits_are_rejected() {
    let f = fixture(&[("a/b", "x\n".into()), ("c", "y\n".into())]);
    let repo = &f.repo;
    let parent = repo.resolve("main").unwrap();
    let blob = put_blob(repo, b"z\n");
    let v = |mode| Some(FileVersion { mode, oid: blob });
    let bad: Vec<Vec<FileEdit>> = vec![
        vec![FileEdit {
            path: "../etc".into(),
            new: v(MODE_BLOB),
        }],
        vec![FileEdit {
            path: ".git/config".into(),
            new: v(MODE_BLOB),
        }],
        vec![FileEdit {
            path: "/abs".into(),
            new: v(MODE_BLOB),
        }],
        vec![FileEdit {
            path: "a".into(),
            new: v(MODE_BLOB),
        }], // a directory
        vec![FileEdit {
            path: "c/d".into(),
            new: v(MODE_BLOB),
        }], // below a file
        vec![FileEdit {
            path: "nope".into(),
            new: None,
        }],
        vec![FileEdit {
            path: "e".into(),
            new: v(0o100664),
        }],
        vec![FileEdit {
            path: "e".into(),
            new: Some(FileVersion {
                mode: MODE_BLOB,
                oid: Oid::hash_object("blob", b"never uploaded"),
            }),
        }],
    ];
    for edits in bad {
        let r = repo.create_changeset(&NewChangeset {
            parents: vec![parent],
            author: SIG.as_bytes().to_vec(),
            committer: None,
            message: b"bad".to_vec(),
            edits: edits.clone(),
        });
        let e = r.expect_err(&format!("{edits:?} should be rejected"));
        assert!(
            e.downcast_ref::<wren_core::BadRequest>().is_some(),
            "{edits:?}: {e:#}"
        );
    }
    assert!(signature("a<b", "x", 0, "+0000").is_err());
    assert!(signature("a", "x", 0, "0000").is_err());
}

// ---- 2. rebase agrees with git merge-tree ----------------------------------

/// Random line edits to a few shared files; sometimes deletions and adds of
/// the same new path, so content, add/add and modify/delete conflicts occur.
fn random_edit(work: &Path, rng: &mut Rng, tag: &str) {
    let files = ["f0", "f1", "f2", "f3"];
    for _ in 0..1 + rng.below(3) {
        let f = files[rng.below(files.len())];
        let path = work.join(f);
        let r = rng.below(10);
        if r == 0 && path.exists() {
            std::fs::remove_file(&path).unwrap();
            continue;
        }
        if r == 1 {
            let p = work.join(format!("new{}", rng.below(2)));
            std::fs::write(p, format!("{tag} {}\n", rng.below(3))).unwrap();
            continue;
        }
        let mut ls: Vec<String> = match std::fs::read_to_string(&path) {
            Ok(s) => s.lines().map(|l| l.to_string()).collect(),
            Err(_) => lines(f, 12).lines().map(|l| l.to_string()).collect(),
        };
        for _ in 0..1 + rng.below(2) {
            let i = rng.below(ls.len().max(1));
            match rng.below(3) {
                0 if !ls.is_empty() => {
                    ls.remove(i);
                }
                1 => ls.insert(i, format!("{tag} inserted {}", rng.below(100))),
                _ if !ls.is_empty() => ls[i] = format!("{tag} changed {}", rng.below(100)),
                _ => {}
            }
        }
        std::fs::write(
            &path,
            ls.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();
        if rng.chance(5) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "--allow-empty", "-m", tag]);
}

#[test]
fn rebase_agrees_with_git_merge_tree() {
    let initial: Vec<(&str, String)> = ["f0", "f1", "f2", "f3"]
        .iter()
        .map(|f| (*f, lines(f, 12)))
        .collect();
    let f = fixture(&initial);
    let (repo, work) = (&f.repo, &f.work);
    let mut rng = Rng(0xbeef);
    let (mut clean, mut conflicted) = (0, 0);
    for round in 0..80 {
        let base = git(work, &["rev-parse", "HEAD"]).trim().to_string();
        let side = format!("side{round}");
        git(work, &["checkout", "-q", "-b", &side]);
        random_edit(work, &mut rng, &format!("side{round}"));
        let x = git(work, &["rev-parse", "HEAD"]).trim().to_string();
        git(work, &["checkout", "-q", "main"]);
        for i in 0..1 + rng.below(2) {
            random_edit(work, &mut rng, &format!("main{round}.{i}"));
        }
        let t = git(work, &["rev-parse", "HEAD"]).trim().to_string();
        git(work, &["push", "-q", "origin", "main", &side]);
        repo.sync().unwrap();

        let (ok, out) = git_out(
            work,
            &[
                "-c",
                "merge.renames=false",
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                &format!("--merge-base={base}"),
                &t,
                &x,
            ],
        );
        let mut out_lines = out.lines();
        let git_tree = oid(out_lines.next().unwrap());
        let git_conflicts: BTreeSet<String> = out_lines
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect();

        match repo.rebase(&[oid(&x)], oid(&t), None).unwrap() {
            Rebased::Done {
                changesets,
                dropped,
            } => {
                assert!(
                    ok,
                    "round {round}: git conflicts on {git_conflicts:?}, Wren merged"
                );
                let tree = match changesets.last() {
                    Some(c) => repo.tree_of(c).unwrap(),
                    None => {
                        assert_eq!(dropped, vec![oid(&x)]);
                        repo.tree_of(&oid(&t)).unwrap()
                    }
                };
                assert_eq!(tree, git_tree, "round {round}: merged trees differ");
                clean += 1;
            }
            Rebased::Conflicts(c) => {
                assert!(!ok, "round {round}: Wren conflicts on {c:?}, git merged");
                let ours: BTreeSet<String> = c.iter().map(|c| c.path.clone()).collect();
                assert_eq!(
                    ours, git_conflicts,
                    "round {round}: conflicted paths differ"
                );
                conflicted += 1;
            }
        }
        // Keep main moving: land nothing, just continue from main.
    }
    eprintln!("rebase vs merge-tree: {clean} clean, {conflicted} conflicted");
    assert!(
        clean > 10 && conflicted > 5,
        "{clean} clean, {conflicted} conflicted"
    );
}

// ---- 3. concurrent lands -----------------------------------------------------

#[test]
fn concurrent_lands_all_land_linearly() {
    let f = fixture(&[("README", "hello\n".into())]);
    let repo = f.repo.clone();
    let base = repo.resolve("main").unwrap();
    const N: usize = 12;
    const OUTSIDE: usize = 4;
    // Meanwhile, someone pushes to the host directly, behind Wren's back:
    // lands lose the host's lease, sync, and replay.
    let work = f.work.clone();
    let (first_push, pushed) = std::sync::mpsc::channel();
    let outsider = std::thread::spawn(move || {
        for i in 0..OUTSIDE {
            std::fs::create_dir_all(work.join("outside")).unwrap();
            std::fs::write(work.join(format!("outside/{i}")), format!("{i}\n")).unwrap();
            git(&work, &["add", "-A"]);
            git(&work, &["commit", "-q", "-m", &format!("outside {i}")]);
            while !git_out(&work, &["push", "-q", "origin", "main"]).0 {
                git(&work, &["pull", "-q", "--rebase", "origin", "main"]);
            }
            if i == 0 {
                first_push.send(()).unwrap();
            }
        }
    });
    // Wren has not seen the outsider's first push, so the first land must
    // lose the host's lease, sync and replay.
    pushed.recv().unwrap();
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let repo = repo.clone();
            std::thread::spawn(move || {
                let blob = put_blob(&repo, format!("agent {i}\n").as_bytes());
                let cs = repo
                    .create_changeset(&NewChangeset {
                        parents: vec![base],
                        author: SIG.as_bytes().to_vec(),
                        committer: None,
                        message: format!("agent {i}").into_bytes(),
                        edits: vec![FileEdit {
                            path: format!("agents/{i}"),
                            new: Some(FileVersion {
                                mode: MODE_BLOB,
                                oid: blob,
                            }),
                        }],
                    })
                    .unwrap();
                match repo.land(&[cs], "main", None).unwrap() {
                    Landed::Landed { attempts, .. } => attempts,
                    other => panic!("agent {i}: {other:?}"),
                }
            })
        })
        .collect();
    let attempts: Vec<u32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    outsider.join().unwrap();
    eprintln!("land attempts: {attempts:?}");
    assert!(attempts.iter().any(|&a| a > 1), "no land had to retry");

    // After a sync, Wren has exactly the host's main.
    repo.sync().unwrap();
    let tip = repo.resolve("main").unwrap();
    let host_main = oid(&git(&f.host, &["rev-parse", "refs/heads/main"]));
    assert_eq!(host_main, tip);
    // Linear: every agent's and the outsider's commit is on the first-parent
    // chain, each a single-parent commit, and every file is in the tree.
    let log = repo.log(&tip, None, 100).unwrap();
    assert_eq!(log.len(), N + OUTSIDE + 1);
    for c in &log[..N + OUTSIDE] {
        assert_eq!(repo.commit(c).unwrap().parents.len(), 1);
    }
    let ls = git(&f.host, &["ls-tree", "-r", "--name-only", "main", "agents"]);
    assert_eq!(ls.lines().count(), N);
    let ls = git(
        &f.host,
        &["ls-tree", "-r", "--name-only", "main", "outside"],
    );
    assert_eq!(ls.lines().count(), OUTSIDE);
    // git agrees the history is sound.
    git(&f.host, &["fsck", "--no-dangling"]);
}

#[test]
fn land_after_the_host_moved_rebases() {
    let f = fixture(&[("a", lines("a", 10)), ("b", lines("b", 10))]);
    let (repo, work) = (&f.repo, &f.work);
    let base = repo.resolve("main").unwrap();
    // Someone pushes to the host directly; Wren has not synced.
    std::fs::write(work.join("a"), lines("a2", 10)).unwrap();
    git(work, &["commit", "-qam", "direct push"]);
    git(work, &["push", "-q", "origin", "main"]);
    let host_tip = oid(&git(work, &["rev-parse", "HEAD"]));

    let blob = put_blob(repo, lines("b2", 10).as_bytes());
    let cs = repo
        .create_changeset(&NewChangeset {
            parents: vec![base],
            author: SIG.as_bytes().to_vec(),
            committer: None,
            message: b"edit b".to_vec(),
            edits: vec![FileEdit {
                path: "b".into(),
                new: Some(FileVersion {
                    mode: MODE_BLOB,
                    oid: blob,
                }),
            }],
        })
        .unwrap();
    let Landed::Landed {
        old, new, attempts, ..
    } = repo.land(&[cs], "main", None).unwrap()
    else {
        panic!("did not land")
    };
    assert_eq!(attempts, 2, "the first push loses the lease");
    assert_eq!(old, host_tip);
    assert_eq!(repo.commit(&new).unwrap().parents, vec![host_tip]);
    assert_eq!(oid(&git(&f.host, &["rev-parse", "main"])), new);
}

// ---- 4. revert ---------------------------------------------------------------

#[test]
fn revert_matches_git() {
    let f = fixture(&[("a", lines("a", 20)), ("b", lines("b", 20))]);
    let (repo, work) = (&f.repo, &f.work);
    let mut rng = Rng(7);
    for i in 0..6 {
        random_edit(work, &mut rng, &format!("c{i}"));
    }
    git(work, &["push", "-q", "origin", "main"]);
    repo.sync().unwrap();
    for back in 1..5 {
        let x = git(work, &["rev-parse", &format!("main~{back}")])
            .trim()
            .to_string();
        let xp = format!("{x}^");
        let (ok, out) = git_out(
            work,
            &[
                "-c",
                "merge.renames=false",
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                &format!("--merge-base={x}"),
                "main",
                &xp,
            ],
        );
        let inverse = repo.revert(&oid(&x), SIG.as_bytes(), None).unwrap();
        assert_eq!(
            repo.tree_of(&inverse).unwrap(),
            oid(&git(work, &["rev-parse", &format!("{xp}^{{tree}}")])),
            "the inverse of {x} restores its parent's tree"
        );
        let main = repo.resolve("main").unwrap();
        match repo.rebase(&[inverse], main, None).unwrap() {
            Rebased::Done { changesets, .. } => {
                assert!(ok, "git could not revert main~{back}");
                let tree = changesets
                    .last()
                    .map(|c| repo.tree_of(c).unwrap())
                    .unwrap_or_else(|| repo.tree_of(&main).unwrap());
                assert_eq!(
                    tree,
                    oid(out.lines().next().unwrap()),
                    "revert of main~{back}"
                );
            }
            Rebased::Conflicts(_) => assert!(!ok, "Wren could not revert main~{back}, git could"),
        }
    }
}
