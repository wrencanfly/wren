//! Directory-level access control for queries, and per-client rate limits.
//!
//! **ACLs.** `--acl FILE` adds read tokens restricted to some repositories
//! and, within them, to some directories:
//!
//! ```json
//! {"tokens": [{"name": "contractor", "token": "…", "repos": {"demo": ["docs", "src/public"]}},
//!             {"name": "docs-bot", "token": "…", "repos": {"*": ["docs"]}}]}
//! ```
//!
//! (`""` grants a whole repository.) A restricted token can use the query
//! API only, and sees only what its directories allow:
//!
//! * other repositories do not exist (404);
//! * `file`, `blame` and `log` need a path inside a granted directory
//!   (`log` cannot list the whole history);
//! * `tree` lists granted directories and the directories leading to them;
//! * `diff`, `commits/{rev}`, `log` (with changes), `grep` and `paths`
//!   answers are filtered to granted paths, and marked `no-store`.
//!
//! The object plane (`/v1/objects`, snapshots), the git front end, writes
//! and the audit log refuse restricted tokens: objects are addressed by
//! hash, and a tier cannot tell which directory an object belongs to (a
//! commit's root tree names every object). Caches are one trust domain:
//! restricted tokens talk to the server.
//!
//! Commit metadata (messages, authors) of commits that touched granted
//! paths stays visible.
//!
//! **Rate limits.** `--rate-limit N` gives each client (each token, or each
//! IP address without one) a token bucket of N requests per second with a
//! burst of 2N; beyond it requests get `429` with `Retry-After`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::Value;
use wren_core::changes::normalize_path;

use super::{constant_time_eq, ApiError, ServerState};

#[derive(Clone, Debug, Deserialize)]
pub struct AclEntry {
    pub name: String,
    pub token: String,
    /// Repository (or `*`) -> granted directories (`""`: everything).
    pub repos: HashMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AclFile {
    pub tokens: Vec<AclEntry>,
}

/// What a restricted token may see (in request extensions).
#[derive(Clone, Debug)]
pub struct Grant {
    pub name: String,
    repos: HashMap<String, Vec<String>>,
}

impl Grant {
    fn dirs(&self, repo: &str) -> Option<&Vec<String>> {
        self.repos.get(repo).or_else(|| self.repos.get("*"))
    }

    pub fn has_repo(&self, repo: &str) -> bool {
        self.dirs(repo).is_some()
    }

    /// `path` is inside a granted directory.
    pub fn allows(&self, repo: &str, path: &str) -> bool {
        let path = normalize_path(path);
        self.dirs(repo).is_some_and(|ds| {
            ds.iter().any(|d| {
                let d = normalize_path(d);
                d.is_empty() || path == d || path.starts_with(&format!("{d}/"))
            })
        })
    }

