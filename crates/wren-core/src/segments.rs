//! Segments in an object store (S3, GCS, Azure, or a local directory).
//!
//! Servers that share a repository share its segments through a bucket
//! instead of a shared filesystem. A segment is two keys, `pack-<hash>.pack`
//! and `pack-<hash>.idx`; the `.idx` is uploaded last, so a listing shows a
//! segment only once it is complete. Keys are never overwritten or deleted:
//! segments are immutable, which is what makes ranged reads and local
//! caching safe without any coordination.
//!
//! The rest of Wren is synchronous; requests run on a small runtime of this
//! module's own, and callers block on a channel (which works from any
//! thread, including a server's async workers).

use std::ops::Range;
use std::path::Path as FsPath;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures::{StreamExt, TryStreamExt};
use object_store::path::Path;
use object_store::{ObjectStore as Bucket, ObjectStoreExt};

pub struct Remote {
    bucket: Arc<dyn Bucket>,
    prefix: Path,
    url: String,
    rt: Option<tokio::runtime::Runtime>,
}

impl Drop for Remote {
    fn drop(&mut self) {
        // Dropping a runtime blocks, which panics inside an async context.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// A complete segment in the bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSegment {
    /// `pack-<hash>`
    pub name: String,
    /// Size of the `.pack`.
    pub size: u64,
}

impl Remote {
    /// Open the bucket at `url`: `s3://bucket/prefix`, `gs://…`, `az://…`,
    /// `file:///dir` or `memory:///`. Credentials and options come from
    /// the usual environment variables (`AWS_*`, `GOOGLE_*`, `AZURE_*`).
    pub fn open(url: &str) -> Result<Remote> {
        let parsed = url::Url::parse(url).with_context(|| format!("bad segment URL {url:?}"))?;
        if parsed.scheme() == "file" {
            // The local backend wants the directory to exist.
            std::fs::create_dir_all(parsed.path())
                .with_context(|| format!("creating {}", parsed.path()))?;
        }
        let options = std::env::vars().filter_map(|(k, v)| {
            let lower = k.to_ascii_lowercase();
            ["aws_", "google_", "azure_"]
                .iter()
                .any(|p| lower.starts_with(p))
                .then_some((lower, v))
        });
        let (bucket, prefix) = object_store::parse_url_opts(&parsed, options)
            .with_context(|| format!("opening {url}"))?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("wren-segments")
            .enable_all()
            .build()?;
        Ok(Remote {
            bucket: Arc::from(bucket),
            prefix,
            url: url.to_string(),
            rt: Some(rt),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn key(&self, file: &str) -> Path {
        self.prefix.clone().join(file)
    }

    /// Run `fut` on this module's runtime and wait for it.
    fn run<T: Send + 'static>(
        &self,
        fut: impl std::future::Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.rt.as_ref().unwrap().spawn(async move {
            let _ = tx.send(fut.await);
        });
        rx.recv()
            .map_err(|_| anyhow!("segment request was cancelled"))?
    }

    /// Every complete segment (both keys present).
    pub fn list(&self) -> Result<Vec<RemoteSegment>> {
        let bucket = self.bucket.clone();
        let prefix = self.prefix.clone();
        let metas: Vec<object_store::ObjectMeta> =
            self.run(async move { Ok(bucket.list(Some(&prefix)).try_collect::<Vec<_>>().await?) })?;
        let mut packs = std::collections::HashMap::new();
        let mut idx = std::collections::HashSet::new();
        for m in metas {
            let Some(file) = m.location.filename() else {
                continue;
            };
            if let Some(name) = file.strip_suffix(".pack") {
                packs.insert(name.to_string(), m.size);
            } else if let Some(name) = file.strip_suffix(".idx") {
                idx.insert(name.to_string());
            }
        }
        let mut out: Vec<RemoteSegment> = packs
            .into_iter()
            .filter(|(n, _)| idx.contains(n) && n.starts_with("pack-"))
            .map(|(name, size)| RemoteSegment { name, size })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Bytes `range` of `file`.
    pub fn get_range(&self, file: &str, range: Range<u64>) -> Result<Vec<u8>> {
        self.get_ranges(file, vec![range]).map(|mut v| v.remove(0))
    }

    /// Several ranges of `file`, fetched concurrently.
    pub fn get_ranges(&self, file: &str, ranges: Vec<Range<u64>>) -> Result<Vec<Vec<u8>>> {
        let bucket = self.bucket.clone();
        let key = self.key(file);
        let file = file.to_string();
        self.run(async move {
            futures::stream::iter(ranges)
                .map(|r| {
                    let (bucket, key) = (bucket.clone(), key.clone());
                    async move { bucket.get_range(&key, r).await.map(|b| b.to_vec()) }
                })
                .buffered(8)
                .try_collect::<Vec<_>>()
                .await
                .with_context(|| format!("reading {file}"))
        })
    }

    /// The whole of `file`.
    pub fn get(&self, file: &str) -> Result<Vec<u8>> {
        let bucket = self.bucket.clone();
        let key = self.key(file);
        let file = file.to_string();
        self.run(async move {
            let r = bucket.get(&key).await?;
            Ok(r.bytes()
                .await
                .with_context(|| format!("reading {file}"))?
                .to_vec())
        })
    }

    /// Upload a local segment (`pack-*.pack` and its `.idx`, in that order).
    /// Uploading a segment that is already there rewrites identical bytes.
    pub fn put_segment(&self, pack: &FsPath) -> Result<()> {
        let name = pack
            .file_stem()
            .and_then(|s| s.to_str())
            .context("segment without a name")?
            .to_string();
        for ext in ["pack", "idx"] {
            let local = pack.with_extension(ext);
            let key = self.key(&format!("{name}.{ext}"));
            let bucket = self.bucket.clone();
            self.run(async move {
                use tokio::io::AsyncWriteExt;
                let mut f = tokio::fs::File::open(&local)
                    .await
                    .with_context(|| format!("{}", local.display()))?;
                let mut w = object_store::buffered::BufWriter::new(bucket, key);
                tokio::io::copy(&mut f, &mut w).await?;
                w.shutdown().await?;
                Ok(())
            })
            .with_context(|| format!("uploading {name}.{ext} to {}", self.url))?;
        }
        Ok(())
    }
}
