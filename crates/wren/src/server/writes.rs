//! The write API (`write.v1`): upload blobs, create changesets, land, merge,
//! move branches and revert, all on the server (see `wren_core::write` and
//! `wren_core::merge`).
//!
//! Writes need the write token. Every answer is `Cache-Control: no-store`.
//! While a git host is the source of truth, ref moves go to the host first
//! (`git push --force-with-lease`) and Wren's ref follows.

use std::sync::Arc;

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wren_core::merge::{MergeRequest, Merged};
use wren_core::object::{Kind, MODE_BLOB, MODE_EXEC, MODE_GITLINK, MODE_SYMLINK};
use wren_core::write::{signature, FileEdit, Landed, NewChangeset, RefUpdate};
use wren_core::{bad_request, changes::FileVersion, Oid, Repo};

use super::{blocking, constant_time_eq, repo, ApiError, ApiResult, ServerState, St};

/// Largest request body a write endpoint accepts.
pub const MAX_WRITE_BODY: usize = 64 << 20;

pub fn routes(state: Arc<ServerState>) -> Router<Arc<ServerState>> {
    Router::new()
        .route("/api/v1/repos/{repo}/blobs", post(blobs))
        .route("/api/v1/repos/{repo}/changesets", post(changesets))
        .route("/api/v1/repos/{repo}/land", post(land))
        .route("/api/v1/repos/{repo}/merge", post(merge))
        .route("/api/v1/repos/{repo}/refs", post(update_ref))
        .route("/api/v1/repos/{repo}/revert", post(revert))
        .route(
            "/api/v1/repos/{repo}/snapshot",
            post(|State(st): St, Path(name): Path<String>| super::snapshot_build(st, name)),
        )
        .layer(axum::extract::DefaultBodyLimit::max(MAX_WRITE_BODY))
        .layer(axum::middleware::from_fn_with_state(
            state,
            require_write_token,
        ))
        .layer(axum::middleware::map_response(no_store))
}

async fn no_store(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

pub(super) fn bearer(req: &Request) -> Option<&str> {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

async fn require_write_token(
    State(st): St,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(wt) = &st.write_token else {
        return ApiError(
            StatusCode::FORBIDDEN,
            "writes are disabled on this server; start it with --write-token \
             (or WREN_WRITE_TOKEN) to enable them"
                .into(),
        )
        .into_response();
    };
    let is = |got: &str, t: &str| constant_time_eq(got.as_bytes(), t.as_bytes());
    match bearer(&req) {
        Some(got) if is(got, wt) => next.run(req).await,
        Some(got) if st.token.as_deref().is_some_and(|t| is(got, t)) => ApiError(
            StatusCode::FORBIDDEN,
            "the read token cannot write; use the write token".into(),
        )
        .into_response(),
        Some(_) => {
            ApiError(StatusCode::UNAUTHORIZED, "invalid bearer token".into()).into_response()
        }
        None => ApiError(
            StatusCode::UNAUTHORIZED,
            "writes need Authorization: Bearer <write-token>".into(),
        )
        .into_response(),
    }
}

/// A JSON request body whose rejections are `ErrorBody`s like every other
/// error (axum's own are plain text).
struct Body<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|e| ApiError(e.status(), e.body_text()))?;
        serde_json::from_slice(&bytes).map(Body).map_err(|e| {
            ApiError(
                StatusCode::BAD_REQUEST,
                format!("invalid request body: {e}"),
            )
        })
    }
}

// ---- request and response shapes ---------------------------------------------

/// An author or committer.
#[derive(Deserialize)]
struct Person {
    name: String,
    email: String,
    /// Unix seconds; default now.
    time: Option<i64>,
    /// `+hhmm` / `-hhmm`; default `+0000`.
    tz: Option<String>,
}

impl Person {
    fn signature(&self) -> Result<Vec<u8>> {
        let time = self.time.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
        signature(
            &self.name,
            &self.email,
            time,
            self.tz.as_deref().unwrap_or("+0000"),
        )
    }
}

/// File content given inline: UTF-8 text or base64.
#[derive(Deserialize)]
struct Content {
    content: Option<String>,
    content_base64: Option<String>,
}

impl Content {
    fn given(&self) -> usize {
        self.content.is_some() as usize + self.content_base64.is_some() as usize
    }

    fn bytes(&self) -> Result<Option<Vec<u8>>> {
        if let Some(t) = &self.content {
            return Ok(Some(t.as_bytes().to_vec()));
        }
        if let Some(b) = &self.content_base64 {
            return base64::engine::general_purpose::STANDARD
                .decode(b.trim())
                .map(Some)
                .map_err(|e| bad_request(format!("invalid content_base64: {e}")));
        }
        Ok(None)
    }
}

#[derive(Deserialize)]
struct BlobsReq {
    blobs: Vec<Content>,
}

#[derive(Serialize)]
struct BlobsResp {
    oids: Vec<Oid>,
}

#[derive(Deserialize)]
struct ChangeReq {
    path: String,
    #[serde(flatten)]
    content: Content,
    blob: Option<String>,
    #[serde(default)]
    delete: bool,
    /// Octal string: 100644, 100755, 120000 or 160000.
    mode: Option<String>,
}

