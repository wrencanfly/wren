//! `wren server`: the authoritative tier. Serves the query API and the object
//! protocol over the shared content-addressed store, and keeps each repo in
//! sync with its upstream git host.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use wren_core::api::*;
use wren_core::changes::normalize_path;
use wren_core::wire::{self, ObjectsRequest};
use wren_core::{ObjectStore, Oid, Repo};

pub mod access;
mod git;
mod writes;

pub struct ServerState {
    pub store: Arc<ObjectStore>,
    pub repos: BTreeMap<String, Arc<Repo>>,
    /// If set, every request except /healthz must carry
    /// `Authorization: Bearer <token>`.
    pub token: Option<String>,
    /// If set, the write API is enabled and needs this token. It is also
    /// accepted wherever `token` is.
    pub write_token: Option<String>,
    /// Per repo: when the last on-demand sync finished (see `resolve`).
    pub on_demand: BTreeMap<String, Mutex<Option<Instant>>>,
    pub snapshots: SnapshotConfig,
    /// Read tokens restricted to some directories (see `access`).
    pub acl: Vec<access::AclEntry>,
    pub rate_limit: Option<access::RateLimit>,
    /// What restricted tokens may fetch, per (repo, commit, token name).
    pub scoped: access::ScopeCache,
}

/// Where snapshots (see `wren_core::snapshot`) are kept, and what they hold.
#[derive(Clone, Debug, Default)]
pub struct SnapshotConfig {
    /// Directory of snapshot files; `None` disables snapshots.
    pub dir: Option<std::path::PathBuf>,
    /// Directories whose blobs are included (empty: all).
    pub cone: Vec<String>,
    /// Rebuild from the default branch this often (and at startup).
    pub every: Option<Duration>,
}

/// On-demand syncs of one repo are at least this far apart, so a stream of
/// requests for unknown revisions cannot turn into a stream of fetches.
const ON_DEMAND_MIN_INTERVAL: Duration = Duration::from_secs(1);

impl ServerState {
    pub fn new(
        store: Arc<ObjectStore>,
        repos: BTreeMap<String, Arc<Repo>>,
        token: Option<String>,
        write_token: Option<String>,
    ) -> ServerState {
        let on_demand = repos
            .keys()
            .map(|k| (k.clone(), Mutex::new(None)))
            .collect();
        ServerState {
            store,
            repos,
            token,
            write_token,
            on_demand,
            snapshots: SnapshotConfig::default(),
            acl: Vec::new(),
            rate_limit: None,
            scoped: Default::default(),
        }
    }

    /// Resolve `rev`. If it is unknown and the repo has an upstream, sync
    /// once and try again: a client that just pushed a commit (or a branch)
    /// can ask about it immediately instead of waiting for the next poll.
    /// Concurrent requests share one sync.
    fn resolve(&self, r: &Repo, rev: &str) -> Result<Oid> {
        let first = r.resolve(rev);
        let unknown = matches!(&first, Err(e) if e.chain().any(|c| c.is::<wren_core::NotFound>()));
        if !unknown || r.upstream.is_none() {
            return first;
        }
        let Some(slot) = self.on_demand.get(&r.name) else {
            return first;
        };
        let asked = Instant::now();
        let mut last = slot.lock().unwrap_or_else(|p| p.into_inner());
        let fresh_enough = last.is_some_and(|t| t > asked || t.elapsed() < ON_DEMAND_MIN_INTERVAL);
        if !fresh_enough {
            if let Err(e) = r.sync() {
                tracing::warn!(repo = %r.name, "on-demand sync failed: {e:#}");
            }
            *last = Some(Instant::now());
        }
        drop(last);
        r.resolve(rev)
    }

    fn private(&self) -> bool {
        self.token.is_some()
    }
}

type St = State<Arc<ServerState>>;

// ---- errors -----------------------------------------------------------------

pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = match self.0 {
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::BAD_REQUEST => "bad_request",
            StatusCode::UNAUTHORIZED => "unauthorized",
            StatusCode::FORBIDDEN => "forbidden",
            StatusCode::CONFLICT => "conflict",
            StatusCode::PAYLOAD_TOO_LARGE => "too_large",
            StatusCode::BAD_GATEWAY | StatusCode::GATEWAY_TIMEOUT => "upstream_unavailable",
            _ => "internal",
        };
        let body = ErrorBody {
            error: self.1,
            code: code.into(),
        };
        (self.0, Json(body)).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> ApiError {
        let status = if e.chain().any(|c| c.is::<wren_core::NotFound>()) {
            StatusCode::NOT_FOUND
        } else if e.chain().any(|c| c.is::<wren_core::BadRequest>()) {
            StatusCode::BAD_REQUEST
        } else {
            // Anything not attributable to the request is a server fault.
            StatusCode::INTERNAL_SERVER_ERROR
        };
        ApiError(status, format!("{e:#}"))
    }
}

fn not_found(what: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, what.into())
}

type ApiResult<T> = std::result::Result<T, ApiError>;

/// Run blocking repository work off the async executor.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task failed: {e}"),
        )
    })?
}

fn repo(st: &ServerState, name: &str) -> ApiResult<Arc<Repo>> {
    st.repos
        .get(name)
        .cloned()
        .ok_or_else(|| not_found(format!("unknown repo {name:?}")))
}

// ---- router -----------------------------------------------------------------

pub fn router(state: Arc<ServerState>) -> Router {
    let private = state.private();
    let reads = Router::new()
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/objects", post(objects_batch))
        .route("/v1/objects/{oid}", get(object_one))
        .route("/api/v1/repos", get(list_repos))
        .route("/api/v1/whoami", get(access::whoami))
        .route("/api/v1/repos/{repo}/refs", get(refs))
        .route("/api/v1/repos/{repo}/resolve", get(resolve))
        .route("/api/v1/repos/{repo}/commits/{rev}", get(commit))
        .route("/api/v1/repos/{repo}/log", get(log))
        .route("/api/v1/repos/{repo}/merge-base", get(merge_base))
        .route("/api/v1/repos/{repo}/is-ancestor", get(is_ancestor))
        .route("/api/v1/repos/{repo}/landed", get(landed))
        .route("/api/v1/repos/{repo}/diff", get(diff))
        .route("/api/v1/repos/{repo}/blame", get(blame))
        .route("/api/v1/repos/{repo}/tree", get(tree))
        .route("/api/v1/repos/{repo}/file", get(file))
        .route("/api/v1/repos/{repo}/bisect", get(bisect))
        .route("/api/v1/repos/{repo}/grep", get(grep))
        .route("/api/v1/repos/{repo}/sync", post(sync))
        .route("/api/v1/repos/{repo}/snapshot", get(snapshot_latest))
        .route("/api/v1/repos/{repo}/audit", get(audit))
        .route("/api/v1/repos/{repo}/paths", get(find_paths))
        .route("/v1/snapshots/{file}", get(snapshot_file))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            freshness_header,
        ))
        .layer(axum::middleware::from_fn(move |req, next| {
            pinned_queries_are_immutable(private, req, next)
        }))
        .layer(axum::middleware::from_fn(access::enforce))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_read_token,
        ));
    reads
        .merge(writes::routes(state.clone()))
        .merge(git::routes())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            access::rate_limit,
        ))
        .route("/healthz", get(|| async { "ok" }))
        .layer(axum::middleware::map_response(protocol_header))
        .with_state(state)
}

// ---- middleware ---------------------------------------------------------------

pub const PROTOCOL_HEADER: &str = "wren-protocol";
pub const SEMANTICS_HEADER: &str = "wren-semantics";
/// Unix time of the repo's last successful sync with its upstream, on every
/// answer that depends on refs (i.e. every unpinned query).
pub const SYNCED_AT_HEADER: &str = "wren-synced-at";

