//! The object protocol, spoken identically by the server, every cache tier,
//! and (as a client) the CLI.
//!
//! There is no have/want negotiation: the client never describes what it
//! already has. It asks for specific objects by id; every answer is immutable,
//! so any tier can serve it from cache, and a failed transfer resumes by
//! simply asking again for whatever is still missing.
//!
//! Request:  `POST /v1/objects` with JSON `{"oids": ["<hex>", ...]}`
//! Response: a stream of frames, one per requested oid, in request order:
//!
//! ```text
//!   oid      20 bytes
//!   status    1 byte   0 = found, 1 = missing
//!   len       4 bytes  big-endian, length of data (0 if missing)
//!   data    len bytes  git loose-object encoding (zlib)
//! ```

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::oid::Oid;

pub const MAX_BATCH: usize = 10_000;
pub const CONTENT_TYPE: &str = "application/x-wren-objects";

#[derive(Serialize, Deserialize)]
pub struct ObjectsRequest {
    pub oids: Vec<Oid>,
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

pub type Frame = (Oid, Option<Vec<u8>>);

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
                    0 => Some(data.to_vec()),
                    1 => None,
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
            assert_eq!(frames, vec![(a, Some(b"hello".to_vec())), (b, None)]);
        }
    }
}
