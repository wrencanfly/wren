use std::fmt;
use std::str::FromStr;

use anyhow::{bail, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha1::{Digest, Sha1};

/// A git object id (SHA-1). Wren keeps git's object identity end to end, so an
/// OID handed out by the Wren API is valid in any git client and vice versa.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Oid(pub [u8; 20]);

impl Oid {
    pub const LEN: usize = 20;
    pub const ZERO: Oid = Oid([0; 20]);

    pub fn from_bytes(b: &[u8]) -> Result<Oid> {
        if b.len() != 20 {
            bail!("oid must be 20 bytes, got {}", b.len());
        }
        let mut a = [0u8; 20];
        a.copy_from_slice(b);
        Ok(Oid(a))
    }

    pub fn from_hex(s: &str) -> Result<Oid> {
        if s.len() != 40 {
            bail!("oid must be 40 hex chars: {s:?}");
        }
        let mut a = [0u8; 20];
        hex::decode_to_slice(s, &mut a)?;
        Ok(Oid(a))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn short(&self) -> String {
        self.to_hex()[..12].to_string()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The git object id of `data` stored as an object of type `kind`.
    pub fn hash_object(kind: &str, data: &[u8]) -> Oid {
        let mut h = Sha1::new();
        h.update(kind.as_bytes());
        h.update(b" ");
        h.update(data.len().to_string().as_bytes());
        h.update([0u8]);
        h.update(data);
        let out = h.finalize();
        let mut a = [0u8; 20];
        a.copy_from_slice(out.as_slice());
        Oid(a)
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.short())
    }
}

impl FromStr for Oid {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Oid> {
        Oid::from_hex(s)
    }
}

impl Serialize for Oid {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Oid {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Oid, D::Error> {
        let s = String::deserialize(d)?;
        Oid::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_blob_and_tree_ids() {
        assert_eq!(
            Oid::hash_object("blob", b"").to_hex(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            Oid::hash_object("tree", b"").to_hex(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
    }
}
