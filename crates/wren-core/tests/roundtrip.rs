//! Round trip over a real repository: ingest it, then check that every
//! commit reachable from branches and tags (what Wren mirrors) has a derived
//! tree and commit object that reproduce git's ids.
//! Ignored by default; run with
//!
//! ```sh
//! WREN_ROUNDTRIP_REPO=/path/to/repo.git \
//!   cargo test --release -p wren-vcs-core --test roundtrip -- --ignored --nocapture
//! ```

use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use wren_core::{ObjectStore, Oid, Repo};

#[test]
#[ignore]
fn real_repository_roundtrip() {
    let src = std::env::var("WREN_ROUNDTRIP_REPO").expect("set WREN_ROUNDTRIP_REPO");
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(ObjectStore::open(tmp.path().join("objects")).unwrap());
    let repo = Repo::open(tmp.path().join("repo"), "rt", store, Some(&src)).unwrap();

    let t = Instant::now();
    let report = repo.sync().unwrap();
    eprintln!("ingest: {report:?} in {:.1}s", t.elapsed().as_secs_f64());
    let (trees, commits) = repo.meta.override_counts().unwrap();
    eprintln!(
        "overrides: {trees} trees, {commits} commits of {} changesets",
        report.new_changesets
    );

    // Show a few overridden commits and why derivation differed.
    let overridden = repo.meta.overridden_changesets(5).unwrap();
    for c in overridden {
        let row = repo.meta.changeset(&c).unwrap().unwrap();
        let base = row.parents.first().map(|p| repo.tree_of(p).unwrap());
        let changes = wren_core::changes::decode_changes(&row.changes).unwrap();
        let derived = wren_core::treegen::apply(&repo.store, base, &changes);
        eprintln!(
            "override {c}: git tree {}, derived {derived:?}, {} changes",
            row.tree_override.unwrap(),
            changes.len()
        );
    }

    let out = Command::new("git")
        .args(["-C", &src, "log", "--branches", "--tags", "--format=%H %T"])
        .output()
        .unwrap();
    let t = Instant::now();
    let mut n = 0;
    for l in String::from_utf8(out.stdout).unwrap().lines() {
        let (c, tree) = l.split_once(' ').unwrap();
        let (c, tree): (Oid, Oid) = (c.parse().unwrap(), tree.parse().unwrap());
        assert_eq!(repo.tree_of(&c).unwrap(), tree, "derived tree of {c}");
        let obj = repo.commit_object(&c).unwrap();
        assert_eq!(Oid::hash_object("commit", &obj), c, "commit object of {c}");
        n += 1;
    }
    eprintln!(
        "round trip: {n} commits verified in {:.1}s",
        t.elapsed().as_secs_f64()
    );
}
