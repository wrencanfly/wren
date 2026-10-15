//! `wren cache` and `wren daemon`: the caching tiers. They answer exactly
//! the same RPCs as the server, so clients cannot tell (and need not know)
//! where in the hierarchy they are.
//!
//! Tiers are strict: a node (`wren daemon`, one per machine) points at a
//! region cache, and a region cache at the server. Each declares its tier in
//! its capabilities and checks its upstream's at startup, refusing to start
//! when the upstream is not exactly the tier above. Otherwise a region
//! outage would send every node straight to the server, which is how a
//! cache failure becomes a server failure.
//!
//! Everything a tier keeps is immutable, so nothing is ever revalidated:
//! * objects, which are content-addressed;
//! * query responses the upstream marked `immutable`, i.e. queries whose
//!   every revision is a full commit id (see `server::is_pinned`);
//! * snapshot files, named by their content hash.
//!
//! Other query calls are forwarded. Writes are refused: they go directly to
//! the server (the control plane). All upstream traffic shares one
//! concurrency limit, concurrent misses for the same object share one
//! upstream request, and recently served objects are also kept in memory.
//! A node keeps its store under a byte budget (see `wren_core::bounded`).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Serialize;
use sha1::{Digest, Sha1};
use tokio::sync::{watch, Semaphore};
use wren_core::api::{Capabilities, PROTOCOL_VERSION, QUERY_SEMANTICS};
use wren_core::bounded::BoundedStore;
use wren_core::wire::{self, FrameDecoder, ObjectsRequest};
use wren_core::{ObjectStore, Oid};

use crate::server::{
    immutable, objects_response, protocol_header, require_token, serve_file, ApiError, FEATURES,
    SEMANTICS_HEADER, SYNCED_AT_HEADER,
};
use crate::transport::{self, HttpResponse, Transport};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// One per machine (`wren daemon`).
    Node,
    /// One per region (`wren cache`).
    Region,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Tier::Node => "node",
            Tier::Region => "region",
        }
    }

    /// The only tier this one may use as its upstream.
    pub fn parent(self) -> &'static str {
        match self {
            Tier::Node => "region",
            Tier::Region => "server",
        }
    }
}

pub struct CacheConfig {
    pub tier: Tier,
    /// Object store.
    pub store_dir: PathBuf,
    /// Stored query answers and snapshot files.
    pub dir: PathBuf,
    pub upstream: String,
    /// Token clients of this tier must present (None = open).
    pub token: Option<String>,
    /// Token this tier presents to its upstream.
    pub upstream_token: Option<String>,
    pub upstream_concurrency: usize,
    /// Bytes of recently served objects kept in memory.
    pub memory: usize,
    /// A node's store budget in bytes, and how long a checkout's lease
    /// protects its objects.
    pub budget: Option<(u64, std::time::Duration)>,
}

pub struct CacheState {
    pub tier: Tier,
    pub store: Arc<ObjectStore>,
    /// A node's budget (None for a region cache: unbounded).
    pub bounded: Option<Arc<BoundedStore>>,
    /// Where immutable query responses are kept.
    pub api_dir: PathBuf,
    /// Where snapshot files are kept.
    pub snapshot_dir: PathBuf,
    pub upstream: String,
    pub transport: Arc<dyn Transport>,
    pub token: Option<String>,
    pub upstream_token: Option<String>,
    /// Bounds concurrent upstream requests (objects and queries alike).
    pub upstream_permits: Semaphore,
    pub stats: Stats,
    /// The upstream's query semantics version, learned from its responses.
    /// Stored answers are keyed by it, so a server upgrade that changes
    /// answers never mixes old and new ones.
    pub semantics: std::sync::RwLock<String>,
    memory: Mutex<Lru>,
    /// Objects being fetched from upstream right now; others who miss on
    /// them wait for that fetch instead of asking again.
    inflight: Mutex<HashMap<Oid, watch::Receiver<bool>>>,
    snapshot_flights: tokio::sync::Mutex<()>,
    /// Queries being forwarded right now (see `proxy`).
    api_flights: Mutex<HashMap<String, watch::Receiver<bool>>>,
    /// Answers reused for a short time (the snapshot description).
    short_lived: Mutex<HashMap<String, (std::time::Instant, ShortLived)>>,
    /// Bytes stored since the node last ran maintenance.
    since_maintain: AtomicU64,
    maintaining: AtomicBool,
}

