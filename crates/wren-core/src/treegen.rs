//! Trees as derived data.
//!
//! A changeset does not point to a tree; it records the files that changed
//! relative to its first parent. The tree is derived from it:
//!
//! ```text
//! git_tree(cs) = apply(git_tree(p1), cs.changes)
//! ```
//!
//! `apply` copies only the path from the root to each changed entry and
//! reuses every untouched subtree by id, so its cost is (changed paths x
//! depth), independent of the size of the repository. Trees are serialized
//! exactly as git writes them, so the derived id equals the id git computes.
//!
//! Commits ingested from git are checked: when the derived tree differs from
//! the commit's own tree (git accepts non-canonical trees, e.g. legacy modes
//! such as `100664`), the changeset records the git tree as an override and
//! descendants derive from it.

use std::collections::BTreeMap;

use anyhow::{bail, Result};

use crate::changes::{self, Change};
use crate::derived::Derivable;
use crate::meta::ChangesetRow;
use crate::object::{encode_tree, Kind, TreeEntry, MODE_TREE};
use crate::oid::Oid;
use crate::repo::Repo;
use crate::store::ObjectStore;

/// One edit: the entry's new (mode, id), or `None` to delete it.
type Edit<'a> = (&'a [u8], Option<(u32, Oid)>);

/// The tree that results from applying `changes` (relative to `base`) to
/// `base`. New trees are written to `store`, children before parents.
pub fn apply(store: &ObjectStore, base: Option<Oid>, changes: &[Change]) -> Result<Oid> {
    let edits: Vec<Edit> = changes
        .iter()
        .map(|c| (c.path.as_bytes(), c.new.map(|v| (v.mode, v.oid))))
        .collect();
    match apply_dir(store, base, &edits, "")? {
        Some(oid) => Ok(oid),
        None => store.put(Kind::Tree, b""),
    }
}

/// Apply `edits` (paths relative to this directory) to the tree `base`.
/// Returns `None` when the directory ends up empty: git has no empty
/// subtrees.
fn apply_dir(
    store: &ObjectStore,
    base: Option<Oid>,
    edits: &[Edit],
    prefix: &str,
) -> Result<Option<Oid>> {
    if edits.is_empty() {
        return Ok(base);
    }
    let mut entries: BTreeMap<Vec<u8>, TreeEntry> = match base {
        Some(b) => store
            .read_tree(&b)?
            .into_iter()
            .map(|e| (e.name.clone(), e))
            .collect(),
        None => BTreeMap::new(),
    };
    // Group the edits by their first path component.
    type Group<'a> = (Option<Option<(u32, Oid)>>, Vec<Edit<'a>>);
    let mut groups: BTreeMap<&[u8], Group> = BTreeMap::new();
    for &(path, new) in edits {
        match path.iter().position(|&b| b == b'/') {
            None => groups.entry(path).or_default().0 = Some(new),
            Some(i) => groups
                .entry(&path[..i])
                .or_default()
                .1
                .push((&path[i + 1..], new)),
        }
    }
    for (name, (direct, nested)) in groups {
        let here = changes::join(prefix, &String::from_utf8_lossy(name));
        let existing = entries.remove(name);
        let mut result = existing.clone();
        if !nested.is_empty() {
            let sub_base = existing.as_ref().filter(|e| e.is_tree()).map(|e| e.oid);
            if existing.as_ref().is_some_and(|e| !e.is_tree()) && direct.is_none() {
                bail!("{here} is a file, but the changeset edits paths below it");
            }
            result = apply_dir(store, sub_base, &nested, &here)?.map(|oid| TreeEntry {
                mode: MODE_TREE,
                name: name.to_vec(),
                oid,
            });
        }
        match direct {
            Some(Some((mode, oid))) => {
                if !nested.is_empty() && result.is_some() {
                    bail!("{here} is both a file and a non-empty directory");
                }
                result = Some(TreeEntry {
                    mode,
                    name: name.to_vec(),
                    oid,
                });
            }
            Some(None) => match &existing {
                None => bail!("{here} is deleted, but it does not exist"),
                Some(e) if e.is_tree() => bail!("{here} is a directory; delete its files"),
                // The file goes; a directory built by `nested` stays.
                Some(_) if nested.is_empty() => result = None,
                Some(_) => {}
            },
            None => {}
        }
        if let Some(e) = result {
            entries.insert(name.to_vec(), e);
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }
    Ok(Some(store.put(Kind::Tree, &encode_tree(entries.values()))?))
}

/// Derived type: the root tree of a changeset.
pub struct GitTree(pub Oid);

impl Derivable for GitTree {
    const KIND: &'static str = "git_tree";

    fn derive(repo: &Repo, cs: &ChangesetRow, parents: &[std::sync::Arc<Self>]) -> Result<Self> {
        if let Some(t) = cs.tree_override {
            return Ok(GitTree(t));
        }
        let changes = changes::decode_changes(&cs.changes)?;
        let tree = apply(&repo.store, parents.first().map(|p| p.0), &changes)?;
        // The value is about to be shared; so must the trees it names.
        repo.store.publish()?;
        Ok(GitTree(tree))
    }

    fn encode(&self) -> Vec<u8> {
        self.0.as_bytes().to_vec()
    }

    fn decode(b: &[u8]) -> Result<Self> {
        Ok(GitTree(Oid::from_bytes(b)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::diff_trees;
    use crate::object::{MODE_BLOB, MODE_EXEC, MODE_SYMLINK};

    fn tree(store: &ObjectStore, files: &[(&str, u32, &str)]) -> Oid {
        // Build through `apply` from nothing, then compare with a hand-built
        // tree below.
        let changes: Vec<Change> = files
            .iter()
            .map(|(p, mode, content)| Change {
                path: p.to_string(),
                status: changes::Status::Added,
                old: None,
                new: Some(changes::FileVersion {
                    mode: *mode,
                    oid: store.put(Kind::Blob, content.as_bytes()).unwrap(),
                }),
            })
            .collect();
        apply(store, None, &changes).unwrap()
    }

    #[test]
    fn apply_reproduces_diff() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(dir.path()).unwrap();
        let cases: Vec<Vec<(&str, u32, &str)>> = vec![
            vec![],
            vec![("a", MODE_BLOB, "1")],
            vec![
                ("a", MODE_BLOB, "1"),
                ("d/x", MODE_BLOB, "x"),
                ("d/e/y", MODE_EXEC, "y"),
            ],
            // a.c < a/ < a0 ordering, and a file that becomes a directory
            vec![
                ("a.c", MODE_BLOB, "1"),
                ("a/b", MODE_BLOB, "2"),
                ("a0", MODE_SYMLINK, "t"),
            ],
            vec![("a", MODE_BLOB, "now a file"), ("d/x", MODE_BLOB, "x2")],
            vec![("d/e/y", MODE_BLOB, "y")],
            vec![("z/z/z/z", MODE_BLOB, "deep")],
        ];
        let trees: Vec<Oid> = cases.iter().map(|c| tree(&store, c)).collect();
        // For every pair, applying diff(old, new) to old must give new.
        for &old in &trees {
            for &new in &trees {
                let d = diff_trees(&store, Some(old), Some(new)).unwrap();
                assert_eq!(apply(&store, Some(old), &d).unwrap(), new);
            }
            let d = diff_trees(&store, None, Some(old)).unwrap();
            assert_eq!(apply(&store, None, &d).unwrap(), old);
        }
        // Empty tree id is git's.
        assert_eq!(
            trees[0].to_hex(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
    }
}
