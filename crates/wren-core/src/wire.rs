//! The object protocol, spoken identically by the server, every cache tier,
//! and (as a client) the CLI.
//!
//! There is no have/want negotiation: the client never describes what it
//! already has. It asks for specific objects by id; every answer is immutable,
//! so any tier can serve it from cache, and a failed transfer resumes by
//! simply asking again for whatever is still missing.
//!
//! Request:  `POST /v1/objects` with JSON
//! `{"oids": ["<hex>", ...], "bases": ["<hex>" | null, ...]}`.
//! `bases` is optional; when present it has one entry per oid: an object the
//! requester already holds that is likely similar (in practice the same path
//! in a tree it checked out before). It is a hint, not negotiation: the
//! answer for an oid is still that object, whatever the base.
//!
//! Response: a stream of frames, one per requested oid, in request order:
//!
//! ```text
//!   oid      20 bytes
//!   status    1 byte   0 = found, 1 = missing, 2 = delta
//!   len       4 bytes  big-endian, length of data (0 if missing)
//!   data    len bytes  status 0: git loose-object encoding (zlib)
//!                      status 2: base oid (20) | kind (1) | size (4, BE)
//!                                | zstd frame, compressed with the base's
//!                                  content as a raw dictionary
//! ```
//!
//! A delta frame is only sent for an oid whose request named a base, so
//! clients that never send bases never see one. Every tier stores whole
//! objects: a delta is decoded and verified on arrival, and each hop
//! computes its own deltas against bases both of its ends hold.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::object::{self, Kind};
use crate::oid::Oid;
use crate::store::ObjectStore;

pub const MAX_BATCH: usize = 10_000;
pub const CONTENT_TYPE: &str = "application/x-wren-objects";

#[derive(Serialize, Deserialize, Default)]
pub struct ObjectsRequest {
    pub oids: Vec<Oid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bases: Vec<Option<Oid>>,
}

impl ObjectsRequest {
    /// The base hint for the `i`-th oid.
    pub fn base(&self, i: usize) -> Option<Oid> {
        self.bases.get(i).copied().flatten()
    }
}

/// What a frame carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Payload {
    Missing,
    /// Loose encoding.
    Loose(Vec<u8>),
    /// Delta frame data (see the module docs).
    Delta(Vec<u8>),
}

/// Objects larger than this are always sent whole.
const DELTA_MAX: usize = 32 << 20;
const ZSTD_LEVEL: i32 = 3;

