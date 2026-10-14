//! HTTP client for the query API and the object protocol.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use futures::stream::{self, StreamExt, TryStreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wren_core::api::*;
use wren_core::wire::{FrameDecoder, ObjectsRequest};
use wren_core::{ObjectStore, Oid};

pub const BATCH: usize = 1000;
pub const PARALLEL: usize = 16;

#[derive(Clone)]
pub struct Client {
    pub server: String,
    pub repo: String,
    http: reqwest::Client,
    pub stats: Arc<FetchStats>,
}

#[derive(Default)]
pub struct FetchStats {
    pub objects: AtomicU64,
    pub bytes: AtomicU64,
    pub requests: AtomicU64,
}

/// Saved in `.wren/config.json` at the root of a checkout, so commands run
/// inside it know which server and repo to talk to.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CheckoutConfig {
    pub server: String,
    pub repo: String,
    pub commit: Oid,
    pub rev: String,
    pub sparse: Vec<String>,
}

pub fn find_checkout_config() -> Option<(PathBuf, CheckoutConfig)> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let p = dir.join(".wren/config.json");
        if let Ok(b) = std::fs::read(&p) {
            if let Ok(c) = serde_json::from_slice(&b) {
                return Some((dir, c));
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

impl Client {
    pub fn new(server: &str, repo: &str, token: Option<&str>) -> Client {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(t) = token {
            let mut v = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))
                .expect("token is header-safe");
            v.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, v);
        }
        Client {
            server: server.trim_end_matches('/').to_string(),
            repo: repo.to_string(),
            http: reqwest::Client::builder()
                .pool_max_idle_per_host(PARALLEL)
                .default_headers(headers)
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .unwrap(),
            stats: Arc::default(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1/repos/{}/{}", self.server, self.repo, path)
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<ErrorBody>(&body)
            .map(|e| e.error)
            .unwrap_or(body);
        bail!("{status}: {msg}")
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let resp = self
            .http
            .get(self.url(path))
            .query(query)
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.server))?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn get_bytes(&self, path: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(self.url(path))
            .query(query)
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.server))?;
        Ok(Self::check(resp).await?.bytes().await?.to_vec())
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .http
            .post(self.url(path))
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.server))?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn repos(&self) -> Result<Vec<RepoInfo>> {
        let resp = self
            .http
            .get(format!("{}/api/v1/repos", self.server))
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.server))?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn repo_info(&self) -> Result<RepoInfo> {
        self.repos()
            .await?
            .into_iter()
            .find(|r| r.name == self.repo)
            .with_context(|| format!("unknown repo {:?}", self.repo))
    }

    pub async fn resolve(&self, rev: &str) -> Result<Oid> {
        Ok(self.get::<Resolved>("resolve", &[("rev", rev)]).await?.oid)
    }

    /// Make sure every object in `oids` is in the local store, fetching the
    /// missing ones. The client only ever names objects it wants; it never
    /// tells the server what it has. Batches run in parallel and failed
    /// batches are retried, which resumes naturally: a retry asks only for
    /// what is still missing.
    pub async fn ensure(&self, store: &Arc<ObjectStore>, oids: &[Oid]) -> Result<()> {
        let mut missing: Vec<Oid> = {
            let store = store.clone();
            let oids = oids.to_vec();
            tokio::task::spawn_blocking(move || {
                let mut seen = std::collections::HashSet::new();
                oids.into_iter()
                    .filter(|o| seen.insert(*o) && !store.has(o))
                    .collect()
            })
            .await?
        };
        for attempt in 0..4 {
            if missing.is_empty() {
                return Ok(());
            }
            let results: Vec<Result<()>> = stream::iter(missing.chunks(BATCH).map(|c| c.to_vec()))
                .map(|chunk| self.fetch_batch(store.clone(), chunk))
                .buffer_unordered(PARALLEL)
                .collect()
                .await;
            let errors: Vec<_> = results.into_iter().filter_map(|r| r.err()).collect();
            let store2 = store.clone();
            missing = tokio::task::spawn_blocking(move || {
                missing.into_iter().filter(|o| !store2.has(o)).collect()
            })
            .await?;
            if !missing.is_empty() {
                if attempt == 3 || errors.is_empty() {
                    let first = errors
                        .into_iter()
                        .next()
                        .map(|e| format!(": {e:#}"))
                        .unwrap_or_default();
                    bail!(
                        "{} objects could not be fetched (e.g. {}){first}",
                        missing.len(),
                        missing[0]
                    );
                }
                tracing::warn!(
                    "retrying {} objects after error: {:#}",
                    missing.len(),
                    errors[0]
                );
            }
        }
        Ok(())
    }

    async fn fetch_batch(&self, store: Arc<ObjectStore>, oids: Vec<Oid>) -> Result<()> {
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        let resp = self
            .http
            .post(format!("{}/v1/objects", self.server))
            .json(&ObjectsRequest { oids })
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.server))?;
        let resp = Self::check(resp).await?;
        let mut body = resp.bytes_stream().map_err(anyhow::Error::from);
        let mut dec = FrameDecoder::default();
        let mut pending = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            self.stats
                .bytes
                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            pending.extend(dec.push(&chunk)?);
        }
        dec.finish()?;
        self.stats.objects.fetch_add(
            pending.iter().filter(|f| f.1.is_some()).count() as u64,
            Ordering::Relaxed,
        );
        // Verify every object against its id before it enters the store.
        tokio::task::spawn_blocking(move || -> Result<()> {
            use rayon::prelude::*;
            pending.par_iter().try_for_each(|(oid, raw)| -> Result<()> {
                if let Some(raw) = raw {
                    store.put_raw_verified(oid, raw)?;
                }
                Ok(())
            })
        })
        .await??;
        Ok(())
    }
}
