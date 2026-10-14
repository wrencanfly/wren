//! `wren cache`: a caching tier. It answers exactly the same RPCs as the
//! server, so clients cannot tell (and need not know) where in the hierarchy
//! they are. Run one per node (like scion-daemon) and/or one per region (like
//! scion-cache); each points at exactly one upstream tier.
//!
//! Everything it keeps is immutable, so nothing is ever revalidated:
//! * objects, which are content-addressed;
//! * query responses the upstream marked `immutable`, i.e. queries whose
//!   every revision is a full commit id (see `server::is_pinned`).
//!
//! Other query calls are forwarded. All upstream traffic shares one
//! concurrency limit and has a timeout, so a cold or flooded cache cannot
//! overwhelm the tier above it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::Serialize;
use sha1::{Digest, Sha1};
use tokio::sync::Semaphore;
use wren_core::api::{Capabilities, PROTOCOL_VERSION};
use wren_core::wire::{self, FrameDecoder, ObjectsRequest};
use wren_core::{ObjectStore, Oid};

use crate::server::{objects_response, protocol_header, require_token, ApiError, IMMUTABLE};

pub struct CacheState {
    pub store: Arc<ObjectStore>,
    /// Where immutable query responses are kept.
    pub api_dir: PathBuf,
    pub upstream: String,
    pub http: reqwest::Client,
    /// Token clients of this tier must present (None = open).
    pub token: Option<String>,
    /// Token this tier presents to its upstream.
    pub upstream_token: Option<String>,
    /// Bounds concurrent upstream requests (objects and queries alike).
    pub upstream_permits: Semaphore,
    pub stats: Stats,
}

#[derive(Default)]
pub struct Stats {
    hits: AtomicU64,
    misses: AtomicU64,
    api_hits: AtomicU64,
    api_misses: AtomicU64,
    upstream_requests: AtomicU64,
}

#[derive(Serialize)]
struct StatsBody {
    hits: u64,
    misses: u64,
    hit_rate: f64,
    api_hits: u64,
    api_misses: u64,
    upstream_requests: u64,
}

type St = State<Arc<CacheState>>;

pub fn router(state: Arc<CacheState>) -> Router {
    let token = state.token.clone();
    Router::new()
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/stats", get(stats))
        .route("/v1/objects", post(objects_batch))
        .route("/v1/objects/{oid}", get(object_one))
        .route("/api/{*rest}", any(proxy))
        .layer(axum::middleware::from_fn(move |req, next| {
            require_token(token.clone(), req, next)
        }))
        .route("/healthz", get(|| async { "ok" }))
        .layer(axum::middleware::map_response(protocol_header))
        .with_state(state)
}

async fn capabilities() -> Json<Capabilities> {
    Json(Capabilities {
        protocol: PROTOCOL_VERSION,
        object_format: "sha1".into(),
        tier: "cache".into(),
        features: [
            "objects-batch",
            "objects-single",
            "query-api",
            "bisect",
            "landed",
            "immutable-pinned-queries",
        ]
        .map(String::from)
        .to_vec(),
    })
}

async fn stats(State(st): St) -> Json<StatsBody> {
    let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let (hits, misses) = (load(&st.stats.hits), load(&st.stats.misses));
    Json(StatsBody {
        hits,
        misses,
        hit_rate: if hits + misses == 0 {
            0.0
        } else {
            hits as f64 / (hits + misses) as f64
        },
        api_hits: load(&st.stats.api_hits),
        api_misses: load(&st.stats.api_misses),
        upstream_requests: load(&st.stats.upstream_requests),
    })
}