/// Delta frame data for `oid` against `base`, if both are in `store` and the
/// delta is clearly smaller than `whole_len` (the loose encoding's size).
pub fn delta_against(
    store: &ObjectStore,
    oid: &Oid,
    base: &Oid,
    whole_len: usize,
) -> Result<Option<Vec<u8>>> {
    if oid == base {
        return Ok(None);
    }
    let (Some((kind, data)), Some((_, base_data))) = (store.get(oid)?, store.get(base)?) else {
        return Ok(None);
    };
    if data.len() > DELTA_MAX || base_data.len() > DELTA_MAX || data.len() < 64 {
        return Ok(None);
    }
    let z = zstd::bulk::Compressor::with_dictionary(ZSTD_LEVEL, &base_data)?.compress(&data)?;
    let len = 20 + 1 + 4 + z.len();
    if len * 10 > whole_len * 9 {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(len);
    out.extend(base.0);
    out.push(kind_byte(kind));
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(z);
    Ok(Some(out))
}

fn kind_byte(k: Kind) -> u8 {
    match k {
        Kind::Commit => 1,
        Kind::Tree => 2,
        Kind::Blob => 3,
        Kind::Tag => 4,
    }
}

/// Verify a received frame against its id and store the object whole.
/// Returns whether the object was present. A delta needs its base in
/// `store`.
pub fn store_frame(store: &ObjectStore, oid: &Oid, payload: &Payload) -> Result<bool> {
    match payload {
        Payload::Missing => Ok(false),
        Payload::Loose(raw) => {
            store.put_raw_verified(oid, raw)?;
            Ok(true)
        }
        Payload::Delta(d) => {
            ensure!(d.len() >= 25, "short delta frame for {oid}");
            let base = Oid::from_bytes(&d[..20])?;
            let kind = match d[20] {
                1 => Kind::Commit,
                2 => Kind::Tree,
                3 => Kind::Blob,
                4 => Kind::Tag,
                k => bail!("bad kind {k} in delta frame for {oid}"),
            };
            let size = u32::from_be_bytes(d[21..25].try_into().unwrap()) as usize;
            ensure!(size <= DELTA_MAX, "delta frame for {oid} is too large");
            let (_, base_data) = store
                .get(&base)?
                .with_context(|| format!("delta base {base} for {oid} is not in the store"))?;
            let data = zstd::bulk::Decompressor::with_dictionary(&base_data)?
                .decompress(&d[25..], size)?;
            ensure!(
                data.len() == size,
                "delta frame for {oid} has the wrong size"
            );
            let actual = Oid::hash_object(kind.as_str(), &data);
            ensure!(
                actual == *oid,
                "object {oid} failed verification (delta result hashes to {actual})"
            );
            store.put_raw(oid, &object::encode_loose(kind, &data))?;
            Ok(true)
        }
    }
}

/// Answer one requested object from `store`: whole, as a delta against
/// `base` when that is smaller, or missing.
pub fn encode_from_store(
    out: &mut Vec<u8>,
    store: &ObjectStore,
    oid: &Oid,
    base: Option<Oid>,
) -> Result<bool> {
    let Some(raw) = store.get_raw(oid)? else {
        encode_frame(out, oid, None)?;
        return Ok(false);
    };
    if let Some(base) = base {
        if let Some(d) = delta_against(store, oid, &base, raw.len())? {
            encode_payload(out, oid, &Payload::Delta(d))?;
            return Ok(true);
        }
    }
    encode_frame(out, oid, Some(&raw))?;
    Ok(true)
}

pub fn encode_payload(out: &mut Vec<u8>, oid: &Oid, p: &Payload) -> Result<()> {
    match p {
        Payload::Missing => encode_frame(out, oid, None),
        Payload::Loose(d) => encode_frame(out, oid, Some(d)),
        Payload::Delta(d) => {
            out.extend(oid.0);
            out.push(2);
            out.extend(u32::try_from(d.len())?.to_be_bytes());
            out.extend(d);
            Ok(())
        }
    }
}

pub fn encode_frame(out: &mut Vec<u8>, oid: &Oid, data: Option<&[u8]>) -> Result<()> {
    out.extend(oid.0);
    match data {
        Some(d) => {
            let len = u32::try_from(d.len()).map_err(|_| {
                anyhow::anyhow!(
                    "object {oid} is too large for a v1 frame ({} bytes)",
                    d.len()
                )
            })?;
            out.push(0);
            out.extend(len.to_be_bytes());
            out.extend(d);
        }
        None => {
            out.push(1);
            out.extend(0u32.to_be_bytes());
        }
    }
    Ok(())
}

/// Incremental frame decoder for streamed responses.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

pub type Frame = (Oid, Payload);

impl FrameDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut i = 0;
        while self.buf.len() - i >= 25 {
            let oid = Oid::from_bytes(&self.buf[i..i + 20])?;
            let status = self.buf[i + 20];
            let len = u32::from_be_bytes(self.buf[i + 21..i + 25].try_into().unwrap()) as usize;
            if self.buf.len() - i - 25 < len {
                break;
            }
            let data = &self.buf[i + 25..i + 25 + len];
            out.push((
                oid,
                match status {
                    0 => Payload::Loose(data.to_vec()),
                    1 => Payload::Missing,
                    2 => Payload::Delta(data.to_vec()),
                    s => bail!("bad frame status {s}"),
                },
            ));
            i += 25 + len;
        }
        self.buf.drain(..i);
        Ok(out)
    }

    pub fn finish(&self) -> Result<()> {
        if !self.buf.is_empty() {
            bail!(
                "truncated object stream ({} trailing bytes)",
                self.buf.len()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_anywhere() {
        let a = Oid::hash_object("blob", b"a");
        let b = Oid::hash_object("blob", b"b");
        let mut buf = Vec::new();
        encode_frame(&mut buf, &a, Some(b"hello")).unwrap();
        encode_frame(&mut buf, &b, None).unwrap();
        for split in 0..buf.len() {
            let mut d = FrameDecoder::default();
            let mut frames = d.push(&buf[..split]).unwrap();
            frames.extend(d.push(&buf[split..]).unwrap());
            d.finish().unwrap();
            assert_eq!(
                frames,
                vec![
                    (a, Payload::Loose(b"hello".to_vec())),
                    (b, Payload::Missing)
                ]
            );
        }
    }

    #[test]
    fn delta_frames_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let server = ObjectStore::open(dir.path().join("s")).unwrap();
        let client = ObjectStore::open(dir.path().join("c")).unwrap();
        let old: String = (0..400).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 200\n", "line two hundred\n");
        let base = server.put(Kind::Blob, old.as_bytes()).unwrap();
        let oid = server.put(Kind::Blob, new.as_bytes()).unwrap();
        client.put(Kind::Blob, old.as_bytes()).unwrap();

        let mut buf = Vec::new();
        encode_from_store(&mut buf, &server, &oid, Some(base)).unwrap();
        let whole = server.get_raw(&oid).unwrap().unwrap().len();
        assert!(
            buf.len() < whole / 4,
            "delta {} vs whole {whole}",
            buf.len()
        );
        let mut d = FrameDecoder::default();
        let frames = d.push(&buf).unwrap();
        assert!(matches!(frames[0].1, Payload::Delta(_)));
        assert!(store_frame(&client, &oid, &frames[0].1).unwrap());
        assert_eq!(client.read_blob(&oid).unwrap(), new.as_bytes());

        // A frame that decodes to other content is rejected, not stored.
        let other = Oid::hash_object("blob", b"something else");
        assert!(store_frame(&client, &other, &frames[0].1).is_err());
        assert!(!client.has(&other));
        // Without the base, the delta cannot be applied.
        let empty = ObjectStore::open(dir.path().join("e")).unwrap();
        assert!(store_frame(&empty, &oid, &frames[0].1).is_err());
        // Without a base hint, the object goes whole.
        let mut buf = Vec::new();
        encode_from_store(&mut buf, &server, &oid, None).unwrap();
        let frames = FrameDecoder::default().push(&buf).unwrap();
        assert!(matches!(frames[0].1, Payload::Loose(_)));
    }
}