pub async fn protocol_header(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(
        PROTOCOL_HEADER,
        axum::http::HeaderValue::from(PROTOCOL_VERSION),
    );
    // A cache tier relays its upstream's semantics; only fill in our own.
    if !h.contains_key(SEMANTICS_HEADER) {
        h.insert(
            SEMANTICS_HEADER,
            axum::http::HeaderValue::from_static(QUERY_SEMANTICS),
        );
    }
    resp
}

async fn freshness_header(
    State(st): St,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let repo = req
        .uri()
        .path()
        .strip_prefix("/api/v1/repos/")
        .and_then(|rest| rest.split('/').next())
        .and_then(|name| st.repos.get(name))
        .cloned();
    let pinned = is_pinned(req.uri());
    let mut resp = next.run(req).await;
    if let (Some(r), false) = (repo, pinned) {
        if let Ok((Some(t), _)) = r.sync_status() {
            resp.headers_mut()
                .insert(SYNCED_AT_HEADER, axum::http::HeaderValue::from(t));
        }
    }
    resp
}

pub async fn require_token(
    token: Option<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(t) = token {
        let ok = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|got| constant_time_eq(got.as_bytes(), t.as_bytes()));
        if !ok {
            return ApiError(
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token".into(),
            )
            .into_response();
        }
    }
    next.run(req).await
}

/// Reads need the read token (when one is configured) or the write token.
async fn require_read_token(
    State(st): St,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(t) = &st.token {
        let ok = writes::bearer(&req).is_some_and(|got| {
            constant_time_eq(got.as_bytes(), t.as_bytes())
                || st
                    .write_token
                    .as_deref()
                    .is_some_and(|w| constant_time_eq(got.as_bytes(), w.as_bytes()))
        });
        // A restricted token: allowed, with its grant attached (`access`).
        let grant = if ok {
            None
        } else {
            writes::bearer(&req).and_then(|got| st.grant_for(got))
        };
        if let Some(g) = grant {
            req.extensions_mut().insert(g);
            return next.run(req).await;
        }
        if !ok {
            return ApiError(
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token".into(),
            )
            .into_response();
        }
    }
    next.run(req).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Query parameters that name revisions.
const REV_PARAMS: &[&str] = &[
    "rev", "a", "b", "base", "head", "commit", "ref", "good", "bad",
];

/// A query whose every revision is a full commit id can never change its
/// answer (all inputs are immutable), so it is marked cacheable forever and
/// any cache tier may keep it. Queries naming branches, or defaulting to
/// HEAD, are not.
pub fn is_pinned(uri: &axum::http::Uri) -> bool {
    let path = uri.path();
    let Some(rest) = path.strip_prefix("/api/v1/repos/") else {
        return false;
    };
    let mut parts = rest.split('/');
    let (_repo, endpoint) = (parts.next(), parts.next().unwrap_or(""));
    let is_full_oid =
        |v: &str| v.len() == 40 && v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let mut revs: Vec<String> = Vec::new();
    match endpoint {
        "commits" => revs.push(parts.next().unwrap_or("").to_string()),
        "log" | "merge-base" | "is-ancestor" | "landed" | "diff" | "blame" | "tree" | "file"
        | "bisect" | "resolve" | "grep" => {}
        _ => return false,
    }
    let query: Vec<(String, String)> = serde_urlencoded_pairs(uri.query().unwrap_or(""));
    revs.extend(
        query
            .into_iter()
            .filter(|(k, _)| REV_PARAMS.contains(&k.as_str()))
            .map(|(_, v)| v),
    );
    // Endpoints whose revision defaults to HEAD must name it explicitly.
    let needs = match endpoint {
        "landed" => 2,
        "merge-base" | "is-ancestor" | "bisect" => 2,
        _ => 1,
    };
    revs.len() >= needs && revs.iter().all(|r| is_full_oid(r))
}

fn serde_urlencoded_pairs(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k.to_string(), v.to_string())
        })
        .collect()
}

async fn pinned_queries_are_immutable(
    private: bool,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let pinned = req.method() == axum::http::Method::GET && is_pinned(req.uri());
    let mut resp = next.run(req).await;
    if pinned && resp.status().is_success() && !resp.headers().contains_key(header::CACHE_CONTROL) {
        resp.headers_mut().insert(
            header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static(immutable(private)),
        );
    }
    resp
}