impl CacheState {
    fn upstream_request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut rb = self
            .http
            .request(method, format!("{}{}", self.upstream, path));
        if let Some(t) = &self.upstream_token {
            rb = rb.bearer_auth(t);
        }
        rb
    }

    /// Fetch `oids` from the upstream tier and store them. Returns the frames.
    async fn fetch_upstream(&self, oids: &[Oid]) -> Result<Vec<(Oid, Option<Vec<u8>>)>> {
        let _permit = self.upstream_permits.acquire().await?;
        self.stats.upstream_requests.fetch_add(1, Ordering::Relaxed);
        let resp = self
            .upstream_request(reqwest::Method::POST, "/v1/objects")
            .json(&ObjectsRequest {
                oids: oids.to_vec(),
            })
            .send()
            .await
            .context("upstream unreachable")?;
        if !resp.status().is_success() {
            bail!("upstream returned {}", resp.status());
        }
        let body = resp.bytes().await?;
        let mut dec = FrameDecoder::default();
        let frames = dec.push(&body)?;
        dec.finish()?;
        let store = self.store.clone();
        let frames = tokio::task::spawn_blocking(move || -> Result<_> {
            // Verify once, on the miss path only (rare in steady state): a
            // corrupt or truncated upstream response must not be cached
            // forever.
            for (oid, raw) in &frames {
                if let Some(raw) = raw {
                    store.put_raw_verified(oid, raw)?;
                }
            }
            Ok(frames)
        })
        .await??;
        Ok(frames)
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
    let store = st.store.clone();
    let oids = req.oids.clone();
    let mut found: Vec<Option<Vec<u8>>> = tokio::task::spawn_blocking(move || {
        oids.iter()
            .map(|o| store.get_raw(o))
            .collect::<Result<Vec<_>>>()
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))??;
    let missing: Vec<Oid> = req
        .oids
        .iter()
        .zip(&found)
        .filter(|(_, f)| f.is_none())
        .map(|(o, _)| *o)
        .collect();
    st.stats
        .hits
        .fetch_add((req.oids.len() - missing.len()) as u64, Ordering::Relaxed);
    st.stats
        .misses
        .fetch_add(missing.len() as u64, Ordering::Relaxed);
    if !missing.is_empty() {
        let fetched: std::collections::HashMap<Oid, Option<Vec<u8>>> = st
            .fetch_upstream(&missing)
            .await
            .map_err(upstream_error)?
            .into_iter()
            .collect();
        for (oid, slot) in req.oids.iter().zip(found.iter_mut()) {
            if slot.is_none() {
                *slot = fetched.get(oid).cloned().flatten();
            }
        }
    }
    let mut body = Vec::new();
    for (oid, raw) in req.oids.iter().zip(&found) {
        wire::encode_frame(&mut body, oid, raw.as_deref())?;
    }
    Ok(objects_response(body))
}

async fn object_one(State(st): St, Path(oid): Path<String>) -> Result<Response, ApiError> {
    let oid: Oid = oid
        .parse()
        .map_err(|e| wren_core::bad_request(format!("{e}")))?;
    let raw = match st.store.get_raw(&oid)? {
        Some(r) => {
            st.stats.hits.fetch_add(1, Ordering::Relaxed);
            Some(r)
        }
        None => {
            st.stats.misses.fetch_add(1, Ordering::Relaxed);
            st.fetch_upstream(&[oid])
                .await
                .map_err(upstream_error)?
                .pop()
                .and_then(|f| f.1)
        }
    };
    match raw {
        Some(raw) => Ok((
            [
                (header::CONTENT_TYPE, "application/x-git-loose-object"),
                (header::CACHE_CONTROL, IMMUTABLE),
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

fn upstream_error(e: anyhow::Error) -> ApiError {
    ApiError(StatusCode::BAD_GATEWAY, format!("upstream: {e:#}"))
}

fn api_cache_path(st: &CacheState, path_q: &str) -> PathBuf {
    let key = hex::encode(Sha1::digest(path_q.as_bytes()));
    st.api_dir.join(&key[..2]).join(&key[2..])
}

/// Serve a query: from the immutable response cache when possible,
/// otherwise forwarded to the upstream tier.
async fn proxy(State(st): St, req: Request) -> Result<Response, ApiError> {
    let path_q = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let method = req.method().clone();
    let cacheable = method == axum::http::Method::GET;
    let cache_file = api_cache_path(&st, &path_q);
    if cacheable {
        if let Ok(body) = tokio::fs::read(&cache_file).await {
            st.stats.api_hits.fetch_add(1, Ordering::Relaxed);
            let ctype = tokio::fs::read_to_string(cache_file.with_extension("type"))
                .await
                .unwrap_or_default();
            let mut out = Response::new(Body::from(body));
            if let Ok(v) = HeaderValue::from_str(&ctype) {
                out.headers_mut().insert(header::CONTENT_TYPE, v);
            }
            out.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
            return Ok(out);
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
        st.upstream_request(method, &path_q)
            .body(body)
            .header(header::CONTENT_TYPE, "application/json")
            .send()
            .await
            .map_err(|e| {
                ApiError(
                    StatusCode::BAD_GATEWAY,
                    format!("upstream unreachable: {e}"),
                )
            })?
    };
    let status = resp.status();
    let ctype = resp.headers().get(header::CONTENT_TYPE).cloned();
    let cache_control = resp.headers().get(header::CACHE_CONTROL).cloned();
    let immutable = resp
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("immutable"));
    let bytes = resp.bytes().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    if cacheable && immutable && status == StatusCode::OK {
        let _ = write_atomic(
            &cache_file,
            &bytes,
            ctype.as_ref().and_then(|c| c.to_str().ok()).unwrap_or(""),
        )
        .await;
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
    Ok(out)
}

async fn write_atomic(path: &std::path::Path, body: &[u8], ctype: &str) -> Result<()> {
    let dir = path.parent().unwrap();
    tokio::fs::create_dir_all(dir).await?;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    // Content type first: the body's presence is what marks the entry valid.
    tokio::fs::write(path.with_extension("type"), ctype).await?;
    tokio::fs::write(&tmp, body).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}