    /// `dir` is granted, or leads to a granted directory.
    pub fn sees_dir(&self, repo: &str, dir: &str) -> bool {
        let dir = normalize_path(dir);
        self.allows(repo, &dir)
            || self.dirs(repo).is_some_and(|ds| {
                ds.iter()
                    .any(|d| dir.is_empty() || normalize_path(d).starts_with(&format!("{dir}/")))
            })
    }
}

impl ServerState {
    /// The grant of a restricted token, if `token` is one.
    pub fn grant_for(&self, token: &str) -> Option<Grant> {
        self.acl
            .iter()
            .find(|e| constant_time_eq(e.token.as_bytes(), token.as_bytes()))
            .map(|e| Grant {
                name: e.name.clone(),
                repos: e.repos.clone(),
            })
    }
}

fn deny(code: StatusCode, msg: &str) -> Response {
    ApiError(code, msg.to_string()).into_response()
}

/// `/api/v1/repos/{repo}/{endpoint}` -> (repo, endpoint)
fn api_route(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/api/v1/repos/")?;
    let (repo, endpoint) = rest.split_once('/')?;
    Some((repo, endpoint))
}

fn query_param(req: &Request, key: &str) -> Option<String> {
    let q = req.uri().query()?;
    url::form_urlencoded::parse(q.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Enforce a restricted token's grant (see the module docs). Runs after
/// authentication, which put the `Grant` in the request's extensions.
pub async fn enforce(req: Request, next: Next) -> Response {
    let Some(grant) = req.extensions().get::<Grant>().cloned() else {
        return next.run(req).await;
    };
    let path = req.uri().path().to_string();
    let refuse = "this token is restricted to some directories and may only use the query API";
    if path == "/api/v1/repos" {
        let resp = next.run(req).await;
        return filter_json(resp, |v| {
            if let Some(a) = v.as_array_mut() {
                a.retain(|r| r["name"].as_str().is_some_and(|n| grant.has_repo(n)));
            }
        })
        .await;
    }
    let Some((repo, endpoint)) = api_route(&path) else {
        return deny(StatusCode::FORBIDDEN, refuse);
    };
    let repo = repo.to_string();
    if !grant.has_repo(&repo) {
        return deny(StatusCode::NOT_FOUND, &format!("unknown repo {repo}"));
    }
    let qpath = query_param(&req, "path");
    let endpoint = endpoint.to_string();
    match endpoint.as_str() {
        "file" | "blame" => {
            if !qpath.as_deref().is_some_and(|p| grant.allows(&repo, p)) {
                return deny(StatusCode::NOT_FOUND, "no such file");
            }
        }
        "log" => {
            if !qpath.as_deref().is_some_and(|p| grant.allows(&repo, p)) {
                return deny(
                    StatusCode::FORBIDDEN,
                    "a restricted token must give log a path inside its directories",
                );
            }
        }
        "tree" => {
            if !grant.sees_dir(&repo, qpath.as_deref().unwrap_or("")) {
                return deny(StatusCode::NOT_FOUND, "no such directory");
            }
        }
        "diff" | "grep" | "paths" | "refs" | "resolve" | "merge-base" | "is-ancestor"
        | "landed" | "bisect" | "snapshot" => {}
        e if e.starts_with("commits/") => {}
        _ => return deny(StatusCode::FORBIDDEN, refuse),
    }
    let resp = next.run(req).await;
    let dir = normalize_path(qpath.as_deref().unwrap_or(""));
    let g = grant.clone();
    let r2 = repo.clone();
    let allowed = move |p: &Value| p.as_str().is_some_and(|p| g.allows(&r2, p));
    let sees = |p: &str| grant.sees_dir(&repo, p);
    filter_json(resp, |v| match endpoint.as_str() {
        "diff" => filter_changes(v, &allowed),
        e if e.starts_with("commits/") => filter_changes(v, &allowed),
        "log" => {
            for c in v["commits"].as_array_mut().into_iter().flatten() {
                filter_changes(c, &allowed);
            }
        }
        "grep" => {
            if let Some(m) = v["matches"].as_array_mut() {
                m.retain(|x| allowed(&x["path"]));
            }
        }
        "paths" => {
            if let Some(m) = v["paths"].as_array_mut() {
                m.retain(|x| allowed(&x["path"]) || x["path"].as_str().is_some_and(sees));
            }
        }
        "tree" => {
            if let Some(m) = v["entries"].as_array_mut() {
                m.retain(|x| {
                    let name = x["name"].as_str().unwrap_or("");
                    let full = if dir.is_empty() {
                        name.to_string()
                    } else {
                        format!("{dir}/{name}")
                    };
                    allowed(&Value::String(full.clone())) || sees(&full)
                });
            }
        }
        _ => {}
    })
    .await
}

fn filter_changes(v: &mut Value, allowed: &impl Fn(&Value) -> bool) {
    if let Some(c) = v["changes"].as_array_mut() {
        c.retain(|x| allowed(&x["path"]));
    }
    if let Some(c) = v["copies"].as_array_mut() {
        c.retain(|x| allowed(&x["from"]) && allowed(&x["to"]));
    }
}

/// Rewrite a JSON response body with `f` and mark it `no-store` (it is
/// specific to the token that asked).
async fn filter_json(resp: Response, f: impl FnOnce(&mut Value)) -> Response {
    let (mut parts, body) = resp.into_parts();
    parts
        .headers
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if !parts.status.is_success() {
        return Response::from_parts(parts, body);
    }
    let Ok(bytes) = axum::body::to_bytes(body, 256 << 20).await else {
        return deny(
            StatusCode::INTERNAL_SERVER_ERROR,
            "response too large to filter",
        );
    };
    let Ok(mut v) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    f(&mut v);
    let out = serde_json::to_vec(&v).unwrap_or_default();
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(out))
}

// ---- rate limits ------------------------------------------------------------------

pub struct RateLimit {
    pub per_second: f64,
    pub burst: f64,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl RateLimit {
    pub fn new(per_second: f64) -> RateLimit {
        RateLimit {
            per_second,
            burst: (per_second * 2.0).max(1.0),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Take one request from `key`'s bucket; `Err(seconds to wait)` if empty.
    fn take(&self, key: &str) -> Result<(), u64> {
        let mut b = self.buckets.lock().unwrap();
        if b.len() > 100_000 {
            // Forget idle clients rather than grow without bound.
            let now = Instant::now();
            b.retain(|_, (_, t)| now.duration_since(*t).as_secs() < 60);
        }
        let now = Instant::now();
        let (tokens, last) = b.entry(key.to_string()).or_insert((self.burst, now));
        *tokens =
            (*tokens + now.duration_since(*last).as_secs_f64() * self.per_second).min(self.burst);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - *tokens) / self.per_second).ceil().max(1.0) as u64)
        }
    }
}

/// Per-client token buckets: keyed by the credential presented, or by the
/// client's address when there is none.
pub async fn rate_limit(State(st): State<Arc<ServerState>>, req: Request, next: Next) -> Response {
    let Some(limit) = &st.rate_limit else {
        return next.run(req).await;
    };
    let key = match req.headers().get(header::AUTHORIZATION) {
        Some(v) => {
            use sha1::{Digest, Sha1};
            format!("auth:{}", hex::encode(Sha1::digest(v.as_bytes())))
        }
        None => match req.extensions().get::<ConnectInfo<SocketAddr>>() {
            Some(ConnectInfo(a)) => format!("ip:{}", a.ip()),
            None => "anonymous".into(),
        },
    };
    match limit.take(&key) {
        Ok(()) => next.run(req).await,
        Err(wait) => (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, wait.to_string())],
            axum::Json(serde_json::json!({
                "error": "rate limit exceeded",
                "code": "rate_limited",
            })),
        )
            .into_response(),
    }
}