#[derive(Deserialize)]
struct LandOpt {
    onto: String,
}

#[derive(Deserialize)]
struct ChangesetReq {
    parents: Vec<String>,
    author: Person,
    committer: Option<Person>,
    message: String,
    #[serde(default)]
    changes: Vec<ChangeReq>,
    land: Option<LandOpt>,
}

#[derive(Serialize)]
struct ChangesetResp {
    oid: Oid,
    tree: Oid,
    #[serde(skip_serializing_if = "Option::is_none")]
    land: Option<Landed>,
}

#[derive(Deserialize)]
struct LandReq {
    changesets: Vec<String>,
    onto: String,
    committer: Option<Person>,
}

#[derive(Deserialize)]
struct MergeReq {
    /// The revision to merge.
    commit: String,
    into: String,
    author: Person,
    committer: Option<Person>,
    message: Option<String>,
    #[serde(default)]
    fast_forward: bool,
}

#[derive(Deserialize)]
struct RefReq {
    name: String,
    expected: Option<String>,
    new: Option<String>,
}

#[derive(Deserialize)]
struct RevertReq {
    commit: String,
    author: Person,
    committer: Option<Person>,
    onto: Option<String>,
}

#[derive(Serialize)]
struct RevertResp {
    oid: Oid,
    #[serde(skip_serializing_if = "Option::is_none")]
    land: Option<Landed>,
}

// ---- helpers --------------------------------------------------------------------

fn land_status(l: &Landed) -> StatusCode {
    match l {
        Landed::Landed { .. } => StatusCode::OK,
        Landed::Conflicts { .. } | Landed::Rejected { .. } => StatusCode::CONFLICT,
    }
}

/// `land` gives up (a plain error) when the branch kept moving through all
/// its attempts: that is contention, not a server fault.
fn land_stack(r: &Repo, stack: &[Oid], onto: &str, committer: Option<&[u8]>) -> ApiResult<Landed> {
    r.land(stack, onto, committer).map_err(contention)
}

/// Giving up because the branch kept moving is a conflict (`409`).
fn contention(e: anyhow::Error) -> ApiError {
    let typed = e
        .chain()
        .any(|c| c.is::<wren_core::NotFound>() || c.is::<wren_core::BadRequest>());
    if !typed && format!("{e:#}").contains("kept moving") {
        ApiError(StatusCode::CONFLICT, format!("{e:#}"))
    } else {
        e.into()
    }
}

fn parse_oid(what: &str, s: &str) -> Result<Oid> {
    s.parse()
        .map_err(|e| bad_request(format!("{what}: expected a full commit id: {e}")))
}

fn parse_mode(path: &str, m: &str) -> Result<u32> {
    match u32::from_str_radix(m, 8) {
        Ok(v) if [MODE_BLOB, MODE_EXEC, MODE_SYMLINK, MODE_GITLINK].contains(&v) => Ok(v),
        _ => Err(bad_request(format!(
            "{path}: bad mode {m:?}; use 100644, 100755, 120000 or 160000"
        ))),
    }
}

/// Turn requested changes into edits, storing inline contents as blobs.
fn edits(r: &Repo, p1: &Oid, changes: &[ChangeReq]) -> Result<Vec<FileEdit>> {
    let mut out = Vec::with_capacity(changes.len());
    for c in changes {
        let given = c.content.given() + c.blob.is_some() as usize + c.delete as usize;
        if given != 1 {
            return Err(bad_request(format!(
                "{}: give exactly one of content, content_base64, blob or delete",
                c.path
            )));
        }
        if c.delete {
            if c.mode.is_some() {
                return Err(bad_request(format!("{}: a deletion has no mode", c.path)));
            }
            out.push(FileEdit {
                path: c.path.clone(),
                new: None,
            });
            continue;
        }
        let mode = match &c.mode {
            Some(m) => parse_mode(&c.path, m)?,
            // Keep the mode of the file being replaced (an executable stays
            // executable); new files are regular.
            None => match r.entry(p1, &c.path) {
                Ok(Some(e)) if !e.is_tree() && !e.is_gitlink() => e.mode,
                _ => MODE_BLOB,
            },
        };
        let oid = match (&c.blob, c.content.bytes()?) {
            (Some(b), _) => parse_oid(&c.path, b)?,
            (None, Some(_)) if mode == MODE_GITLINK => {
                return Err(bad_request(format!(
                    "{}: a submodule (160000) takes a commit id in \"blob\"",
                    c.path
                )))
            }
            (None, Some(bytes)) => r.store.put(Kind::Blob, &bytes)?,
            (None, None) => unreachable!("checked above"),
        };
        out.push(FileEdit {
            path: c.path.clone(),
            new: Some(FileVersion { mode, oid }),
        });
    }
    Ok(out)
}

// ---- handlers -------------------------------------------------------------------

