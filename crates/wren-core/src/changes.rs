//! Changesets: the list of paths a commit changed relative to its first
//! parent, with content-addressed pointers to old and new contents. This is
//! Wren's unit of history; everything else is derived from it.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::object::{TreeEntry, MODE_TREE};
use crate::oid::Oid;
use crate::store::ObjectStore;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    #[serde(rename = "A")]
    Added,
    #[serde(rename = "M")]
    Modified,
    #[serde(rename = "D")]
    Deleted,
    /// File type changed (e.g. regular file <-> symlink).
    #[serde(rename = "T")]
    TypeChanged,
}

impl Status {
    pub fn letter(&self) -> char {
        match self {
            Status::Added => 'A',
            Status::Modified => 'M',
            Status::Deleted => 'D',
            Status::TypeChanged => 'T',
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<FileVersion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<FileVersion>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileVersion {
    /// Serialized as git writes it: an octal string such as "100644".
    #[serde(with = "octal_mode")]
    pub mode: u32,
    pub oid: Oid,
}

mod octal_mode {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(mode: &u32, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{mode:06o}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
        let s = String::deserialize(d)?;
        u32::from_str_radix(&s, 8).map_err(serde::de::Error::custom)
    }
}

/// Diff two trees (either may be absent), recursing only into subtrees whose
/// ids differ, so the cost is proportional to the size of the change.
pub fn diff_trees(store: &ObjectStore, old: Option<Oid>, new: Option<Oid>) -> Result<Vec<Change>> {
    let mut out = Vec::new();
    diff_rec(store, old, new, "", &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn diff_rec(
    store: &ObjectStore,
    old: Option<Oid>,
    new: Option<Oid>,
    prefix: &str,
    out: &mut Vec<Change>,
) -> Result<()> {
    if old == new {
        return Ok(());
    }
    let load = |o: Option<Oid>| -> Result<Vec<TreeEntry>> {
        match o {
            Some(o) => store.read_tree(&o),
            None => Ok(Vec::new()),
        }
    };
    let (old_entries, new_entries) = (load(old)?, load(new)?);
    let mut by_name: BTreeMap<&[u8], (Option<&TreeEntry>, Option<&TreeEntry>)> = BTreeMap::new();
    for e in &old_entries {
        by_name.entry(&e.name).or_default().0 = Some(e);
    }
    for e in &new_entries {
        by_name.entry(&e.name).or_default().1 = Some(e);
    }
    for (name, (o, n)) in by_name {
        let path = join(prefix, &String::from_utf8_lossy(name));
        let ver = |e: &TreeEntry| FileVersion {
            mode: e.mode,
            oid: e.oid,
        };
        match (o, n) {
            (Some(o), Some(n)) if o.oid == n.oid && o.mode == n.mode => {}
            (Some(o), Some(n)) => match (o.is_tree(), n.is_tree()) {
                (true, true) => diff_rec(store, Some(o.oid), Some(n.oid), &path, out)?,
                (true, false) => {
                    diff_rec(store, Some(o.oid), None, &path, out)?;
                    out.push(Change {
                        path,
                        status: Status::Added,
                        old: None,
                        new: Some(ver(n)),
                    });
                }
                (false, true) => {
                    out.push(Change {
                        path: path.clone(),
                        status: Status::Deleted,
                        old: Some(ver(o)),
                        new: None,
                    });
                    diff_rec(store, None, Some(n.oid), &path, out)?;
                }
                (false, false) => {
                    let status = if o.mode & 0o170000 == n.mode & 0o170000 {
                        Status::Modified
                    } else {
                        Status::TypeChanged
                    };
                    out.push(Change {
                        path,
                        status,
                        old: Some(ver(o)),
                        new: Some(ver(n)),
                    });
                }
            },
            (Some(o), None) if o.is_tree() => diff_rec(store, Some(o.oid), None, &path, out)?,
            (Some(o), None) => out.push(Change {
                path,
                status: Status::Deleted,
                old: Some(ver(o)),
                new: None,
            }),
            (None, Some(n)) if n.is_tree() => diff_rec(store, None, Some(n.oid), &path, out)?,
            (None, Some(n)) => out.push(Change {
                path,
                status: Status::Added,
                old: None,
                new: Some(ver(n)),
            }),
            (None, None) => unreachable!(),
        }
    }
    Ok(())
}

pub fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// Normalise a user-supplied repository path ("/a/b/" -> "a/b").
pub fn normalize_path(p: &str) -> String {
    p.split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Look up the entry at `path` inside `tree` ("" is the tree itself).
pub fn entry_at(store: &ObjectStore, tree: Oid, path: &str) -> Result<Option<TreeEntry>> {
    let path = normalize_path(path);
    if path.is_empty() {
        return Ok(Some(TreeEntry {
            mode: MODE_TREE,
            name: Vec::new(),
            oid: tree,
        }));
    }
    let mut cur = tree;
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        let entries = store.read_tree(&cur)?;
        let Some(e) = entries.into_iter().find(|e| e.name == part.as_bytes()) else {
            return Ok(None);
        };
        if i + 1 == parts.len() {
            return Ok(Some(e));
        }
        if !e.is_tree() {
            return Ok(None);
        }
        cur = e.oid;
    }
    unreachable!()
}

// ---- compact binary encoding for the changeset log -------------------------

pub fn encode_changes(changes: &[Change]) -> Vec<u8> {
    let mut b = Vec::with_capacity(changes.len() * 64);
    b.extend((changes.len() as u32).to_le_bytes());
    for c in changes {
        b.push(c.status.letter() as u8);
        b.extend((c.path.len() as u32).to_le_bytes());
        b.extend(c.path.as_bytes());
        for v in [c.old, c.new] {
            match v {
                Some(v) => {
                    b.extend(v.mode.to_le_bytes());
                    b.extend(v.oid.0);
                }
                None => b.extend([0u8; 24]),
            }
        }
    }
    b
}

pub fn decode_changes(b: &[u8]) -> Result<Vec<Change>> {
    let mut r = Reader { b, i: 0 };
    let n = r.u32()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let status = match r.take(1)?[0] {
            b'A' => Status::Added,
            b'M' => Status::Modified,
            b'D' => Status::Deleted,
            b'T' => Status::TypeChanged,
            s => bail!("bad change status {s}"),
        };
        let len = r.u32()? as usize;
        let path = String::from_utf8(r.take(len)?.to_vec())?;
        let mut vers = [None, None];
        for v in &mut vers {
            let mode = r.u32()?;
            let oid = Oid::from_bytes(r.take(20)?)?;
            if mode != 0 {
                *v = Some(FileVersion { mode, oid });
            }
        }
        out.push(Change {
            path,
            status,
            old: vers[0],
            new: vers[1],
        });
    }
    Ok(out)
}

/// The record for exactly `path` in encoded changes, without decoding the
/// others. A changeset has at most one record per path.
pub fn find_change(b: &[u8], path: &str) -> Result<Option<Change>> {
    let mut r = Reader { b, i: 0 };
    let n = r.u32()? as usize;
    for _ in 0..n {
        let status = r.take(1)?[0];
        let len = r.u32()? as usize;
        let p = r.take(len)?;
        let vers = r.take(48)?;
        if p != path.as_bytes() {
            continue;
        }
        let mut one = Vec::with_capacity(4 + 1 + 4 + len + 48);
        one.extend(1u32.to_le_bytes());
        one.push(status);
        one.extend((len as u32).to_le_bytes());
        one.extend(p);
        one.extend(vers);
        return Ok(decode_changes(&one)?.pop());
    }
    Ok(None)
}

pub(crate) struct Reader<'a> {
    pub b: &'a [u8],
    pub i: usize,
}

impl<'a> Reader<'a> {
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.i + n > self.b.len() {
            bail!("truncated record");
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn oid(&mut self) -> Result<Oid> {
        Oid::from_bytes(self.take(20)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_roundtrip() {
        let o = Oid::hash_object("blob", b"x");
        let cs = vec![
            Change {
                path: "a/b".into(),
                status: Status::Added,
                old: None,
                new: Some(FileVersion {
                    mode: 0o100644,
                    oid: o,
                }),
            },
            Change {
                path: "c".into(),
                status: Status::Modified,
                old: Some(FileVersion {
                    mode: 0o100644,
                    oid: o,
                }),
                new: Some(FileVersion {
                    mode: 0o100755,
                    oid: o,
                }),
            },
        ];
        let enc = encode_changes(&cs);
        assert_eq!(decode_changes(&enc).unwrap(), cs);
        assert_eq!(find_change(&enc, "a/b").unwrap().as_ref(), Some(&cs[0]));
        assert_eq!(find_change(&enc, "c").unwrap().as_ref(), Some(&cs[1]));
        assert_eq!(find_change(&enc, "a").unwrap(), None);
        assert!(find_change(&enc[..enc.len() - 1], "c").is_err());
    }

    #[test]
    fn normalize() {
        assert_eq!(normalize_path("/a//b/./c/"), "a/b/c");
        assert_eq!(normalize_path("/"), "");
    }
}