#[derive(Default)]
pub struct Stats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    /// Hits answered from memory (included in `hits`).
    pub memory_hits: AtomicU64,
    /// Misses that waited for another request's upstream fetch.
    pub coalesced: AtomicU64,
    pub api_hits: AtomicU64,
    pub api_misses: AtomicU64,
    pub upstream_requests: AtomicU64,
    pub snapshot_downloads: AtomicU64,
}

#[derive(Serialize)]
struct StatsBody {
    tier: &'static str,
    hits: u64,
    misses: u64,
    hit_rate: f64,
    memory_hits: u64,
    coalesced: u64,
    api_hits: u64,
    api_misses: u64,
    upstream_requests: u64,
    snapshot_downloads: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    store_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget: Option<u64>,
}

/// Objects one request is fetching from upstream (see `fetch_coalesced`).
struct Inflight<'a> {
    map: &'a Mutex<HashMap<Oid, watch::Receiver<bool>>>,
    oids: Vec<Oid>,
    tx: watch::Sender<bool>,
}

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        let mut map = self.map.lock().unwrap();
        for o in &self.oids {
            map.remove(o);
        }
        drop(map);
        let _ = self.tx.send(true);
    }
}

/// Raw (loose-encoded) objects, least recently used out first.
#[derive(Default)]
struct Lru {
    map: HashMap<Oid, (Arc<Vec<u8>>, u64)>,
    order: BTreeMap<u64, Oid>,
    tick: u64,
    bytes: usize,
    limit: usize,
}

impl Lru {
    fn get(&mut self, oid: &Oid) -> Option<Arc<Vec<u8>>> {
        let (v, t) = self.map.get_mut(oid)?;
        self.order.remove(t);
        self.tick += 1;
        *t = self.tick;
        self.order.insert(self.tick, *oid);
        Some(v.clone())
    }

    fn put(&mut self, oid: Oid, v: Arc<Vec<u8>>) {
        if v.len() > self.limit / 16 || self.map.contains_key(&oid) {
            return;
        }
        self.tick += 1;
        self.bytes += v.len();
        self.map.insert(oid, (v, self.tick));
        self.order.insert(self.tick, oid);
        while self.bytes > self.limit {
            let Some((_, old)) = self.order.pop_first() else {
                break;
            };
            if let Some((v, _)) = self.map.remove(&old) {
                self.bytes -= v.len();
            }
        }
    }
}

type St = State<Arc<CacheState>>;
type ShortLived = (StatusCode, Option<HeaderValue>, Bytes);

pub fn router(state: Arc<CacheState>) -> Router {
    let token = state.token.clone();
    Router::new()
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/stats", get(stats))
        .route("/v1/objects", post(objects_batch))
        .route("/v1/objects/{oid}", get(object_one))
        .route("/v1/snapshots/{file}", get(snapshot_file))
        .route("/api/{*rest}", any(proxy))
        .layer(axum::middleware::from_fn(move |req, next| {
            require_token(token.clone(), req, next)
        }))
        .route("/healthz", get(|| async { "ok" }))
        .layer(axum::middleware::map_response(protocol_header))
        .with_state(state)
}

async fn capabilities(State(st): St) -> Json<Capabilities> {
    Json(Capabilities {
        protocol: PROTOCOL_VERSION,
        object_format: "sha1".into(),
        tier: st.tier.name().into(),
        features: FEATURES.iter().map(|f| f.to_string()).collect(),
        semantics: st.current_semantics(),
        store: (st.tier == Tier::Node).then(|| st.store.root().display().to_string()),
    })
}

async fn stats(State(st): St) -> Json<StatsBody> {
    let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let (hits, misses) = (load(&st.stats.hits), load(&st.stats.misses));
    let store_bytes = match &st.bounded {
        Some(b) => {
            let b = b.clone();
            tokio::task::spawn_blocking(move || b.size()).await.ok()
        }
        None => None,
    };
    Json(StatsBody {
        tier: st.tier.name(),
        hits,
        misses,
        hit_rate: if hits + misses == 0 {
            0.0
        } else {
            hits as f64 / (hits + misses) as f64
        },
        memory_hits: load(&st.stats.memory_hits),
        coalesced: load(&st.stats.coalesced),
        api_hits: load(&st.stats.api_hits),
        api_misses: load(&st.stats.api_misses),
        upstream_requests: load(&st.stats.upstream_requests),
        snapshot_downloads: load(&st.stats.snapshot_downloads),
        store_bytes,
        budget: st.bounded.as_ref().map(|b| b.budget),
    })
}