/// `Cache-Control` for answers that can never change. With a token, the
/// content is not public: only Wren's own (authenticated) cache tiers may
/// keep it, never a shared HTTP cache or CDN.
pub fn immutable(private: bool) -> &'static str {
    if private {
        "private, max-age=31536000, immutable"
    } else {
        "public, max-age=31536000, immutable"
    }
}

pub const FEATURES: &[&str] = &[
    "objects-batch",
    "objects-single",
    "query-api",
    "bisect",
    "landed",
    "grep",
    "immutable-pinned-queries",
    "on-demand-sync",
    "error-codes",
    "object-deltas.v1",
    "snapshots.v1",
];

/// Advertised only by a server started with a write token.
pub const WRITE_FEATURE: &str = "write.v1";

async fn capabilities(State(st): St) -> Json<Capabilities> {
    let mut features: Vec<String> = FEATURES.iter().map(|f| f.to_string()).collect();
    if st.write_token.is_some() {
        features.push(WRITE_FEATURE.into());
    }
    Json(Capabilities {
        protocol: PROTOCOL_VERSION,
        object_format: "sha1".into(),
        tier: "server".into(),
        features,
        semantics: QUERY_SEMANTICS.into(),
        store: None,
    })
}

/// Keep repos fresh: sync each one now, then every `interval`.
pub fn spawn_pollers(state: Arc<ServerState>, interval: Option<Duration>) {
    for repo in state.repos.values().cloned() {
        if repo.upstream.is_none() {
            continue;
        }
        tokio::spawn(async move {
            // With a shared bucket, skip a round another server just did.
            let fresh = interval.map_or(Duration::from_secs(30), |d| d / 2);
            loop {
                let r = repo.clone();
                match tokio::task::spawn_blocking(move || r.sync_unless_fresh(fresh)).await {
                    Ok(Ok(Some(rep))) if rep.new_changesets > 0 || rep.refs_updated > 0 => {
                        tracing::info!(repo = %repo.name, ?rep, "synced")
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => tracing::warn!(repo = %repo.name, "sync failed: {e:#}"),
                    Err(e) => tracing::warn!(repo = %repo.name, "sync panicked: {e}"),
                }
                match interval {
                    Some(d) => tokio::time::sleep(d).await,
                    None => break,
                }
            }
        });
    }
}

// ---- object protocol --------------------------------------------------------

pub fn objects_response(body: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, wire::CONTENT_TYPE)], body).into_response()
}

async fn objects_batch(
    State(st): St,
    grant: Option<Extension<access::Grant>>,
    Json(req): Json<ObjectsRequest>,
) -> ApiResult<Response> {
    if req.oids.len() > wire::MAX_BATCH {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("at most {} oids per request", wire::MAX_BATCH),
        ));
    }
    let store = st.store.clone();
    let body = blocking(move || {
        let req = match grant {
            Some(Extension(g)) => st.scope_request(&g, req)?,
            None => req,
        };
        let mut out = Vec::new();
        for (i, oid) in req.oids.iter().enumerate() {
            wire::encode_from_store(&mut out, &store, oid, req.base(i))?;
        }
        Ok(out)
    })
    .await?;
    Ok(objects_response(body))
}

#[derive(Deserialize)]
struct ScopeQ {
    repo: Option<String>,
    commit: Option<Oid>,
}

