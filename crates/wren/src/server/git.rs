//! git's smart HTTP protocol (see `wren_core::gitproto`): stock git clones,
//! fetches (protocol v2, partial clones included) and pushes at
//! `/git/{repo}` (a trailing `.git` is accepted).
//!
//! git authenticates with HTTP Basic: any user name, and a token as the
//! password (the read or write token to fetch, the write token to push).
//! Bearer tokens work too. A request without credentials gets a `401` with
//! a Basic challenge, which makes git ask its credential helper.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::Engine as _;
use serde::Deserialize;
use wren_core::gitproto;

use super::{blocking, constant_time_eq, repo, ApiError, ApiResult, ServerState};

type St = State<Arc<ServerState>>;

pub fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/git/{repo}/info/refs", get(info_refs))
        .route("/git/{repo}/git-upload-pack", post(upload_pack))
        .route("/git/{repo}/git-receive-pack", post(receive_pack))
}

/// The token a git request presents: a Basic password or a Bearer token.
fn presented(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if let Some(b) = v.strip_prefix("Bearer ") {
        return Some(b.to_string());
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(v.strip_prefix("Basic ")?)
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    Some(s.split_once(':').map_or(s.clone(), |(_, p)| p.to_string()))
}

fn challenge(msg: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"wren\"")],
        msg.to_string(),
    )
        .into_response()
}

/// `Ok` if the request may read (or, with `write`, push).
#[allow(clippy::result_large_err)]
fn authorize(st: &ServerState, headers: &HeaderMap, write: bool) -> Result<(), Response> {
    let got = presented(headers);
    let is = |t: &Option<String>| match (&got, t) {
        (Some(g), Some(t)) => constant_time_eq(g.as_bytes(), t.as_bytes()),
        _ => false,
    };
    if write {
        if st.write_token.is_none() {
            return Err((
                StatusCode::FORBIDDEN,
                "pushes are disabled on this server (no --write-token)",
            )
                .into_response());
        }
        if is(&st.write_token) {
            return Ok(());
        }
        if got.is_some() && is(&st.token) {
            return Err((
                StatusCode::FORBIDDEN,
                "the read token cannot push; use the write token",
            )
                .into_response());
        }
        return Err(challenge("pushing needs the write token as the password"));
    }
    if st.token.is_none() || is(&st.token) || is(&st.write_token) {
        Ok(())
    } else {
        Err(challenge("fetching needs a token as the password"))
    }
}

fn repo_name(name: &str) -> &str {
    name.strip_suffix(".git").unwrap_or(name)
}

#[derive(Deserialize)]
struct Service {
    service: Option<String>,
}

fn no_cache(ctype: &str, body: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, ctype),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn info_refs(
    State(st): St,
    Path(name): Path<String>,
    Query(q): Query<Service>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let service = q.service.unwrap_or_default();
    let write = match service.as_str() {
        "git-upload-pack" => false,
        "git-receive-pack" => true,
        _ => {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "only smart HTTP is served (git ≥ 2.26)".into(),
            ))
        }
    };
    if let Err(r) = authorize(&st, &headers, write) {
        return Ok(r);
    }
    let r = repo(&st, repo_name(&name))?;
    if write {
        let body = blocking(move || Ok(gitproto::receive_pack_advertisement(&r)?)).await?;
        return Ok(no_cache(
            "application/x-git-receive-pack-advertisement",
            body,
        ));
    }
    let v2 = headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(':').any(|p| p == "version=2"));
    if !v2 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "fetching needs git protocol version 2 (git ≥ 2.26, or -c protocol.version=2)".into(),
        ));
    }
    Ok(no_cache(
        "application/x-git-upload-pack-advertisement",
        gitproto::upload_pack_advertisement(),
    ))
}

/// The request body, gunzipped if git compressed it.
fn body_bytes(headers: &HeaderMap, body: Bytes) -> Result<Vec<u8>, ApiError> {
    let gz = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("gzip"));
    if !gz {
        return Ok(body.to_vec());
    }
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&body[..]), &mut out)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("bad gzip body: {e}")))?;
    Ok(out)
}

async fn upload_pack(
    State(st): St,
    Path(name): Path<String>,
    headers: HeaderMap,
    req: Request,
) -> ApiResult<Response> {
    if let Err(r) = authorize(&st, &headers, false) {
        return Ok(r);
    }
    let r = repo(&st, repo_name(&name))?;
    let body = axum::body::to_bytes(req.into_body(), 64 << 20)
        .await
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e}")))?;
    let body = body_bytes(&headers, body)?;
    let out = blocking(move || Ok(gitproto::upload_pack(&r, &body)?)).await?;
    Ok(no_cache("application/x-git-upload-pack-result", out))
}

async fn receive_pack(
    State(st): St,
    Path(name): Path<String>,
    headers: HeaderMap,
    req: Request,
) -> ApiResult<Response> {
    if let Err(r) = authorize(&st, &headers, true) {
        return Ok(r);
    }
    let r = repo(&st, repo_name(&name))?;
    let body = axum::body::to_bytes(req.into_body(), 2 << 30)
        .await
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e}")))?;
    let body = body_bytes(&headers, body)?;
    let out = blocking(move || Ok(gitproto::receive_pack(&r, &body)?)).await?;
    Ok(no_cache("application/x-git-receive-pack-result", out))
}