impl CacheState {
    pub fn new(cfg: CacheConfig, transport: Arc<dyn Transport>) -> Result<CacheState> {
        let store = Arc::new(ObjectStore::open(&cfg.store_dir)?);
        let bounded = match cfg.budget {
            Some((bytes, lease)) => {
                Some(Arc::new(BoundedStore::open(store.clone(), bytes, lease)?))
            }
            None => None,
        };
        Ok(CacheState {
            tier: cfg.tier,
            store,
            bounded,
            api_dir: cfg.dir.join("api"),
            snapshot_dir: cfg.dir.join("snapshots"),
            upstream: cfg.upstream.trim_end_matches('/').to_string(),
            transport,
            token: cfg.token,
            upstream_token: cfg.upstream_token,
            upstream_permits: Semaphore::new(cfg.upstream_concurrency),
            stats: Stats::default(),
            semantics: Default::default(),
            memory: Mutex::new(Lru {
                limit: cfg.memory,
                ..Default::default()
            }),
            inflight: Mutex::new(HashMap::new()),
            snapshot_flights: tokio::sync::Mutex::new(()),
            api_flights: Mutex::new(HashMap::new()),
            short_lived: Mutex::new(HashMap::new()),
            since_maintain: AtomicU64::new(0),
            maintaining: AtomicBool::new(false),
        })
    }

    fn current_semantics(&self) -> String {
        let s = self.semantics.read().unwrap_or_else(|p| p.into_inner());
        if s.is_empty() {
            QUERY_SEMANTICS.to_string()
        } else {
            s.clone()
        }
    }

    fn learn_semantics(&self, v: &str) {
        let mut s = self.semantics.write().unwrap_or_else(|p| p.into_inner());
        if *s != v {
            *s = v.to_string();
        }
    }

    async fn upstream_call(
        &self,
        method: Method,
        path: &str,
        content_type: Option<&str>,
        body: Bytes,
        extra: &[(header::HeaderName, String)],
    ) -> Result<HttpResponse> {
        let mut req = transport::request(
            method,
            &format!("{}{}", self.upstream, path),
            self.upstream_token.as_deref(),
            content_type,
            body,
        )?;
        for (k, v) in extra {
            req.headers_mut().insert(k.clone(), v.parse()?);
        }
        self.transport.send(req).await
    }

    /// Check that the upstream is exactly the tier above this one, retrying
    /// while it is unreachable (up to `patience`). Also learns its query
    /// semantics.
    pub async fn check_upstream(
        &self,
        allow_skip: bool,
        patience: std::time::Duration,
    ) -> Result<()> {
        let start = tokio::time::Instant::now();
        let caps = loop {
            let r = self
                .upstream_call(Method::GET, "/v1/capabilities", None, Bytes::new(), &[])
                .await;
            match r {
                Ok(r) if r.status.is_success() => {
                    break serde_json::from_slice::<Capabilities>(&r.body)
                        .context("upstream capabilities")?
                }
                Ok(r) if r.status == StatusCode::UNAUTHORIZED => {
                    bail!("the upstream {} rejected this tier's token", self.upstream)
                }
                _ if start.elapsed() > patience => {
                    bail!("upstream {} is unreachable", self.upstream)
                }
                _ => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
            }
        };
        if !caps.semantics.is_empty() {
            self.learn_semantics(&caps.semantics);
        }
        // Before tiers had names, every cache said "cache".
        let theirs = match caps.tier.as_str() {
            "cache" => "region",
            t => t,
        };
        let want = self.tier.parent();
        if theirs != want {
            let msg = format!(
                "a {} must point at a {want}, but {} is a {theirs}",
                self.tier.name(),
                self.upstream
            );
            if !allow_skip {
                bail!("{msg} (refusing to start; --allow-tier-skip overrides this for small deployments)");
            }
            tracing::warn!("{msg}; continuing because of --allow-tier-skip");
        }
        Ok(())
    }