async fn blobs(
    State(st): St,
    Path(name): Path<String>,
    Body(req): Body<BlobsReq>,
) -> ApiResult<Json<BlobsResp>> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let mut oids = Vec::with_capacity(req.blobs.len());
        for b in &req.blobs {
            if b.given() != 1 {
                return Err(bad_request(
                    "each blob needs exactly one of content or content_base64",
                )
                .into());
            }
            let bytes = b.bytes()?.unwrap_or_default();
            oids.push(r.store.put(Kind::Blob, &bytes)?);
        }
        // Any server may be asked to use them next.
        r.store.publish()?;
        Ok(Json(BlobsResp { oids }))
    })
    .await
}

async fn changesets(
    State(st): St,
    Path(name): Path<String>,
    Body(req): Body<ChangesetReq>,
) -> ApiResult<Response> {
    let r = repo(&st, &name)?;
    blocking(move || {
        if req.parents.is_empty() {
            return Err(bad_request("a changeset needs at least one parent").into());
        }
        let parents = req
            .parents
            .iter()
            .map(|p| st.resolve(&r, p))
            .collect::<Result<Vec<_>>>()?;
        let author = req.author.signature()?;
        let committer = req.committer.as_ref().map(Person::signature).transpose()?;
        let edits = edits(&r, &parents[0], &req.changes)?;
        let oid = r.create_changeset(&NewChangeset {
            parents,
            author,
            committer,
            message: req.message.into_bytes(),
            edits,
        })?;
        let tree = r.tree_of(&oid)?;
        let Some(l) = req.land else {
            let body = ChangesetResp {
                oid,
                tree,
                land: None,
            };
            return Ok((StatusCode::CREATED, Json(body)).into_response());
        };
        let landed = land_stack(&r, &[oid], &l.onto, None)?;
        let status = land_status(&landed);
        let body = ChangesetResp {
            oid,
            tree,
            land: Some(landed),
        };
        Ok((status, Json(body)).into_response())
    })
    .await
}

async fn land(
    State(st): St,
    Path(name): Path<String>,
    Body(req): Body<LandReq>,
) -> ApiResult<Response> {
    let r = repo(&st, &name)?;
    blocking(move || {
        if req.changesets.is_empty() {
            return Err(bad_request("nothing to land").into());
        }
        let stack = req
            .changesets
            .iter()
            .map(|c| st.resolve(&r, c))
            .collect::<Result<Vec<_>>>()?;
        let committer = req.committer.as_ref().map(Person::signature).transpose()?;
        let landed = land_stack(&r, &stack, &req.onto, committer.as_deref())?;
        Ok((land_status(&landed), Json(landed)).into_response())
    })
    .await
}

async fn merge(
    State(st): St,
    Path(name): Path<String>,
    Body(req): Body<MergeReq>,
) -> ApiResult<Response> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let commit = st.resolve(&r, &req.commit)?;
        let merged = r
            .merge_into(&MergeRequest {
                into: req.into.clone(),
                commit,
                author: req.author.signature()?,
                committer: req.committer.as_ref().map(Person::signature).transpose()?,
                message: req.message.map(String::into_bytes),
                fast_forward: req.fast_forward,
            })
            .map_err(contention)?;
        let status = match merged {
            Merged::Merged { .. } | Merged::UpToDate { .. } => StatusCode::OK,
            Merged::Conflicts { .. } | Merged::Rejected { .. } => StatusCode::CONFLICT,
        };
        Ok((status, Json(merged)).into_response())
    })
    .await
}

async fn update_ref(
    State(st): St,
    Path(name): Path<String>,
    Body(req): Body<RefReq>,
) -> ApiResult<Response> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let refname = if req.name.starts_with("refs/") {
            req.name.clone()
        } else {
            format!("refs/heads/{}", req.name)
        };
        let expected = req
            .expected
            .as_deref()
            .map(|s| parse_oid("expected", s))
            .transpose()?;
        let new = match req.new.as_deref() {
            // Resolving a full id syncs on demand if it was just pushed to
            // the host.
            Some(s) => Some(st.resolve(&r, &parse_oid("new", s)?.to_hex())?),
            None => None,
        };
        let u = r.update_ref(&refname, expected, new)?;
        let status = match u {
            RefUpdate::Updated => StatusCode::OK,
            RefUpdate::Stale { .. } | RefUpdate::Rejected { .. } => StatusCode::CONFLICT,
        };
        Ok((status, Json(u)).into_response())
    })
    .await
}

async fn revert(
    State(st): St,
    Path(name): Path<String>,
    Body(req): Body<RevertReq>,
) -> ApiResult<Response> {
    let r = repo(&st, &name)?;
    blocking(move || {
        let commit = st.resolve(&r, &req.commit)?;
        let author = req.author.signature()?;
        let committer = req.committer.as_ref().map(Person::signature).transpose()?;
        let oid = r.revert(&commit, &author, committer.as_deref())?;
        let Some(onto) = &req.onto else {
            let body = RevertResp { oid, land: None };
            return Ok((StatusCode::CREATED, Json(body)).into_response());
        };
        let landed = land_stack(&r, &[oid], onto, None)?;
        let status = land_status(&landed);
        Ok((
            status,
            Json(RevertResp {
                oid,
                land: Some(landed),
            }),
        )
            .into_response())
    })
    .await
}
