//! The metadata backends are interchangeable: the same operations give the
//! same answers on SQLite and Postgres, including compare-and-swap edge
//! cases, ordering, prefix scans and idempotent inserts.

use std::sync::Arc;

use wren_core::meta::{ChangesetRow, MetaConfig, MetaDb, WriteOp};
use wren_core::testpg::TestPg;
use wren_core::Oid;

fn oid(i: u8) -> Oid {
    Oid::from_bytes(&[i; 20]).unwrap()
}

fn row(o: Oid) -> ChangesetRow {
    ChangesetRow {
        seq: 0,
        oid: o,
        parents: vec![oid(1)],
        author: b"A <a@x> 1 +0000".to_vec(),
        committer: None,
        extra_headers: Vec::new(),
        message: b"m\n".to_vec(),
        commit_time: 1,
        changes: vec![1, 2, 3],
        tree_override: (o == oid(0x30)).then(|| oid(9)),
        raw_commit: None,
    }
}

fn exercise(db: &MetaDb) {
    // Changesets and the path index, in one batch, twice (idempotent).
    let rows: Vec<ChangesetRow> = [0x10, 0x11, 0x30, 0xab]
        .into_iter()
        .map(|i| row(oid(i)))
        .collect();
    for _ in 0..2 {
        let mut ops: Vec<WriteOp> = rows.iter().map(WriteOp::Changeset).collect();
        let ids: Vec<Oid> = rows.iter().map(|r| r.oid).collect();
        for (g, id) in ids.iter().enumerate() {
            ops.push(WriteOp::PathIndex {
                path: "a/b",
                generation: g as u32,
                oid: id,
            });
        }
        db.write_batch(&ops).unwrap();
    }
    assert_eq!(db.changeset_count().unwrap(), 4);
    let r = db.changeset(&oid(0x30)).unwrap().unwrap();
    assert_eq!(
        (r.tree_override, r.committer, r.changes),
        (Some(oid(9)), None, vec![1, 2, 3])
    );
    assert!(db.changeset(&oid(0x99)).unwrap().is_none());
    assert_eq!(db.override_counts().unwrap(), (1, 0));
    assert_eq!(db.overridden_changesets(10).unwrap(), vec![oid(0x30)]);
    assert_eq!(db.parents(&oid(0x10)).unwrap(), Some(vec![oid(1)]));
    assert!(db.has_changeset(&oid(0xab)).unwrap());
    assert_eq!(
        db.resolve_prefix("1", 10).unwrap(),
        vec![oid(0x10), oid(0x11)]
    );
    assert_eq!(db.resolve_prefix("ab", 10).unwrap(), vec![oid(0xab)]);

    // Path index: generation desc, oid asc, keyset cursor.
    let all = db.path_changes("a/b", (u32::MAX, None), 10).unwrap();
    assert_eq!(
        all.iter().map(|x| x.0).collect::<Vec<_>>(),
        vec![3, 2, 1, 0]
    );
    let page = db.path_changes("a/b", (2, Some(oid(0x30))), 10).unwrap();
    assert_eq!(page, vec![(1, oid(0x11)), (0, oid(0x10))]);

    // Refs: every compare-and-swap case.
    let (a, b) = ((oid(0x10), oid(0x10)), (oid(0x11), oid(0x11)));
    assert!(
        db.cas_ref("refs/heads/m", None, None).unwrap(),
        "absent stays absent"
    );
    assert!(db.cas_ref("refs/heads/m", None, Some(a)).unwrap(), "create");
    assert!(
        !db.cas_ref("refs/heads/m", None, Some(b)).unwrap(),
        "create over existing"
    );
    assert!(
        !db.cas_ref("refs/heads/m", None, None).unwrap(),
        "present is not absent"
    );
    assert!(
        !db.cas_ref("refs/heads/m", Some(b.0), Some(b)).unwrap(),
        "wrong expected"
    );
    assert!(
        db.cas_ref("refs/heads/m", Some(a.0), Some(b)).unwrap(),
        "move"
    );
    assert_eq!(db.get_ref("refs/heads/m").unwrap().unwrap().target, b.0);
    assert!(db.cas_ref("refs/heads/z", None, Some(a)).unwrap());
    assert_eq!(
        db.refs()
            .unwrap()
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["refs/heads/m", "refs/heads/z"]
    );
    assert!(
        !db.cas_ref("refs/heads/z", Some(b.0), None).unwrap(),
        "delete, wrong expected"
    );
    assert!(
        db.cas_ref("refs/heads/z", Some(a.0), None).unwrap(),
        "delete"
    );
    assert!(db.get_ref("refs/heads/z").unwrap().is_none());

    // Derived values: first write wins; prefix scans, incl. 0xff edges.
    db.put_derived("k", b"\x01\x02", b"first").unwrap();
    db.put_derived("k", b"\x01\x02", b"second").unwrap();
    assert_eq!(db.get_derived("k", b"\x01\x02").unwrap().unwrap(), b"first");
    for key in [&b"\x01\xff"[..], b"\x01\xff\x00", b"\x02", b"\x01"] {
        db.put_derived("k", key, b"v").unwrap();
    }
    let mut keys = db.derived_keys_with_prefix("k", b"\x01").unwrap();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            b"\x01".to_vec(),
            b"\x01\x02".to_vec(),
            b"\x01\xff".to_vec(),
            b"\x01\xff\x00".to_vec()
        ]
    );
    assert_eq!(
        db.derived_keys_with_prefix("k", b"\x01\xff").unwrap().len(),
        2
    );
    assert_eq!(db.derived_keys_with_prefix("other", b"").unwrap().len(), 0);

    // kv overwrites.
    db.set_kv("upstream", "a").unwrap();
    db.set_kv("upstream", "b").unwrap();
    assert_eq!(db.get_kv("upstream").unwrap().as_deref(), Some("b"));
    assert!(db.get_kv("nope").unwrap().is_none());

    // Keys by prefix (the exporter's state).
    db.set_kv("exported:refs/heads/a", &oid(1).to_hex())
        .unwrap();
    db.set_kv("exported:refs/heads/b", "").unwrap();
    db.set_kv("exportedX", "no").unwrap();
    assert_eq!(
        db.exported_names().unwrap(),
        vec!["refs/heads/a".to_string()]
    );

    // Append-only logs, per stream, in order.
    let a = db.append_log("audit", b"one").unwrap();
    db.append_log("other", b"x").unwrap();
    let b = db.append_log("audit", b"two").unwrap();
    assert!(b > a);
    let all = db.read_log("audit", 0, 10).unwrap();
    assert_eq!(all, vec![(a, b"one".to_vec()), (b, b"two".to_vec())]);
    assert_eq!(
        db.read_log("audit", a, 10).unwrap(),
        vec![(b, b"two".to_vec())]
    );
    assert_eq!(db.read_log("audit", 0, 1).unwrap().len(), 1);
}