    /// Fetch `oids` from the upstream tier and store them, verified.
    /// `bases[i]` is passed on as the delta hint for `oids[i]` only when
    /// this cache holds that base itself: every tier stores whole objects,
    /// so a delta from upstream must be applicable here.
    async fn fetch_upstream(&self, oids: &[Oid], bases: &[Option<Oid>]) -> Result<()> {
        let _permit = self.upstream_permits.acquire().await?;
        self.stats.upstream_requests.fetch_add(1, Ordering::Relaxed);
        let store = self.store.clone();
        let bases = bases.to_vec();
        let bases = tokio::task::spawn_blocking(move || {
            bases
                .into_iter()
                .map(|b| b.filter(|b| store.has(b)))
                .collect::<Vec<_>>()
        })
        .await?;
        let body = serde_json::to_vec(&ObjectsRequest {
            oids: oids.to_vec(),
            bases: if bases.iter().any(Option::is_some) {
                bases
            } else {
                Vec::new()
            },
        })?;
        let resp = self
            .upstream_call(
                Method::POST,
                "/v1/objects",
                Some("application/json"),
                body.into(),
                &[],
            )
            .await
            .context("upstream unreachable")?;
        if !resp.status.is_success() {
            bail!("upstream returned {}", resp.status);
        }
        let mut dec = FrameDecoder::default();
        let frames = dec.push(&resp.body)?;
        dec.finish()?;
        let stored: u64 = frames
            .iter()
            .map(|(_, p)| match p {
                wire::Payload::Missing => 0,
                wire::Payload::Loose(d) | wire::Payload::Delta(d) => d.len() as u64,
            })
            .sum();
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            // Verify once, on the miss path only (rare in steady state): a
            // corrupt or truncated upstream response must not be cached
            // forever.
            for (oid, payload) in &frames {
                wire::store_frame(&store, oid, payload)?;
            }
            Ok(())
        })
        .await??;
        self.since_maintain.fetch_add(stored, Ordering::Relaxed);
        Ok(())
    }

    /// `fetch_upstream`, except that an object another request is already
    /// fetching is waited for rather than asked for again. `Ok` means the
    /// upstream answered for every object (it may have said "missing"): if
    /// the fetch waited for fails, its objects are fetched again here.
    async fn fetch_coalesced(&self, oids: &[Oid], bases: &[Option<Oid>]) -> Result<()> {
        let mut todo: Vec<(Oid, Option<Oid>)> = oids
            .iter()
            .enumerate()
            .map(|(i, o)| (*o, bases.get(i).copied().flatten()))
            .collect();
        loop {
            let (tx, rx) = watch::channel(false);
            let (mut mine, mut mine_bases) = (Vec::new(), Vec::new());
            let mut wait: Vec<(Oid, Option<Oid>, watch::Receiver<bool>)> = Vec::new();
            {
                let mut inflight = self.inflight.lock().unwrap();
                for (o, b) in &todo {
                    match inflight.get(o) {
                        Some(r) => wait.push((*o, *b, r.clone())),
                        None => {
                            inflight.insert(*o, rx.clone());
                            mine.push(*o);
                            mine_bases.push(*b);
                        }
                    }
                }
            }
            // Removes this request's entries and wakes its waiters however
            // it ends, including when the request is cancelled.
            let guard = Inflight {
                map: &self.inflight,
                oids: mine.clone(),
                tx,
            };
            self.stats
                .coalesced
                .fetch_add(wait.len() as u64, Ordering::Relaxed);
            let mut result = Ok(());
            for (chunk, chunk_bases) in mine
                .chunks(wire::MAX_BATCH)
                .zip(mine_bases.chunks(wire::MAX_BATCH))
            {
                if let Err(e) = self.fetch_upstream(chunk, chunk_bases).await {
                    result = Err(e);
                    break;
                }
            }
            drop(guard);
            result?;
            for (_, _, r) in &mut wait {
                // An error means the fetcher went away; checked below.
                let _ = r.wait_for(|done| *done).await;
            }
            // What the fetches waited for did not bring (they failed, or
            // were cancelled) is fetched again, by this request if no other
            // is at it by now.
            let store = self.store.clone();
            let waited: Vec<(Oid, Option<Oid>)> =
                wait.into_iter().map(|(o, b, _)| (o, b)).collect();
            todo = tokio::task::spawn_blocking(move || {
                waited.into_iter().filter(|(o, _)| !store.has(o)).collect()
            })
            .await?;
            if todo.is_empty() {
                return Ok(());
            }
        }
    }

    /// Run a node's maintenance (in the background) once enough has
    /// arrived since the last run.
    fn maybe_maintain(self: &Arc<Self>) {
        let Some(b) = &self.bounded else {
            return;
        };
        if self.since_maintain.load(Ordering::Relaxed) < (b.budget / 20).max(64 << 10) {
            return;
        }
        if self.maintaining.swap(true, Ordering::SeqCst) {
            return;
        }
        self.since_maintain.store(0, Ordering::Relaxed);
        let st = self.clone();
        tokio::spawn(async move {
            let b = st.bounded.clone().unwrap();
            match tokio::task::spawn_blocking(move || b.maintain()).await {
                Ok(Ok(r)) if r.segments_dropped > 0 => tracing::info!(?r, "evicted"),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!("maintenance failed: {e:#}"),
                Err(e) => tracing::warn!("maintenance panicked: {e}"),
            }
            st.maintaining.store(false, Ordering::SeqCst);
        });
    }

    /// Run maintenance now (for nodes; a no-op elsewhere).
    pub async fn maintain(&self) -> Result<Option<wren_core::bounded::MaintainReport>> {
        let Some(b) = self.bounded.clone() else {
            return Ok(None);
        };
        Ok(Some(
            tokio::task::spawn_blocking(move || b.maintain()).await??,
        ))
    }

    fn present(&self, oid: &Oid) -> bool {
        self.memory.lock().unwrap().map.contains_key(oid) || self.store.has(oid)
    }

    /// Download a snapshot file from upstream into `snapshot_dir`
    /// (resuming a partial download), verify it against its name, and
    /// return its path.
    pub async fn fetch_snapshot_file(&self, file: &str) -> Result<PathBuf> {
        let (name, ext) = file.rsplit_once('.').context("bad snapshot file")?;
        if !(ext == "pack" || ext == "idx") || !wren_core::snapshot::is_segment_name(name) {
            bail!("bad snapshot file {file:?}");
        }
        let dest = self.snapshot_dir.join(file);
        if dest.exists() {
            return Ok(dest);
        }
        // One download at a time; whoever waited finds the file.
        let _one = self.snapshot_flights.lock().await;
        if dest.exists() {
            return Ok(dest);
        }
        tokio::fs::create_dir_all(&self.snapshot_dir).await?;
        let part = dest.with_extension(format!("{ext}.part"));
        const CHUNK: u64 = 8 << 20;
        loop {
            let have = tokio::fs::metadata(&part)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            let resp = {
                let _permit = self.upstream_permits.acquire().await?;
                self.stats.upstream_requests.fetch_add(1, Ordering::Relaxed);
                self.upstream_call(
                    Method::GET,
                    &format!("/v1/snapshots/{file}"),
                    None,
                    Bytes::new(),
                    &[(header::RANGE, format!("bytes={have}-{}", have + CHUNK - 1))],
                )
                .await?
            };
            let total = match resp.status {
                StatusCode::PARTIAL_CONTENT => resp
                    .headers
                    .get(header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.rsplit_once('/'))
                    .and_then(|(_, t)| t.parse::<u64>().ok())
                    .context("bad Content-Range")?,
                StatusCode::OK if have == 0 => resp.body.len() as u64,
                StatusCode::RANGE_NOT_SATISFIABLE => have,
                s => bail!("upstream returned {s} for snapshot {file}"),
            };
            if !resp.body.is_empty() {
                use tokio::io::AsyncWriteExt;
                let mut f = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&part)
                    .await?;
                f.write_all(&resp.body).await?;
                f.flush().await?;
            }
            let now = have + resp.body.len() as u64;
            if now >= total {
                break;
            }
            if resp.body.is_empty() {
                bail!("snapshot {file}: upstream sent nothing");
            }
        }
        let bytes = tokio::fs::read(&part).await?;
        if let Err(e) = wren_core::snapshot::verify(name, ext, &bytes) {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
        tokio::fs::rename(&part, &dest).await?;
        self.stats
            .snapshot_downloads
            .fetch_add(1, Ordering::Relaxed);
        Ok(dest)
    }

    /// Seed an empty node from `repo`'s current snapshot. Returns the
    /// number of objects installed (0 if the store was not empty or there
    /// is no snapshot).
    pub async fn seed_from_snapshot(&self, repo: &str) -> Result<usize> {
        let store = self.store.clone();
        let empty = tokio::task::spawn_blocking(move || store.packs().count() == 0).await?;
        if !empty {
            return Ok(0);
        }
        let resp = self
            .upstream_call(
                Method::GET,
                &format!("/api/v1/repos/{repo}/snapshot"),
                None,
                Bytes::new(),
                &[],
            )
            .await?;
        if resp.status == StatusCode::NOT_FOUND {
            return Ok(0);
        }
        if !resp.status.is_success() {
            bail!("upstream returned {} for the snapshot", resp.status);
        }
        let info: wren_core::snapshot::SnapshotInfo = serde_json::from_slice(&resp.body)?;
        let pack = self
            .fetch_snapshot_file(&format!("{}.pack", info.name))
            .await?;
        let idx = self
            .fetch_snapshot_file(&format!("{}.idx", info.name))
            .await?;
        // Install copies (a node may also serve the files to others).
        let tmp = self
            .store
            .root()
            .join(format!("snapshot-{}", std::process::id()));
        tokio::fs::create_dir_all(&tmp).await?;
        let tpack = tmp.join(format!("{}.pack", info.name));
        tokio::fs::copy(&pack, &tpack).await?;
        tokio::fs::copy(&idx, tpack.with_extension("idx")).await?;
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.add_pack(&tpack)).await??;
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        Ok(info.objects)
    }
}