async fn object_one(
    State(st): St,
    grant: Option<Extension<access::Grant>>,
    Path(oid): Path<String>,
    Query(q): Query<ScopeQ>,
) -> ApiResult<Response> {
    let oid: Oid = oid
        .parse()
        .map_err(|e| wren_core::bad_request(format!("{e}")))?;
    let private = st.private() || grant.is_some();
    let store = st.store.clone();
    let raw = blocking(move || {
        if let Some(Extension(g)) = grant {
            // As for a batch: `?repo=…&commit=…` is the scope.
            let req = ObjectsRequest {
                oids: vec![oid],
                bases: Vec::new(),
                scope: q
                    .repo
                    .zip(q.commit)
                    .map(|(repo, commit)| wire::Scope { repo, commit }),
            };
            st.scope_request(&g, req)?;
        }
        Ok(store.get_raw(&oid)?)
    })
    .await?;
    match raw {
        // Content-addressed: the response for a given URL can never change.
        Some(raw) => Ok((
            [
                (header::CONTENT_TYPE, "application/x-git-loose-object"),
                (header::CACHE_CONTROL, immutable(private)),
            ],
            raw,
        )
            .into_response()),
        None => Err(not_found(format!("object {oid} not found"))),
    }
}

// ---- query API ----------------------------------------------------------------

async fn list_repos(State(st): St) -> ApiResult<Json<Vec<RepoInfo>>> {
    let st2 = st.clone();
    blocking(move || {
        let mut out = Vec::new();
        for r in st2.repos.values() {
            let (tree_overrides, commit_overrides) = r.meta.override_counts()?;
            out.push(RepoInfo {
                tree_overrides,
                commit_overrides,
                name: r.name.clone(),
                upstream: r.upstream.clone(),
                head: r.head_ref()?,
                changesets: r.meta.changeset_count()?,
                last_sync: r.sync_status()?.0,
                last_sync_error: r.sync_status()?.1,
                export: r.export_status()?,
            });
        }
        Ok(Json(out))
    })
    .await
}

async fn refs(State(st): St, Path(name): Path<String>) -> ApiResult<Json<Vec<RefInfo>>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        Ok(Json(
            r.meta
                .refs()?
                .into_iter()
                .map(|x| RefInfo {
                    name: x.name,
                    target: x.target,
                    commit: x.peeled,
                })
                .collect(),
        ))
    })
    .await
}

#[derive(Deserialize)]
struct RevQ {
    rev: Option<String>,
}

async fn resolve(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<RevQ>,
) -> ApiResult<Json<Resolved>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        Ok(Json(Resolved {
            oid: st.resolve(&r, q.rev.as_deref().unwrap_or("HEAD"))?,
        }))
    })
    .await
}

#[derive(Deserialize)]
struct CommitQ {
    #[serde(default)]
    changes: bool,
    /// Also report renames and copies (git -M -C).
    #[serde(default)]
    renames: bool,
}

async fn commit(
    State(st): St,
    Path((name, rev)): Path<(String, String)>,
    Query(q): Query<CommitQ>,
) -> ApiResult<Json<CommitInfo>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let oid = st.resolve(&r, &rev)?;
        let mut info = r.commit_info(&oid, q.changes)?;
        if q.renames {
            info.copies = Some(r.derived::<wren_core::renames::Copies>(&oid)?.0.clone());
        }
        Ok(Json(info))
    })
    .await
}

#[derive(Deserialize)]
struct LogQ {
    rev: Option<String>,
    path: Option<String>,
    limit: Option<usize>,
    #[serde(default)]
    changes: bool,
    /// Follow the file `path` across renames (git log --follow).
    #[serde(default)]
    follow: bool,
}

async fn log(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<LogQ>,
) -> ApiResult<Json<LogResponse>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let tip = st.resolve(&r, q.rev.as_deref().unwrap_or("HEAD"))?;
        let limit = q.limit.unwrap_or(50).min(10_000);
        if q.follow {
            let path = q
                .path
                .as_deref()
                .ok_or_else(|| wren_core::bad_request("follow needs a path"))?;
            let commits = r
                .log_follow(&tip, path, limit)?
                .into_iter()
                .map(|(o, p)| {
                    let mut info = r.commit_info(&o, q.changes)?;
                    info.path = Some(p);
                    Ok(info)
                })
                .collect::<Result<_>>()?;
            return Ok(Json(LogResponse { commits }));
        }
        let oids = r.log(&tip, q.path.as_deref(), limit)?;
        let commits = oids
            .iter()
            .map(|o| r.commit_info(o, q.changes))
            .collect::<Result<_>>()?;
        Ok(Json(LogResponse { commits }))
    })
    .await
}

