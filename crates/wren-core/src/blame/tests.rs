//! Edge cases of the change chain, checked against tree lookups and git.

use super::*;
use crate::store::ObjectStore;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
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
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn oids(s: &str) -> Vec<Oid> {
    s.lines().map(|l| Oid::from_hex(l).unwrap()).collect()
}

struct Up<'a> {
    dir: &'a Path,
    n: usize,
}

impl Up<'_> {
    fn init(dir: &Path) -> Up<'_> {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        Up { dir, n: 0 }
    }
    fn lines(&mut self, k: usize) -> String {
        (0..k)
            .map(|_| {
                self.n += 1;
                format!("line {}\n", self.n)
            })
            .collect()
    }
    fn write(&self, p: &str, s: &str) {
        let f = self.dir.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, s).unwrap();
    }
    fn read(&self, p: &str) -> Vec<String> {
        std::fs::read_to_string(self.dir.join(p))
            .unwrap()
            .lines()
            .map(|l| format!("{l}\n"))
            .collect()
    }
    fn rm(&self, p: &str) {
        git(self.dir, &["rm", "-rq", p]);
    }
    fn symlink(&self, p: &str, target: &str) {
        let f = self.dir.join(p);
        let _ = std::fs::remove_file(&f);
        std::os::unix::fs::symlink(target, f).unwrap();
    }
    fn commit(&self, msg: &str) {
        git(self.dir, &["add", "-A"]);
        git(self.dir, &["commit", "-q", "--allow-empty", "-m", msg]);
    }
    /// Replace line `i` of `p`.
    fn edit(&mut self, p: &str, i: usize) {
        let mut v = self.read(p);
        v[i] = self.lines(1);
        self.write(p, &v.concat());
    }
    fn repo(&self, root: &Path, name: &str) -> Repo {
        let store = Arc::new(ObjectStore::open(root.join("objects")).unwrap());
        let r = Repo::open(
            root.join(name),
            "t",
            store,
            Some(self.dir.to_str().unwrap()),
        )
        .unwrap();
        r.sync().unwrap();
        r
    }
}

/// The change chain as the first implementation walked it: one `log(.., 1)`
/// and three tree lookups per step.
fn chain_by_trees(repo: &Repo, rev: &Oid, path: &str) -> Vec<Step> {
    let last_change = |tip: &Oid| repo.log(tip, Some(path), 1).unwrap().into_iter().next();
    let mut out = Vec::new();
    let mut cur = last_change(rev).unwrap();
    loop {
        let blob = repo.file_blob(&cur, path).unwrap().unwrap();
        let parent = repo.node(&cur).unwrap().parents.first().copied();
        let prev = parent.and_then(|p| repo.file_blob(&p, path).unwrap());
        out.push(Step {
            commit: cur,
            blob,
            prev,
        });
        match (parent, prev) {
            (Some(p), Some(_)) => cur = last_change(&p).unwrap(),
            _ => return out,
        }
    }
}

fn git_blame(dir: &Path, rev: &Oid, path: &str) -> Vec<(Oid, u32)> {
    let rev = rev.to_hex();
    git(
        dir,
        &["blame", "--first-parent", "--porcelain", &rev, "--", path],
    )
    .lines()
    .filter_map(|l| {
        let p: Vec<&str> = l.split(' ').collect();
        (p.len() >= 3 && p[0].len() == 40 && p[0].bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| (Oid::from_hex(p[0]).unwrap(), p[1].parse().unwrap()))
    })
    .collect()
}

fn ours(repo: &Repo, rev: &Oid, path: &str) -> Option<Vec<(Oid, u32)>> {
    repo.blame(rev, path)
        .unwrap()
        .map(|v| v.into_iter().map(|l| (l.commit, l.orig_line)).collect())
}

fn persisted(repo: &Repo, path: &str) -> usize {
    repo.meta
        .derived_keys_with_prefix(KIND, &key_prefix(path))
        .unwrap()
        .len()
}