async fn objects_batch(
    State(st): St,
    Json(req): Json<ObjectsRequest>,
) -> Result<Response, ApiError> {
    if req.oids.len() > wire::MAX_BATCH {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("at most {} oids per request", wire::MAX_BATCH),
        ));
    }
    if let Some(b) = &st.bounded {
        // Before anything else: maintenance keeps what was asked for.
        b.touch(&req.oids);
    }
    let req = Arc::new(req);
    // Twice at most: an object present at the first look may be evicted
    // (by a node's maintenance) before it is read.
    for _ in 0..2 {
        let st2 = st.clone();
        let req2 = req.clone();
        let missing: Vec<(Oid, Option<Oid>)> = tokio::task::spawn_blocking(move || {
            req2.oids
                .iter()
                .enumerate()
                .filter(|(_, o)| !st2.present(o))
                .map(|(i, o)| (*o, req2.base(i)))
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        if missing.is_empty() {
            break;
        }
        st.stats
            .misses
            .fetch_add(missing.len() as u64, Ordering::Relaxed);
        let (oids, bases): (Vec<Oid>, Vec<Option<Oid>>) = missing.into_iter().unzip();
        st.fetch_coalesced(&oids, &bases)
            .await
            .map_err(upstream_error)?;
    }
    // Everything is now whole in this cache (or missing upstream too).
    // Answer from memory or the store, with deltas against the requester's
    // bases.
    let st2 = st.clone();
    let body = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut body = Vec::new();
        let (mut hits, mut memory_hits) = (0u64, 0u64);
        for (i, oid) in req.oids.iter().enumerate() {
            if req.base(i).is_none() {
                let cached = st2.memory.lock().unwrap().get(oid);
                if let Some(raw) = cached {
                    memory_hits += 1;
                    wire::encode_frame(&mut body, oid, Some(&raw))?;
                    continue;
                }
                if let Some(raw) = st2.store.get_raw(oid)? {
                    hits += 1;
                    let raw = Arc::new(raw);
                    wire::encode_frame(&mut body, oid, Some(&raw))?;
                    st2.memory.lock().unwrap().put(*oid, raw);
                    continue;
                }
                wire::encode_frame(&mut body, oid, None)?;
            } else if wire::encode_from_store(&mut body, &st2.store, oid, req.base(i))? {
                hits += 1;
            }
        }
        // Memory hits are hits too; they are also counted on their own.
        st2.stats
            .hits
            .fetch_add(hits + memory_hits, Ordering::Relaxed);
        st2.stats
            .memory_hits
            .fetch_add(memory_hits, Ordering::Relaxed);
        Ok(body)
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))??;
    st.maybe_maintain();
    Ok(objects_response(body))
}