#[derive(Deserialize)]
struct PairQ {
    a: String,
    b: String,
}

async fn merge_base(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<PairQ>,
) -> ApiResult<Json<MergeBaseResponse>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let (a, b) = (st.resolve(&r, &q.a)?, st.resolve(&r, &q.b)?);
        Ok(Json(MergeBaseResponse {
            bases: r.merge_bases(&a, &b)?,
        }))
    })
    .await
}

async fn is_ancestor(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<PairQ>,
) -> ApiResult<Json<IsAncestorResponse>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let (a, b) = (st.resolve(&r, &q.a)?, st.resolve(&r, &q.b)?);
        Ok(Json(IsAncestorResponse {
            result: r.is_ancestor(&a, &b)?,
        }))
    })
    .await
}

#[derive(Deserialize)]
struct LandedQ {
    commit: String,
    #[serde(rename = "ref")]
    into: Option<String>,
}

#[derive(Deserialize)]
struct BisectQ {
    good: String,
    bad: String,
}

async fn bisect(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<BisectQ>,
) -> ApiResult<Json<BisectStep>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let (good, bad) = (st.resolve(&r, &q.good)?, st.resolve(&r, &q.bad)?);
        Ok(Json(r.bisect_step(&good, &bad)?))
    })
    .await
}

async fn landed(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<LandedQ>,
) -> ApiResult<Json<LandedResponse>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let c = st.resolve(&r, &q.commit)?;
        let tip = st.resolve(&r, q.into.as_deref().unwrap_or("HEAD"))?;
        Ok(Json(LandedResponse {
            landed: r.landed(&c, &tip)?,
        }))
    })
    .await
}

#[derive(Deserialize)]
struct DiffQ {
    base: Option<String>,
    head: String,
    #[serde(default)]
    patch: bool,
    path: Option<String>,
    /// Also report renames and copies (git -M -C).
    #[serde(default)]
    renames: bool,
}

async fn diff(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<DiffQ>,
) -> ApiResult<Json<DiffResponse>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let head = st.resolve(&r, &q.head)?;
        // Default base: the first parent, i.e. "what did this commit change".
        let base =
            match &q.base {
                Some(b) => st.resolve(&r, b)?,
                None => *r.node(&head)?.parents.first().ok_or_else(|| {
                    wren_core::bad_request("root commit has no parent; pass base")
                })?,
            };
        let mut changes = r.diff(&base, &head)?;
        let copies = if q.renames {
            Some(wren_core::renames::detect(&r.store, &changes, true)?)
        } else {
            None
        };
        if let Some(p) = q
            .path
            .as_deref()
            .map(normalize_path)
            .filter(|p| !p.is_empty())
        {
            changes.retain(|c| c.path == p || c.path.starts_with(&format!("{p}/")));
        }
        let patch = if q.patch {
            Some(unified_patch(&r.store, &changes)?)
        } else {
            None
        };
        Ok(Json(DiffResponse {
            base,
            head,
            changes,
            copies,
            patch,
        }))
    })
    .await
}

#[derive(Deserialize)]
struct PathQ {
    rev: Option<String>,
    path: Option<String>,
}

async fn blame(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<PathQ>,
) -> ApiResult<Json<BlameResponse>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let rev = st.resolve(&r, q.rev.as_deref().unwrap_or("HEAD"))?;
        let path = q.path.unwrap_or_default();
        r.blame_response(&rev, &path)?
            .map(Json)
            .ok_or_else(|| not_found(format!("no file {path:?} at {rev}")))
    })
    .await
}

async fn tree(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<PathQ>,
) -> ApiResult<Json<TreeResponse>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let rev = st.resolve(&r, q.rev.as_deref().unwrap_or("HEAD"))?;
        let path = q.path.unwrap_or_default();
        r.tree_listing(&rev, &path)?
            .map(Json)
            .ok_or_else(|| not_found(format!("no directory {path:?} at {rev}")))
    })
    .await
}

