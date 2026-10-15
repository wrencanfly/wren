//! Object stores that share segments through a bucket (here `file://`, the
//! same code path as S3 or GCS): no shared filesystem between them.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use wren_core::object::Kind;
use wren_core::segments::Remote;
use wren_core::{ObjectStore, Oid};

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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn bucket(dir: &Path) -> Arc<Remote> {
    Arc::new(Remote::open(&format!("file://{}", dir.display())).unwrap())
}

/// Bytes of a remote segment's sparse cache file that are really there.
fn cached_blocks(root: &Path) -> u32 {
    std::fs::read_dir(root.join("remote"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "have"))
        .map(|e| {
            std::fs::read(e.path())
                .unwrap()
                .iter()
                .map(|b| b.count_ones())
                .sum::<u32>()
        })
        .sum()
}

#[test]
fn loose_objects_are_sealed_and_shared() {
    let tmp = tempfile::tempdir().unwrap();
    let b = tmp.path().join("bucket");
    let a = ObjectStore::open_with(tmp.path().join("a"), Some(bucket(&b))).unwrap();
    let c = ObjectStore::open_with(tmp.path().join("c"), Some(bucket(&b))).unwrap();

    let oids: Vec<Oid> = (0..50)
        .map(|i| a.put(Kind::Blob, format!("blob {i}\n").as_bytes()).unwrap())
        .collect();
    assert!(!c.has(&oids[0]), "not published yet");
    assert_eq!(a.publish().unwrap(), 50);
    assert_eq!(a.publish().unwrap(), 0, "nothing new");
    // a keeps no loose copies; both read the segment.
    assert!(!a.path(&oids[0]).exists());
    for (i, o) in oids.iter().enumerate() {
        let want = format!("blob {i}\n");
        assert_eq!(a.read_blob(o).unwrap(), want.as_bytes());
        // c lists the bucket on its first miss.
        assert_eq!(c.read_blob(o).unwrap(), want.as_bytes());
    }
    assert_eq!(c.packs().counts(), (1, 0));

    // The sealed segment is a valid git pack.
    let pack = std::fs::read_dir(tmp.path().join("a/pack"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .unwrap();
    git(tmp.path(), &["verify-pack", pack.to_str().unwrap()]);
}

#[test]
fn ranged_reads_then_hydrate() {
    let tmp = tempfile::tempdir().unwrap();
    // A git repository whose pack spans several blocks, with deltas.
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    let mut seed = 1u64;
    let mut noise = |n: usize| -> String {
        (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                char::from(b'a' + (seed >> 59) as u8 % 26)
            })
            .collect()
    };
    let mut big = String::new();
    for i in 0..40 {
        big.push_str(&noise(2000));
        big.push('\n');
        std::fs::write(repo.join("grow.txt"), &big).unwrap();
        std::fs::write(repo.join(format!("r{i}.bin")), noise(100_000)).unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(&repo, &["repack", "-adq"]);
    let pack_dir = repo.join(".git/objects/pack");
    let pack = std::fs::read_dir(&pack_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .unwrap();
    let size = std::fs::metadata(&pack).unwrap().len();
    assert!(size > 2 << 20, "{size}");

    let b = tmp.path().join("bucket");
    let a = ObjectStore::open_with(tmp.path().join("a"), Some(bucket(&b))).unwrap();
    // add_pack moves the pack; give it a copy.
    let copy = tmp.path().join("in");
    std::fs::create_dir_all(&copy).unwrap();
    for ext in ["pack", "idx"] {
        std::fs::copy(
            pack.with_extension(ext),
            copy.join(pack.with_extension(ext).file_name().unwrap()),
        )
        .unwrap();
    }
    a.add_pack(&copy.join(pack.file_name().unwrap())).unwrap();

    let c_root = tmp.path().join("c");
    let c = ObjectStore::open_with(&c_root, Some(bucket(&b))).unwrap();
    assert_eq!(c.packs().counts(), (1, 0));
    // One small file: only the blocks holding it (and its delta chain).
    let head_tree = git(&repo, &["rev-parse", "HEAD:grow.txt"]);
    let oid: Oid = head_tree.trim().parse().unwrap();
    assert_eq!(c.read_blob(&oid).unwrap(), big.as_bytes());
    let blocks = cached_blocks(&c_root);
    let total = size.div_ceil(1 << 20) as u32;
    assert!(blocks < total, "read {blocks} of {total} blocks");

    // Every object reads back with its own id.
    let all = git(
        &repo,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    for line in all.lines() {
        let oid: Oid = line.parse().unwrap();
        let (kind, data) = c.get(&oid).unwrap().expect("in the bucket");
        assert_eq!(Oid::hash_object(kind.as_str(), &data), oid);
    }

    // Hydrate: the segment becomes a complete local file git can use.
    assert_eq!(c.hydrate().unwrap(), 1);
    assert_eq!(c.packs().counts(), (0, 1));
    assert_eq!(c.hydrate().unwrap(), 0);
    let local_idx = c_root
        .join("pack")
        .join(pack.with_extension("idx").file_name().unwrap());
    git(tmp.path(), &["verify-pack", local_idx.to_str().unwrap()]);
    assert_eq!(c.read_blob(&oid).unwrap(), big.as_bytes());

    // A fresh store sharing the bucket sees one segment, not two.
    let d = ObjectStore::open_with(tmp.path().join("d"), Some(bucket(&b))).unwrap();
    assert_eq!(d.packs().counts(), (1, 0));
}

#[test]
fn corrupt_bucket_is_detected_on_hydrate() {
    let tmp = tempfile::tempdir().unwrap();
    let b = tmp.path().join("bucket");
    let a = ObjectStore::open_with(tmp.path().join("a"), Some(bucket(&b))).unwrap();
    for i in 0..20 {
        a.put(Kind::Blob, format!("x{i}").as_bytes()).unwrap();
    }
    a.publish().unwrap();
    let pack = std::fs::read_dir(&b)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .unwrap();
    let mut bytes = std::fs::read(&pack).unwrap();
    let n = bytes.len();
    bytes[n - 30] ^= 0xff;
    std::fs::write(&pack, bytes).unwrap();
    let c = ObjectStore::open_with(tmp.path().join("c"), Some(bucket(&b))).unwrap();
    let err = c.hydrate().unwrap_err();
    assert!(format!("{err:#}").contains("checksum"), "{err:#}");
}
