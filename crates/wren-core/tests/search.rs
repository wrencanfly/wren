//! The content index never changes an answer: `grep` with the index equals
//! `grep` reading every file, for random files (mixed case, non-ASCII,
//! the Kelvin sign and the long s, binary files, a large file) and random
//! patterns (literals, concatenations, classes, repetitions, alternations,
//! case-insensitive), at the indexed tip and at older revisions, through
//! incremental indexing and compaction.

use std::path::Path;
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
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@wren.dev"])
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

const WORDS: &[&str] = &[
    "alpha",
    "Beta",
    "GAMMA",
    "delta_x",
    "mask_bits",
    "Kelvin",
    "\u{212a}elvin",
    "ſtream",
    "stream",
    "café",
    "CAFÉ",
    "foo(bar)",
    "x+y",
    "tab\there",
    "zeta",
    "TODO",
    "fixme",
    "Ok",
];

fn line(rng: &mut Rng) -> String {
    let n = 1 + rng.below(5);
    let mut ws: Vec<&str> = (0..n).map(|_| WORDS[rng.below(WORDS.len())]).collect();
    if rng.below(4) == 0 {
        ws.push("12345");
    }
    ws.join(" ")
}

fn write_file(dir: &Path, rng: &mut Rng, name: &str) {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut body: Vec<u8> = (0..1 + rng.below(12))
        .flat_map(|_| format!("{}\n", line(rng)).into_bytes())
        .collect();
    if rng.below(15) == 0 {
        body.insert(rng.below(body.len()), 0); // binary
    }
    std::fs::write(p, body).unwrap();
}

const PATTERNS: &[(&str, bool)] = &[
    ("alpha", false),
    ("alpha", true),
    ("ALPHA", true),
    ("Beta GAMMA", false),
    ("beta gamma", true),
    ("mask_bits", true),
    ("kelvin", true),
    ("Kelvin", false),
    ("stream", true),
    ("STREAM", true),
    ("café", false),
    ("café", true),
    ("delta_x.*zeta", false),
    ("foo\\(bar\\)", false),
    ("x\\+y", false),
    ("(alpha|zeta) Ok", false),
    ("[Tt][Oo][Dd][Oo]", false),
    ("(fix)+me", false),
    ("12345$", false),
    ("^Ok", false),
    ("tab\there", false),
    ("a.p.a", false),
    ("\\bBeta\\b", false),
    ("nothing-like-this", false),
    ("gam+a", true),
];

#[test]
fn indexed_grep_equals_scanning() {
    let tmp = tempfile::tempdir().unwrap();
    let up = tmp.path().join("up");
    std::fs::create_dir_all(&up).unwrap();
    git(&up, &["init", "-q", "-b", "main"]);
    let mut rng = Rng(0x5eed);
    for i in 0..120 {
        write_file(&up, &mut rng, &format!("d{}/f{i}.txt", i % 7));
    }
    // A file past the size limit: never indexed, always read.
    let big: String = (0..80_000)
        .map(|i| format!("big line {i} alpha\n"))
        .collect();
    std::fs::write(up.join("big.txt"), big).unwrap();
    git(&up, &["add", "-A"]);
    git(&up, &["commit", "-qm", "start"]);

    let store = Arc::new(ObjectStore::open(tmp.path().join("objects")).unwrap());
    let repo = Repo::open(
        tmp.path().join("repo"),
        "t",
        store,
        Some(up.to_str().unwrap()),
    )
    .unwrap();
    repo.sync().unwrap();
    let r = repo.index_content().unwrap();
    assert!(r.indexed > 100 && r.skipped > 0, "{r:?}");
    let mut revs: Vec<Oid> = vec![repo.resolve("main").unwrap()];

    let check = |repo: &Repo, revs: &[Oid], what: &str| {
        for rev in revs {
            for (p, ic) in PATTERNS {
                for path in ["", "d3"] {
                    std::env::remove_var("WREN_NO_CONTENT_INDEX");
                    let with = repo.grep(rev, p, path, *ic, 100_000).unwrap();
                    std::env::set_var("WREN_NO_CONTENT_INDEX", "1");
                    let without = repo.grep(rev, p, path, *ic, 100_000).unwrap();
                    std::env::remove_var("WREN_NO_CONTENT_INDEX");
                    let a: Vec<_> = with.matches.iter().map(|m| (&m.path, m.line)).collect();
                    let b: Vec<_> = without.matches.iter().map(|m| (&m.path, m.line)).collect();
                    assert_eq!(a, b, "{what}: {p:?} ic={ic} path={path:?} at {rev}");
                }
            }
        }
    };
    check(&repo, &revs, "initial");

    // A selective pattern reads few files.
    let mut files = Vec::new();
    repo.collect_files(repo.tree_of(&revs[0]).unwrap(), "", &mut files)
        .unwrap();
    let all = files.len();
    repo.prune_by_index(
        &mut files,
        &wren_core::search::required_trigrams("nothing-like-this", false).unwrap(),
    )
    .unwrap();
    assert_eq!(files.len(), 1, "only the unindexed big file is left");
    assert!(all > 100);

    // History moves; the index follows a commit at a time (each run is a
    // batch, so compaction happens along the way).
    for round in 0..40 {
        for _ in 0..1 + rng.below(3) {
            let i = rng.below(140);
            write_file(&up, &mut rng, &format!("d{}/f{i}.txt", i % 7));
        }
        if round % 5 == 0 {
            let _ = std::fs::remove_file(up.join(format!("d1/f{}.txt", 7 * round + 1)));
        }
        git(&up, &["add", "-A"]);
        git(&up, &["commit", "-qm", &format!("round {round}")]);
        repo.sync().unwrap();
        repo.index_content().unwrap();
        revs.push(repo.resolve("main").unwrap());
    }
    // The tip, and older revisions (whose blobs are partly indexed).
    let sample: Vec<Oid> = [0, 7, 21, revs.len() - 1]
        .iter()
        .map(|&i| revs[i])
        .collect();
    check(&repo, &sample, "after updates");
}
