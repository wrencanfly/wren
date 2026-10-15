//! HTTP client for the query API, the object protocol and the write API.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use futures::stream::{self, StreamExt, TryStreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wren_core::api::*;
use wren_core::wire::{self, FrameDecoder, ObjectsRequest, Payload};
use wren_core::write::Landed;
use wren_core::{ObjectStore, Oid};

pub const BATCH: usize = 1000;
pub const PARALLEL: usize = 16;

#[derive(Clone)]
pub struct Client {
    pub server: String,
    pub repo: String,
    http: reqwest::Client,
    /// The object plane, when it is not `server` (WREN_OBJECTS): usually
    /// the node's daemon. Queries and writes still go to `server`.
    objects: Option<(reqwest::Client, String)>,
    /// The object plane's own store, if it said (a node daemon does).
    objects_store: Arc<tokio::sync::OnceCell<Option<PathBuf>>>,
    pub stats: Arc<FetchStats>,
}

#[derive(Default)]
pub struct FetchStats {
    pub objects: AtomicU64,
    pub bytes: AtomicU64,
    pub requests: AtomicU64,
    /// Objects that arrived as deltas against a base the client named.
    pub deltas: AtomicU64,
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
        let objects = std::env::var("WREN_OBJECTS")
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| {
                let b = reqwest::Client::builder()
                    .pool_max_idle_per_host(PARALLEL)
                    .default_headers(headers.clone())
                    .timeout(std::time::Duration::from_secs(300));
                match v.strip_prefix("unix:") {
                    Some(path) => (
                        b.unix_socket(path).build().unwrap(),
                        "http://wren-daemon".to_string(),
                    ),
                    None => (b.build().unwrap(), v.trim_end_matches('/').to_string()),
                }
            });
        Client {
            server: server.trim_end_matches('/').to_string(),
            repo: repo.to_string(),
            http: reqwest::Client::builder()
                .pool_max_idle_per_host(PARALLEL)
                .default_headers(headers)
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .unwrap(),
            objects,
            objects_store: Arc::default(),
            stats: Arc::default(),
        }
    }

    /// Whether objects fetched into `store` are written by the object plane
    /// itself: a node daemon serving this very store. The client then only
    /// verifies what it receives.
    async fn daemon_owns(&self, store: &ObjectStore) -> bool {
        let Some((http, base)) = &self.objects else {
            return false;
        };
        let theirs = self
            .objects_store
            .get_or_init(|| async {
                let caps: Capabilities = http
                    .get(format!("{base}/v1/capabilities"))
                    .send()
                    .await
                    .ok()?
                    .json()
                    .await
                    .ok()?;
                caps.store.map(PathBuf::from)
            })
            .await;
        let canon = |p: &std::path::Path| std::fs::canonicalize(p).ok();
        theirs
            .as_deref()
            .is_some_and(|t| canon(t).is_some() && canon(t) == canon(store.root()))
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

    /// POST a JSON body to a write endpoint. A 409 carries a meaningful body
    /// (conflicts, a stale ref, a rejection), so it is parsed like a success.
    pub async fn write<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let resp = self
            .http
            .post(self.url(path))
            .json(body)
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.server))?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        parse_write(status, &bytes)
    }

    /// Store blobs on the server; returns their ids in order.
    pub async fn upload_blobs(&self, blobs: &[Content]) -> Result<Vec<Oid>> {
        let r: BlobsResponse = self
            .write(
                "blobs",
                &BlobsRequest {
                    blobs: blobs.to_vec(),
                },
            )
            .await?;
        if r.oids.len() != blobs.len() {
            bail!(
                "server returned {} ids for {} blobs",
                r.oids.len(),
                blobs.len()
            );
        }
        Ok(r.oids)
    }

    pub async fn create_changeset(&self, req: &ChangesetRequest) -> Result<ChangesetResponse> {
        self.write("changesets", req).await
    }

    pub async fn land(&self, req: &LandRequest) -> Result<Landed> {
        self.write("land", req).await
    }

    pub async fn revert(&self, req: &RevertRequest) -> Result<RevertResponse> {
        self.write("revert", req).await
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
        let wants: Vec<(Oid, Option<Oid>)> = oids.iter().map(|o| (*o, None)).collect();
        self.ensure_with_bases(store, &wants).await
    }

    /// `ensure`, naming for each object a similar one this store already
    /// holds (typically the same path in a tree checked out before). The
    /// server may then send a delta against it instead of the whole object.
    /// `WREN_NO_DELTA=1` turns the hints off.
    pub async fn ensure_with_bases(
        &self,
        store: &Arc<ObjectStore>,
        wants: &[(Oid, Option<Oid>)],
    ) -> Result<()> {
        let use_bases = std::env::var_os("WREN_NO_DELTA").is_none();
        let mut missing: Vec<(Oid, Option<Oid>)> = {
            let store = store.clone();
            let wants = wants.to_vec();
            tokio::task::spawn_blocking(move || {
                let mut seen = std::collections::HashSet::new();
                wants
                    .into_iter()
                    .filter(|(o, _)| seen.insert(*o) && !store.has(o))
                    .map(|(o, b)| (o, b.filter(|b| use_bases && b != &o && store.has(b))))
                    .collect()
            })
            .await?
        };
        for attempt in 0..4 {
            if missing.is_empty() {
                return Ok(());
            }
            let results: Vec<Result<()>> = stream::iter(missing.chunks(BATCH).map(|c| c.to_vec()))
                .map(|chunk| {
                    let (oids, bases): (Vec<Oid>, Vec<Option<Oid>>) = chunk.into_iter().unzip();
                    let bases = if bases.iter().any(Option::is_some) {
                        bases
                    } else {
                        Vec::new()
                    };
                    self.fetch_batch(store.clone(), oids, bases)
                })
                .buffer_unordered(PARALLEL)
                .collect()
                .await;
            let errors: Vec<_> = results.into_iter().filter_map(|r| r.err()).collect();
            let store2 = store.clone();
            missing = tokio::task::spawn_blocking(move || {
                missing
                    .into_iter()
                    .filter(|(o, _)| !store2.has(o))
                    .collect()
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
                        missing[0].0
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

    async fn fetch_batch(
        &self,
        store: Arc<ObjectStore>,
        oids: Vec<Oid>,
        bases: Vec<Option<Oid>>,
    ) -> Result<()> {
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        let (http, base) = match &self.objects {
            Some((c, b)) => (c, b.as_str()),
            None => (&self.http, self.server.as_str()),
        };
        let resp = http
            .post(format!("{base}/v1/objects"))
            .json(&ObjectsRequest { oids, bases })
            .send()
            .await
            .with_context(|| format!("connecting to {base}"))?;
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
            pending.iter().filter(|f| f.1 != Payload::Missing).count() as u64,
            Ordering::Relaxed,
        );
        self.stats.deltas.fetch_add(
            pending
                .iter()
                .filter(|f| matches!(f.1, Payload::Delta(_)))
                .count() as u64,
            Ordering::Relaxed,
        );
        // Verify every object against its id before it enters the store
        // (or, when a daemon owns the store, before trusting it is there).
        let owned = self.daemon_owns(&store).await;
        tokio::task::spawn_blocking(move || -> Result<()> {
            use rayon::prelude::*;
            pending
                .par_iter()
                .try_for_each(|(oid, payload)| -> Result<()> {
                    if owned {
                        wire::verify_frame(&store, oid, payload)?;
                    } else {
                        wire::store_frame(&store, oid, payload)?;
                    }
                    Ok(())
                })
        })
        .await??;
        Ok(())
    }
}

// ---- Writes -----------------------------------------------------------------
//
// Request and response bodies of the server-side write API. Revisions are sent
// as given; the server resolves them against its own (authoritative) refs.

/// `Name <email>`, optionally with a time and timezone (the server fills in
/// the current time when they are missing).
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub name: String,
    pub email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
}

impl Person {
    /// Parse `Name <email>`.
    pub fn parse(s: &str) -> Result<Person> {
        let s = s.trim();
        let parsed = s.split_once('<').and_then(|(name, rest)| {
            let email = rest.strip_suffix('>')?;
            let (name, email) = (name.trim(), email.trim());
            let ok = !name.is_empty() && !email.is_empty() && !email.contains(['<', '>']);
            ok.then(|| (name.to_string(), email.to_string()))
        });
        let Some((name, email)) = parsed else {
            bail!("expected \"Name <email>\", got {s:?}");
        };
        Ok(Person {
            name,
            email,
            time: None,
            tz: None,
        })
    }
}

/// A file's new content: UTF-8 text is sent as is, anything else as base64.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum Content {
    Text { content: String },
    Base64 { content_base64: String },
}