/// Paths that change kind (file, executable, symlink, directory, submodule,
/// absent): change records must say exactly what tree lookups say, the
/// one-pass chain must equal the step-by-step walk, and blame must match git.
#[test]
fn change_records_match_trees_and_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("up");
    let mut up = Up::init(&dir);
    for (p, n) in [("p", 12), ("q", 5), ("g", 3)] {
        let s = up.lines(n);
        up.write(p, &s);
    }
    up.commit("root");
    up.edit("p", 2);
    up.commit("edit p");
    git(&dir, &["update-index", "--chmod=+x", "p"]);
    up.commit("mode-only change");
    up.edit("p", 5);
    up.commit("edit p again");
    let kept = up.read("p").concat();
    up.symlink("p", "target-one");
    up.commit("p becomes a symlink");
    up.symlink("p", "target-two");
    up.commit("retarget symlink");
    std::fs::remove_file(dir.join("p")).unwrap();
    up.write("p", &format!("target-two\n{kept}"));
    up.commit("symlink becomes a file");
    up.rm("p");
    up.commit("delete p");
    up.write("p", &kept);
    up.commit("re-add p with old contents");
    up.rm("p");
    let x = up.lines(6);
    up.write("p/x", &x);
    up.commit("p becomes a directory");
    up.edit("p/x", 1);
    up.commit("edit p/x");
    up.rm("p");
    let fresh = up.lines(12);
    up.write("p", &fresh);
    up.commit("p is a file again");
    // g: a file that becomes a submodule (gitlink) and then a file again.
    let root = git(&dir, &["rev-list", "--max-parents=0", "HEAD"]);
    let g = up.read("g").concat();
    up.rm("g");
    let gitlink = format!("160000,{},g", root.trim());
    git(&dir, &["update-index", "--add", "--cacheinfo", &gitlink]);
    git(&dir, &["commit", "-q", "-m", "g becomes a submodule"]);
    git(&dir, &["rm", "-q", "--cached", "g"]);
    up.write("g", &g);
    up.commit("g is a file again");
    // A side branch edits p while main does too, merged with --no-ff.
    git(&dir, &["checkout", "-q", "-b", "side"]);
    up.edit("p", 0);
    up.commit("side edit 1");
    up.edit("p", 1);
    up.commit("side edit 2");
    git(&dir, &["checkout", "-q", "main"]);
    up.edit("p", 8);
    up.commit("main edit");
    git(
        &dir,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    up.edit("p", 4);
    up.commit("edit after merge");

    let repo = up.repo(tmp.path(), "repo");
    let all = oids(&git(&dir, &["rev-list", "--all"]));
    let main = oids(&git(&dir, &["rev-list", "--first-parent", "main"]));
    let paths = ["p", "p/x", "q", "g"];

    // 1. For every commit that changed a path (as a file or a directory),
    //    the change record agrees with tree lookups.
    for path in paths {
        let changed = repo
            .meta
            .path_changes(path, (u32::MAX, None), 1000)
            .unwrap();
        assert!(!changed.is_empty());
        for (_, c) in changed {
            assert_eq!(
                repo.file_versions(&c, path, true).unwrap(),
                repo.file_versions(&c, path, false).unwrap(),
                "{path} @ {c}"
            );
        }
    }
    // 2. The one-pass chain equals the step-by-step walk from every commit
    //    where the path is a file (nothing is persisted yet).
    let mut chains = 0;
    for path in paths {
        for rev in &all {
            if repo.file_blob(rev, path).unwrap().is_none() {
                continue;
            }
            let chain = repo.change_chain(rev, path).unwrap();
            assert!(chain.base.is_none());
            assert_eq!(
                chain.steps,
                chain_by_trees(&repo, rev, path),
                "{path} @ {rev}"
            );
            chains += 1;
        }
    }
    assert!(chains > 40, "{chains}");

    // 3. Blame agrees with git wherever git's answer is defined the same way.
    for path in ["p", "p/x", "q"] {
        for rev in &all {
            match ours(&repo, rev, path) {
                Some(got) => assert_eq!(got, git_blame(&dir, rev, path), "{path} @ {rev}"),
                None => assert!(repo.file_blob(rev, path).unwrap().is_none()),
            }
        }
    }
    // A submodule is not blamed, and a file that replaces one starts afresh
    // (git would read the gitlink's commit object as the old contents).
    let subject = |c: &Oid| git(&dir, &["log", "-1", "--format=%s", &c.to_hex()]);
    let g_sub = *main
        .iter()
        .find(|c| subject(c).trim() == "g becomes a submodule")
        .unwrap();
    let g_back = *main
        .iter()
        .find(|c| subject(c).trim() == "g is a file again")
        .unwrap();
    assert_eq!(ours(&repo, &g_sub, "g"), None);
    let expect: Vec<(Oid, u32)> = (1..=3).map(|l| (g_back, l)).collect();
    assert_eq!(ours(&repo, &main[0], "g").unwrap(), expect);
    // Directories are not blamed either.
    let p_dir = *main
        .iter()
        .find(|c| subject(c).trim() == "edit p/x")
        .unwrap();
    assert_eq!(ours(&repo, &p_dir, "p"), None);
    assert!(ours(&repo, &p_dir, "p/x").is_some());
}

/// Only results and checkpoints are persisted; an older revision is derived
/// from the nearest checkpoint; answers never depend on what was persisted.
#[test]
fn checkpoints_and_purity() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("up");
    let mut up = Up::init(&dir);
    let f = up.lines(30);
    up.write("f", &f);
    up.commit("root");
    for i in 0..100 {
        if i % 5 == 0 {
            let mut v = up.read("f");
            let extra = up.lines(3);
            v.insert((i * 3) % v.len(), extra);
            up.write("f", &v.concat());
        } else {
            up.edit("f", (i * 7) % 30);
        }
        up.commit(&format!("c{i}"));
    }
    let main = oids(&git(&dir, &["rev-list", "--first-parent", "main"]));
    assert_eq!(main.len(), 101);

    // Newest first: one derivation stores steps 32, 64, 96 and the result.
    let a = up.repo(tmp.path(), "a");
    ours(&a, &main[0], "f").unwrap();
    assert_eq!(persisted(&a, "f"), 4);
    assert_eq!(a.change_chain(&main[0], "f").unwrap().steps.len(), 0);
    // An older revision (step 60) starts from the checkpoint at step 32.
    let chain = a.change_chain(&main[40], "f").unwrap();
    assert_eq!(chain.steps.len(), 60 - 32);
    assert_eq!(chain.base, Some(main[100 - 32]));
    ours(&a, &main[40], "f").unwrap();
    assert_eq!(persisted(&a, "f"), 5);
    assert_eq!(a.change_chain(&main[40], "f").unwrap().steps.len(), 0);
    // One change past a persisted result costs one step.
    assert_eq!(a.change_chain(&main[39], "f").unwrap().steps.len(), 1);

    // A second store, queried in a scrambled order, agrees everywhere, and
    // so does git.
    let b = up.repo(tmp.path(), "b");
    let mut order: Vec<usize> = (0..main.len()).collect();
    order.sort_by_key(|i| (i * 37) % main.len());
    for &i in &order {
        let got = ours(&b, &main[i], "f");
        assert_eq!(got, ours(&a, &main[i], "f"), "rev {i}");
        if i % 7 == 0 {
            assert_eq!(got.unwrap(), git_blame(&dir, &main[i], "f"), "rev {i}");
        }
    }
}