async fn file(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<PathQ>,
) -> ApiResult<Response> {
    let r = repo(&st, &name)?;
    let data: Bytes = blocking(move || {
        let rev = st.resolve(&r, q.rev.as_deref().unwrap_or("HEAD"))?;
        let path = q.path.unwrap_or_default();
        match r.entry(&rev, &path)? {
            Some(e) if !e.is_tree() && !e.is_gitlink() => {
                Ok(Bytes::from(r.store.read_blob(&e.oid)?))
            }
            _ => Err(not_found(format!("no file {path:?} at {rev}"))),
        }
    })
    .await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], data).into_response())
}

#[derive(Deserialize)]
struct GrepQ {
    rev: Option<String>,
    pattern: String,
    path: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    limit: Option<usize>,
}

async fn grep(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<GrepQ>,
) -> ApiResult<Json<wren_core::grep::GrepResponse>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let rev = st.resolve(&r, q.rev.as_deref().unwrap_or("HEAD"))?;
        let limit = q.limit.unwrap_or(100).clamp(1, 10_000);
        Ok(Json(r.grep(
            &rev,
            &q.pattern,
            q.path.as_deref().unwrap_or(""),
            q.ignore_case,
            limit,
        )?))
    })
    .await
}

async fn sync(
    State(st): St,
    Path(name): Path<String>,
) -> ApiResult<Json<wren_core::repo::SyncReport>> {
    let r = repo(&st, &name)?;
    blocking(move || Ok(Json(r.sync()?))).await
}

#[derive(Deserialize)]
struct PathsQ {
    rev: Option<String>,
    q: String,
    limit: Option<usize>,
}

#[derive(serde::Serialize)]
struct PathsResponse {
    paths: Vec<wren_core::pathindex::FoundPath>,
}

/// Paths at a revision whose name contains a substring (case-insensitive).
async fn find_paths(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<PathsQ>,
) -> ApiResult<Json<PathsResponse>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let rev = st.resolve(&r, q.rev.as_deref().unwrap_or("HEAD"))?;
        let paths = r.find_paths(&rev, &q.q, q.limit.unwrap_or(100).min(10_000))?;
        Ok(Json(PathsResponse { paths }))
    })
    .await
}

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default)]
    after: i64,
    limit: Option<usize>,
}

#[derive(serde::Serialize)]
struct AuditItem {
    seq: i64,
    #[serde(flatten)]
    entry: wren_core::write::AuditEntry,
}

/// Every ref move (write API, land, push), oldest first.
async fn audit(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Json<Vec<AuditItem>>> {
    let r = repo(&st, &name)?;
    let limit = q.limit.unwrap_or(1000).min(10_000);
    blocking(move || {
        Ok(Json(
            r.audit(q.after, limit)?
                .into_iter()
                .map(|(seq, entry)| AuditItem { seq, entry })
                .collect(),
        ))
    })
    .await
}

/// While Wren is the source of truth, keep each repository's git host up
/// to date (see `Repo::export`).
pub fn spawn_exporters(state: Arc<ServerState>, every: Duration) {
    for repo in state.repos.values().cloned() {
        if repo.upstream.is_none() {
            continue;
        }
        tokio::spawn(async move {
            loop {
                let r = repo.clone();
                match tokio::task::spawn_blocking(move || r.export()).await {
                    Ok(Ok(rep)) if !rep.errors.is_empty() => {
                        tracing::error!(repo = %repo.name, errors = ?rep.errors, "export to the git host")
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => tracing::warn!(repo = %repo.name, "export failed: {e:#}"),
                    Err(e) => tracing::warn!(repo = %repo.name, "export panicked: {e}"),
                }
                tokio::time::sleep(every).await;
            }
        });
    }
}

// ---- snapshots ------------------------------------------------------------------

fn snapshot_dir(st: &ServerState) -> ApiResult<std::path::PathBuf> {
    st.snapshots.dir.clone().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "this server does not publish snapshots".into(),
        )
    })
}