impl Content {
    pub fn from_bytes(data: Vec<u8>) -> Content {
        match String::from_utf8(data) {
            Ok(content) => Content::Text { content },
            Err(e) => Content::Base64 {
                content_base64: base64(e.as_bytes()),
            },
        }
    }
}

/// What happens to one path.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum Edit {
    Write(Content),
    /// A blob already stored on the server (see [`Client::upload_blobs`]).
    Blob {
        blob: Oid,
    },
    Delete {
        delete: bool,
    },
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    #[serde(flatten)]
    pub edit: Edit,
    /// `100644`, `100755` or `120000`. Default: the file's existing mode,
    /// else `100644`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct LandOnto {
    pub onto: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct ChangesetRequest {
    pub parents: Vec<String>,
    pub author: Person,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committer: Option<Person>,
    pub message: String,
    pub changes: Vec<FileChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub land: Option<LandOnto>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChangesetResponse {
    pub oid: Oid,
    #[serde(default)]
    pub tree: Option<Oid>,
    #[serde(default)]
    pub land: Option<Landed>,
}

#[derive(Serialize, Clone, Debug)]
pub struct LandRequest {
    pub changesets: Vec<String>,
    pub onto: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committer: Option<Person>,
}

#[derive(Serialize, Clone, Debug)]
pub struct RevertRequest {
    pub commit: String,
    pub author: Person,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committer: Option<Person>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onto: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RevertResponse {
    pub oid: Oid,
    #[serde(default)]
    pub land: Option<Landed>,
}

#[derive(Serialize)]
struct BlobsRequest {
    blobs: Vec<Content>,
}

#[derive(Deserialize)]
struct BlobsResponse {
    oids: Vec<Oid>,
}

/// Interpret a write endpoint's response. Success and 409 both carry the
/// typed body; a 409 that is a plain error (or any other failure) becomes an
/// error, with a hint for the usual configuration mistakes.
pub fn parse_write<T: DeserializeOwned>(status: reqwest::StatusCode, body: &[u8]) -> Result<T> {
    if status.is_success() {
        return serde_json::from_slice(body).with_context(|| {
            format!(
                "unexpected response ({status}): {}",
                String::from_utf8_lossy(body)
            )
        });
    }
    if status == reqwest::StatusCode::CONFLICT {
        if let Ok(v) = serde_json::from_slice(body) {
            return Ok(v);
        }
    }
    let (code, msg) = match serde_json::from_slice::<ErrorBody>(body) {
        Ok(e) => (e.code, e.error),
        Err(_) => (String::new(), String::from_utf8_lossy(body).into_owned()),
    };
    let hint = match (status.as_u16(), code.as_str()) {
        (403, _) | (_, "forbidden") => {
            " (writes must go to the control-plane server, not a cache, with a write token: set WREN_WRITE_SERVER and WREN_WRITE_TOKEN)"
        }
        (401, _) | (_, "unauthorized") => {
            " (write endpoints need the write token: set WREN_WRITE_TOKEN)"
        }
        _ => "",
    };
    bail!("{status}: {msg}{hint}")
}

/// Standard base64 with padding (RFC 4648).
pub fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = |i: usize| chunk.get(i).copied().unwrap_or(0) as u32;
        let n = (b(0) << 16) | (b(1) << 8) | b(2);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;
    use serde_json::json;

    fn oid(c: char) -> String {
        c.to_string().repeat(40)
    }

    #[test]
    fn base64_matches_rfc4648() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
        assert_eq!(base64(&[0xff, 0xfe, 0x00]), "//4A");
    }

    #[test]
    fn person_parse() {
        let p = Person::parse(" Ada Lovelace <ada@example.com> ").unwrap();
        assert_eq!(
            (p.name.as_str(), p.email.as_str()),
            ("Ada Lovelace", "ada@example.com")
        );
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({"name": "Ada Lovelace", "email": "ada@example.com"})
        );
        for bad in ["Ada", "<a@b>", "Ada <>", "Ada <a@b", "Ada <a<b>"] {
            assert!(Person::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn request_shapes() {
        let change = |path: &str, edit, mode: Option<&str>| FileChange {
            path: path.into(),
            edit,
            mode: mode.map(Into::into),
        };
        let req = ChangesetRequest {
            parents: vec!["main".into()],
            author: Person::parse("A <a@x>").unwrap(),
            committer: None,
            message: "m".into(),
            changes: vec![
                change(
                    "a.txt",
                    Edit::Write(Content::from_bytes(b"hi\n".to_vec())),
                    None,
                ),
                change(
                    "bin",
                    Edit::Write(Content::from_bytes(vec![0xff, 0])),
                    Some("100755"),
                ),
                change(
                    "big",
                    Edit::Blob {
                        blob: oid('b').parse().unwrap(),
                    },
                    None,
                ),
                change("gone", Edit::Delete { delete: true }, None),
            ],
            land: Some(LandOnto {
                onto: "main".into(),
            }),
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({
                "parents": ["main"],
                "author": {"name": "A", "email": "a@x"},
                "message": "m",
                "changes": [
                    {"path": "a.txt", "content": "hi\n"},
                    {"path": "bin", "content_base64": "/wA=", "mode": "100755"},
                    {"path": "big", "blob": oid('b')},
                    {"path": "gone", "delete": true},
                ],
                "land": {"onto": "main"},
            })
        );
        let land = LandRequest {
            changesets: vec!["abc".into()],
            onto: "main".into(),
            committer: None,
        };
        assert_eq!(
            serde_json::to_value(&land).unwrap(),
            json!({"changesets": ["abc"], "onto": "main"})
        );
        let rv = RevertRequest {
            commit: "abc".into(),
            author: Person::parse("A <a@x>").unwrap(),
            committer: None,
            onto: Some("main".into()),
        };
        assert_eq!(
            serde_json::to_value(&rv).unwrap(),
            json!({"commit": "abc", "author": {"name": "A", "email": "a@x"}, "onto": "main"})
        );
        assert_eq!(
            serde_json::to_value(BlobsRequest {
                blobs: vec![Content::from_bytes(b"x".to_vec())]
            })
            .unwrap(),
            json!({"blobs": [{"content": "x"}]})
        );
    }

    #[test]
    fn parses_success_and_409_bodies() {
        let created: ChangesetResponse = parse_write(
            StatusCode::CREATED,
            json!({"oid": oid('a'), "tree": oid('b')})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(created.oid.to_hex(), oid('a'));
        assert!(created.land.is_none());

        let landed: ChangesetResponse = parse_write(
            StatusCode::OK,
            json!({"oid": oid('a'), "tree": oid('b'), "land": {
                "result": "landed", "ref": "refs/heads/main", "old": oid('c'), "new": oid('d'),
                "changesets": [oid('d')], "dropped": [], "attempts": 1}})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert!(
            matches!(landed.land, Some(Landed::Landed { ref name, attempts: 1, .. }) if name == "refs/heads/main")
        );

        let conflict: ChangesetResponse = parse_write(
            StatusCode::CONFLICT,
            json!({"oid": oid('a'), "tree": oid('b'), "land": {
                "result": "conflicts", "onto": oid('c'), "conflicts": [{
                    "changeset": oid('a'), "path": "src/x.rs", "kind": "content",
                    "base": {"mode": "100644", "oid": oid('1')},
                    "ours": {"mode": "100644", "oid": oid('2')},
                    "theirs": {"mode": "100644", "oid": oid('3')}}]}})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        match conflict.land {
            Some(Landed::Conflicts { onto, conflicts }) => {
                assert_eq!(onto.to_hex(), oid('c'));
                assert_eq!(conflicts[0].path, "src/x.rs");
                assert_eq!(conflicts[0].kind, "content");
            }
            other => panic!("{other:?}"),
        }

        let rejected: Landed = parse_write(
            StatusCode::CONFLICT,
            br#"{"result":"rejected","reason":"protected branch"}"#,
        )
        .unwrap();
        assert_eq!(
            rejected,
            Landed::Rejected {
                reason: "protected branch".into()
            }
        );

        let deleted: Landed = parse_write(
            StatusCode::CONFLICT,
            json!({"result": "conflicts", "onto": oid('c'), "conflicts": [{
                "changeset": oid('a'), "path": "f", "kind": "modify/delete",
                "base": {"mode": "100644", "oid": oid('1')}, "ours": null,
                "theirs": {"mode": "100644", "oid": oid('3')}, "detail": "deleted on the branch"}]})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert!(
            matches!(deleted, Landed::Conflicts { ref conflicts, .. } if conflicts[0].ours.is_none())
        );

        let reverted: RevertResponse = parse_write(
            StatusCode::CREATED,
            json!({"oid": oid('e')}).to_string().as_bytes(),
        )
        .unwrap();
        assert_eq!(reverted.oid.to_hex(), oid('e'));
    }

    #[test]
    fn errors_keep_message_and_hint() {
        let err =
            |status, body: &[u8]| parse_write::<Landed>(status, body).unwrap_err().to_string();
        let e = err(
            StatusCode::FORBIDDEN,
            br#"{"code":"forbidden","error":"caches do not accept writes"}"#,
        );
        assert!(
            e.contains("403")
                && e.contains("caches do not accept writes")
                && e.contains("WREN_WRITE_SERVER"),
            "{e}"
        );
        let e = err(StatusCode::UNAUTHORIZED, b"");
        assert!(e.contains("WREN_WRITE_TOKEN"), "{e}");
        // A 409 whose body is a plain error rather than a typed outcome.
        let e = err(
            StatusCode::CONFLICT,
            br#"{"code":"conflict","error":"main kept moving"}"#,
        );
        assert!(e.contains("409") && e.contains("main kept moving"), "{e}");
        let e = err(
            StatusCode::NOT_FOUND,
            br#"{"code":"not_found","error":"unknown revision nope"}"#,
        );
        assert!(
            e.contains("404") && e.contains("unknown revision nope"),
            "{e}"
        );
        // A success that is not the expected shape is an error, not a panic.
        assert!(parse_write::<Landed>(StatusCode::OK, b"{}").is_err());
    }
}