async fn object_one(State(st): St, Path(oid): Path<String>) -> Result<Response, ApiError> {
    let oid: Oid = oid
        .parse()
        .map_err(|e| wren_core::bad_request(format!("{e}")))?;
    if let Some(b) = &st.bounded {
        b.touch(&[oid]);
    }
    let raw = match st.store.get_raw(&oid)? {
        Some(r) => {
            st.stats.hits.fetch_add(1, Ordering::Relaxed);
            Some(r)
        }
        None => {
            st.stats.misses.fetch_add(1, Ordering::Relaxed);
            st.fetch_coalesced(&[oid], &[])
                .await
                .map_err(upstream_error)?;
            st.store.get_raw(&oid)?
        }
    };
    st.maybe_maintain();
    match raw {
        Some(raw) => Ok((
            [
                (header::CONTENT_TYPE, "application/x-git-loose-object"),
                (header::CACHE_CONTROL, immutable(st.token.is_some())),
            ],
            raw,
        )
            .into_response()),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("object {oid} not found"),
        )),
    }
}

async fn snapshot_file(
    State(st): St,
    Path(file): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let path = st
        .fetch_snapshot_file(&file)
        .await
        .map_err(upstream_error)?;
    Ok(serve_file(&path, &headers, st.token.is_some()).await?)
}

