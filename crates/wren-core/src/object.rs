//! Git object model: parsing commits, trees and tags, and the zlib "loose"
//! encoding that Wren uses as its storage *and* wire format.

use std::io::{Read, Write};

use anyhow::{anyhow, bail, Context, Result};
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use serde::{Deserialize, Serialize};

use crate::oid::Oid;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
            Kind::Tag => "tag",
        }
    }

    pub fn parse(s: &[u8]) -> Result<Kind> {
        Ok(match s {
            b"commit" => Kind::Commit,
            b"tree" => Kind::Tree,
            b"blob" => Kind::Blob,
            b"tag" => Kind::Tag,
            _ => bail!("unknown object kind {:?}", String::from_utf8_lossy(s)),
        })
    }
}

pub const MODE_TREE: u32 = 0o040000;
pub const MODE_BLOB: u32 = 0o100644;
pub const MODE_EXEC: u32 = 0o100755;
pub const MODE_SYMLINK: u32 = 0o120000;
pub const MODE_GITLINK: u32 = 0o160000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub name: String,
    pub email: String,
    /// Seconds since the epoch.
    pub time: i64,
    /// Timezone offset as written by git, e.g. "+0200".
    pub tz: String,
}

impl Signature {
    fn parse(s: &[u8]) -> Result<Signature> {
        let s = String::from_utf8_lossy(s);
        let lt = s.find('<').ok_or_else(|| anyhow!("bad signature {s:?}"))?;
        let gt = s.rfind('>').ok_or_else(|| anyhow!("bad signature {s:?}"))?;
        let name = s[..lt].trim_end().to_string();
        let email = s[lt + 1..gt].to_string();
        let mut rest = s[gt + 1..].split_whitespace();
        let time = rest.next().unwrap_or("0").parse().unwrap_or(0);
        let tz = rest.next().unwrap_or("+0000").to_string();
        Ok(Signature {
            name,
            email,
            time,
            tz,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Signature,
    pub committer: Signature,
    pub message: String,
}

impl Commit {
    pub fn parse(data: &[u8]) -> Result<Commit> {
        let (headers, message) = match find(data, b"\n\n") {
            Some(i) => (&data[..i], &data[i + 2..]),
            None => (data, &b""[..]),
        };
        let mut tree = None;
        let mut parents = Vec::new();
        let mut author = None;
        let mut committer = None;
        for line in headers.split(|&b| b == b'\n') {
            // Continuation lines (e.g. inside gpgsig) start with a space.
            if line.first() == Some(&b' ') {
                continue;
            }
            let Some(sp) = line.iter().position(|&b| b == b' ') else {
                continue;
            };
            let (key, val) = (&line[..sp], &line[sp + 1..]);
            match key {
                b"tree" => tree = Some(Oid::from_hex(std::str::from_utf8(val)?)?),
                b"parent" => parents.push(Oid::from_hex(std::str::from_utf8(val)?)?),
                b"author" => author = Some(Signature::parse(val)?),
                b"committer" => committer = Some(Signature::parse(val)?),
                _ => {}
            }
        }
        Ok(Commit {
            tree: tree.context("commit without tree")?,
            parents,
            author: author.context("commit without author")?,
            committer: committer.context("commit without committer")?,
            message: String::from_utf8_lossy(message).into_owned(),
        })
    }

    pub fn summary(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: u32,
    /// Raw entry name; git does not require names to be UTF-8.
    pub name: Vec<u8>,
    pub oid: Oid,
}

impl TreeEntry {
    pub fn is_tree(&self) -> bool {
        self.mode == MODE_TREE
    }
    pub fn is_gitlink(&self) -> bool {
        self.mode == MODE_GITLINK
    }
    pub fn name_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.name)
    }
}

pub fn parse_tree(data: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let sp = data[i..]
            .iter()
            .position(|&b| b == b' ')
            .context("bad tree entry")?
            + i;
        let mode = u32::from_str_radix(std::str::from_utf8(&data[i..sp])?, 8)?;
        let nul = data[sp..]
            .iter()
            .position(|&b| b == 0)
            .context("bad tree entry")?
            + sp;
        let name = data[sp + 1..nul].to_vec();
        let oid = Oid::from_bytes(data.get(nul + 1..nul + 21).context("truncated tree")?)?;
        // Normalise legacy modes (e.g. 100664) the way git does.
        let mode = match mode {
            MODE_TREE | MODE_SYMLINK | MODE_GITLINK | MODE_EXEC => mode,
            m if m & 0o170000 == 0o040000 => MODE_TREE,
            m if m & 0o111 != 0 => MODE_EXEC,
            _ => MODE_BLOB,
        };
        out.push(TreeEntry { mode, name, oid });
        i = nul + 21;
    }
    Ok(out)
}

/// Annotated tag: only the fields needed to peel it.
pub struct Tag {
    pub object: Oid,
    pub kind: Kind,
}

impl Tag {
    pub fn parse(data: &[u8]) -> Result<Tag> {
        let mut object = None;
        let mut kind = None;
        for line in data.split(|&b| b == b'\n') {
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix(b"object ") {
                object = Some(Oid::from_hex(std::str::from_utf8(v)?)?);
            } else if let Some(v) = line.strip_prefix(b"type ") {
                kind = Some(Kind::parse(v)?);
            }
        }
        Ok(Tag {
            object: object.context("tag without object")?,
            kind: kind.context("tag without type")?,
        })
    }
}

/// Encode an object in git's loose-object format: zlib("<kind> <len>\0<data>").
/// This exact byte string is what Wren stores, ships over the wire, and drops
/// into `.git/objects`, so no tier ever has to re-encode an object.
pub fn encode_loose(kind: Kind, data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::with_capacity(data.len() / 2 + 64), Compression::fast());
    enc.write_all(format!("{} {}\0", kind.as_str(), data.len()).as_bytes())
        .unwrap();
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

pub fn decode_loose(raw: &[u8]) -> Result<(Kind, Vec<u8>)> {
    let mut buf = Vec::new();
    ZlibDecoder::new(raw)
        .read_to_end(&mut buf)
        .context("corrupt loose object")?;
    let nul = buf
        .iter()
        .position(|&b| b == 0)
        .context("loose object without header")?;
    let header = &buf[..nul];
    let sp = header
        .iter()
        .position(|&b| b == b' ')
        .context("bad loose header")?;
    let kind = Kind::parse(&header[..sp])?;
    let len: usize = std::str::from_utf8(&header[sp + 1..])?.parse()?;
    let data = buf[nul + 1..].to_vec();
    if data.len() != len {
        bail!(
            "loose object length mismatch: header {len}, actual {}",
            data.len()
        );
    }
    Ok((kind, data))
}

/// Decode and check that the content hashes to `oid`.
pub fn decode_loose_verified(oid: &Oid, raw: &[u8]) -> Result<(Kind, Vec<u8>)> {
    let (kind, data) = decode_loose(raw)?;
    let actual = Oid::hash_object(kind.as_str(), &data);
    if actual != *oid {
        bail!("object {oid} failed verification (content hashes to {actual})");
    }
    Ok((kind, data))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loose_roundtrip() {
        let raw = encode_loose(Kind::Blob, b"hello\n");
        let oid = Oid::hash_object("blob", b"hello\n");
        assert_eq!(oid.to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
        let (k, d) = decode_loose_verified(&oid, &raw).unwrap();
        assert_eq!(k, Kind::Blob);
        assert_eq!(d, b"hello\n");
    }

    #[test]
    fn parse_commit_with_gpgsig() {
        let c = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
parent ce013625030ba8dba906f756967f9e9ca394464a\n\
author A U Thor <a@example.com> 1700000000 +0100\n\
committer C O Mitter <c@example.com> 1700000001 -0500\n\
gpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\
\n\
Subject line\n\nBody\n";
        let c = Commit::parse(c).unwrap();
        assert_eq!(c.parents.len(), 1);
        assert_eq!(c.author.name, "A U Thor");
        assert_eq!(c.committer.time, 1700000001);
        assert_eq!(c.summary(), "Subject line");
    }
}