#[test]
fn sqlite_backend() {
    let dir = tempfile::tempdir().unwrap();
    exercise(&MetaDb::open(dir.path().join("m.sqlite")).unwrap());
}

#[test]
fn postgres_backend() {
    let Some(pg) = TestPg::start() else { return };
    let dir = tempfile::tempdir().unwrap();
    let cfg = MetaConfig::Postgres(pg.url().to_string());
    exercise(&MetaDb::open_config(&cfg, dir.path(), "repo-one").unwrap());
    // Repositories are separate schemas, even with names that clean to the
    // same identifier.
    let other = MetaDb::open_config(&cfg, dir.path(), "repo_one").unwrap();
    assert_eq!(other.changeset_count().unwrap(), 0);
    // Reopening keeps the data (the schema version matches).
    let again = MetaDb::open_config(&cfg, dir.path(), "repo-one").unwrap();
    assert_eq!(again.changeset_count().unwrap(), 4);
}

#[test]
fn postgres_lock_excludes_across_connections() {
    let Some(pg) = TestPg::start() else { return };
    let dir = tempfile::tempdir().unwrap();
    let cfg = MetaConfig::Postgres(pg.url().to_string());
    // Two handles, as two servers would have.
    let a = Arc::new(MetaDb::open_config(&cfg, dir.path(), "r").unwrap());
    let b = Arc::new(MetaDb::open_config(&cfg, dir.path(), "r").unwrap());
    let inside = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let db = if i % 2 == 0 { a.clone() } else { b.clone() };
            let (inside, max) = (inside.clone(), max.clone());
            std::thread::spawn(move || {
                for _ in 0..5 {
                    let _g = db.lock("mirror").unwrap();
                    let n = inside.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    max.fetch_max(n, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    inside.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(max.load(std::sync::atomic::Ordering::SeqCst), 1);
    // Other lock names do not exclude each other.
    let _x = a.lock("mirror").unwrap();
    let _y = b.lock("other").unwrap();
}

#[test]
fn postgres_connection_errors_reach_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    // Nothing listens on this socket directory.
    let cfg = MetaConfig::Postgres("host=/nonexistent-wren port=1 user=wren".into());
    let err = MetaDb::open_config(&cfg, dir.path(), "r")
        .err()
        .expect("must fail");
    assert!(
        format!("{err:#}").contains("connecting to postgres"),
        "{err:#}"
    );
}