fn upstream_error(e: anyhow::Error) -> ApiError {
    ApiError(StatusCode::BAD_GATEWAY, format!("upstream: {e:#}"))
}

fn api_cache_path(st: &CacheState, semantics: &str, path_q: &str) -> PathBuf {
    let key = hex::encode(Sha1::digest(format!("{semantics}\n{path_q}").as_bytes()));
    st.api_dir.join(&key[..2]).join(&key[2..])
}

/// Whether a request under /api/ may be forwarded. The cache presents its
/// own upstream token, so relaying a write would let anyone who can reach the
/// cache write with the cache's credentials. Only queries (GET) and the
/// sync trigger pass.
fn is_read(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    if *method == Method::GET || *method == Method::HEAD {
        return true;
    }
    let sync = path
        .strip_prefix("/api/v1/repos/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(repo, endpoint)| !repo.is_empty() && endpoint == "sync");
    *method == Method::POST && sync
}

/// A stored immutable answer, if there is one.
async fn stored_answer(
    st: &CacheState,
    file: &std::path::Path,
    semantics: &str,
) -> Option<Response> {
    let body = tokio::fs::read(file).await.ok()?;
    let ctype = tokio::fs::read_to_string(file.with_extension("type"))
        .await
        .unwrap_or_default();
    let mut out = Response::new(Body::from(body));
    if let Ok(v) = HeaderValue::from_str(&ctype) {
        out.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    out.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(immutable(st.token.is_some())),
    );
    if let Ok(v) = HeaderValue::from_str(semantics) {
        out.headers_mut().insert(SEMANTICS_HEADER, v);
    }
    Some(out)
}

/// How long a repository's snapshot description is reused. It changes as
/// the default branch moves, but a slightly older snapshot seeds a node
/// just as well; without this, every node starting at once would ask the
/// server.
const SNAPSHOT_INFO_TTL: std::time::Duration = std::time::Duration::from_secs(10);

fn is_snapshot_info(path: &str) -> bool {
    path.strip_prefix("/api/v1/repos/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(repo, rest)| !repo.is_empty() && rest == "snapshot")
}

/// Serve a query: from the immutable response cache when possible,
/// otherwise forwarded to the upstream tier. Concurrent identical queries
/// that miss share one upstream request.
async fn proxy(State(st): St, req: Request) -> Result<Response, ApiError> {
    let path_q = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let method = req.method().clone();
    if !is_read(&method, req.uri().path()) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "a cache does not relay writes; send them directly to the server \
             (the control plane)"
                .into(),
        ));
    }
    let cacheable = method == axum::http::Method::GET;
    let short_lived = cacheable && is_snapshot_info(req.uri().path());
    let semantics = st.current_semantics();
    let cache_file = api_cache_path(&st, &semantics, &path_q);
    let mut _flight = None;
    if cacheable {
        for attempt in 0..2 {
            if let Some(out) = stored_answer(&st, &cache_file, &semantics).await {
                st.stats.api_hits.fetch_add(1, Ordering::Relaxed);
                return Ok(out);
            }
            if short_lived {
                let hit = st
                    .short_lived
                    .lock()
                    .unwrap()
                    .get(&path_q)
                    .filter(|(at, _)| at.elapsed() < SNAPSHOT_INFO_TTL)
                    .map(|(_, r)| r.clone());
                if let Some((status, ctype, body)) = hit {
                    st.stats.api_hits.fetch_add(1, Ordering::Relaxed);
                    let mut out = Response::new(Body::from(body));
                    *out.status_mut() = status;
                    if let Some(c) = ctype {
                        out.headers_mut().insert(header::CONTENT_TYPE, c);
                    }
                    return Ok(out);
                }
            }
            if attempt == 1 {
                break;
            }
            // Wait for an identical query in flight, then look again; or
            // become the one in flight.
            let waiting = {
                let mut flights = st.api_flights.lock().unwrap();
                match flights.get(&path_q) {
                    Some(r) => Some(r.clone()),
                    None => {
                        let (tx, rx) = watch::channel(false);
                        flights.insert(path_q.clone(), rx);
                        _flight = Some(ApiFlight {
                            map: &st.api_flights,
                            key: path_q.clone(),
                            tx,
                        });
                        None
                    }
                }
            };
            match waiting {
                Some(mut r) => {
                    st.stats.coalesced.fetch_add(1, Ordering::Relaxed);
                    let _ = r.wait_for(|done| *done).await;
                }
                None => break,
            }
        }
        st.stats.api_misses.fetch_add(1, Ordering::Relaxed);
    }
    let body = axum::body::to_bytes(req.into_body(), 64 << 20)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let resp = {
        let _permit = st
            .upstream_permits
            .acquire()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        st.stats.upstream_requests.fetch_add(1, Ordering::Relaxed);
        st.upstream_call(method, &path_q, Some("application/json"), body, &[])
            .await
            .map_err(|e| {
                ApiError(
                    StatusCode::BAD_GATEWAY,
                    format!("upstream unreachable: {e:#}"),
                )
            })?
    };
    let status = resp.status;
    let ctype = resp.headers.get(header::CONTENT_TYPE).cloned();
    let cache_control = resp.headers.get(header::CACHE_CONTROL).cloned();
    let synced_at = resp.headers.get(SYNCED_AT_HEADER).cloned();
    let resp_semantics = resp
        .headers
        .get(SEMANTICS_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(v) = &resp_semantics {
        st.learn_semantics(v);
    }
    let upstream_immutable = resp
        .headers
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("immutable"));
    let bytes = resp.body;
    if cacheable && upstream_immutable && status == StatusCode::OK {
        // Store under the semantics that produced the answer.
        let file = match &resp_semantics {
            Some(v) if *v != semantics => api_cache_path(&st, v, &path_q),
            _ => cache_file,
        };
        let _ = write_atomic(
            &file,
            &bytes,
            ctype.as_ref().and_then(|c| c.to_str().ok()).unwrap_or(""),
        )
        .await;
    }
    if short_lived && (status == StatusCode::OK || status == StatusCode::NOT_FOUND) {
        st.short_lived.lock().unwrap().insert(
            path_q.clone(),
            (
                std::time::Instant::now(),
                (status, ctype.clone(), bytes.clone()),
            ),
        );
    }
    let mut out = Response::new(Body::from(bytes));
    *out.status_mut() = status;
    if let Some(c) = ctype {
        out.headers_mut().insert(header::CONTENT_TYPE, c);
    }
    // Relay the upstream's caching decision unchanged (immutable, no-store,
    // …) so HTTP caches below this tier can honour it too.
    if let Some(cc) = cache_control {
        out.headers_mut().insert(header::CACHE_CONTROL, cc);
    }
    if let Some(v) = synced_at {
        out.headers_mut().insert(SYNCED_AT_HEADER, v);
    }
    if let Some(v) = resp_semantics.and_then(|v| HeaderValue::from_str(&v).ok()) {
        out.headers_mut().insert(SEMANTICS_HEADER, v);
    }
    Ok(out)
}

/// A query this cache is forwarding; identical ones wait for it.
struct ApiFlight<'a> {
    map: &'a Mutex<HashMap<String, watch::Receiver<bool>>>,
    key: String,
    tx: watch::Sender<bool>,
}

impl Drop for ApiFlight<'_> {
    fn drop(&mut self) {
        self.map.lock().unwrap().remove(&self.key);
        let _ = self.tx.send(true);
    }
}

async fn write_atomic(path: &std::path::Path, body: &[u8], ctype: &str) -> Result<()> {
    let dir = path.parent().unwrap();
    tokio::fs::create_dir_all(dir).await?;
    static N: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "tmp{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    // Content type first: the body's presence is what marks the entry valid.
    tokio::fs::write(path.with_extension("type"), ctype).await?;
    tokio::fs::write(&tmp, body).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}