/// The repository's current snapshot. Not pinned: it changes as the
/// default branch moves, so it is never cached.
async fn snapshot_latest(
    State(st): St,
    Path(name): Path<String>,
) -> ApiResult<Json<wren_core::snapshot::SnapshotInfo>> {
    let r = repo(&st, &name)?;
    snapshot_dir(&st)?;
    blocking(move || {
        wren_core::snapshot::current(&r)?
            .map(Json)
            .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no snapshot yet".into()))
    })
    .await
}

/// Build the repository's snapshot now (write token).
pub(crate) async fn snapshot_build(
    st: Arc<ServerState>,
    name: String,
) -> ApiResult<Json<wren_core::snapshot::SnapshotInfo>> {
    let r = repo(&st, &name)?;
    let dir = snapshot_dir(&st)?;
    let cone = st.snapshots.cone.clone();
    blocking(move || Ok(Json(wren_core::snapshot::publish(&r, "HEAD", &cone, &dir)?))).await
}

/// A snapshot file, by content hash: immutable, served with ranges so
/// that downloads resume.
async fn snapshot_file(
    State(st): St,
    Path(file): Path<String>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Response> {
    let dir = snapshot_dir(&st)?;
    let st2 = st.clone();
    let path = blocking(move || {
        for r in st2.repos.values() {
            if let Some(p) = wren_core::snapshot::locate(r, &dir, &file)? {
                return Ok(p);
            }
        }
        Err(ApiError(StatusCode::NOT_FOUND, "no such snapshot".into()))
    })
    .await?;
    Ok(serve_file(&path, &headers, st.private()).await?)
}

/// Serve an immutable file, honouring `Range: bytes=a-` and `bytes=a-b`.
pub async fn serve_file(
    path: &std::path::Path,
    headers: &axum::http::HeaderMap,
    private: bool,
) -> Result<Response> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::open(path).await?;
    let len = f.metadata().await?.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split_once('-'))
        .and_then(|(a, b)| {
            let a: u64 = a.parse().ok()?;
            let b: u64 = if b.is_empty() {
                len.checked_sub(1)?
            } else {
                b.parse::<u64>().ok()?.min(len.checked_sub(1)?)
            };
            (a <= b).then_some((a, b))
        });
    let (status, start, end) = match range {
        Some((a, b)) => (StatusCode::PARTIAL_CONTENT, a, b + 1),
        None => (StatusCode::OK, 0, len),
    };
    f.seek(std::io::SeekFrom::Start(start)).await?;
    let mut body = vec![0u8; (end - start) as usize];
    f.read_exact(&mut body).await?;
    let mut resp = (
        status,
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, immutable(private)),
            (header::ACCEPT_RANGES, "bytes"),
        ],
        body,
    )
        .into_response();
    if status == StatusCode::PARTIAL_CONTENT {
        if let Ok(v) = format!("bytes {start}-{}/{len}", end - 1).parse() {
            resp.headers_mut().insert(header::CONTENT_RANGE, v);
        }
    }
    Ok(resp)
}

/// Rebuild each repository's snapshot now and then every `every`.
pub fn spawn_snapshotters(state: Arc<ServerState>) {
    let (Some(dir), Some(every)) = (state.snapshots.dir.clone(), state.snapshots.every) else {
        return;
    };
    for repo in state.repos.values().cloned() {
        let (dir, cone) = (dir.clone(), state.snapshots.cone.clone());
        tokio::spawn(async move {
            loop {
                let (r, d, c) = (repo.clone(), dir.clone(), cone.clone());
                match tokio::task::spawn_blocking(move || {
                    wren_core::snapshot::publish(&r, "HEAD", &c, &d)
                })
                .await
                {
                    Ok(Ok(info)) => tracing::debug!(repo = %repo.name, ?info, "snapshot"),
                    Ok(Err(e)) => tracing::warn!(repo = %repo.name, "snapshot failed: {e:#}"),
                    Err(e) => tracing::warn!(repo = %repo.name, "snapshot panicked: {e}"),
                }
                tokio::time::sleep(every).await;
            }
        });
    }
}
