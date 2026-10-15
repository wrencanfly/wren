//! How a tier talks to the tier above it.
//!
//! Every upstream exchange is a buffered request and response, so the
//! transport is a single method. Production uses reqwest; the simulation
//! tests plug in a transport over a simulated network.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use bytes::Bytes;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub struct HttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub trait Transport: Send + Sync + 'static {
    /// Send `req` (an absolute URL) and read the whole response.
    fn send(&self, req: Request<Bytes>) -> BoxFuture<Result<HttpResponse>>;
}

pub struct Reqwest(pub reqwest::Client);

impl Reqwest {
    pub fn new(timeout: Duration) -> Result<Reqwest> {
        Ok(Reqwest(
            reqwest::Client::builder().timeout(timeout).build()?,
        ))
    }
}

impl Transport for Reqwest {
    fn send(&self, req: Request<Bytes>) -> BoxFuture<Result<HttpResponse>> {
        let client = self.0.clone();
        Box::pin(async move {
            let url = req.uri().to_string();
            let req = reqwest::Request::try_from(req.map(reqwest::Body::from))?;
            let resp = client
                .execute(req)
                .await
                .with_context(|| format!("{url} unreachable"))?;
            let status = resp.status();
            let headers = resp.headers().clone();
            let body = resp.bytes().await?;
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        })
    }
}

/// A request to `url` with an optional bearer token and body.
pub fn request(
    method: Method,
    url: &str,
    token: Option<&str>,
    content_type: Option<&str>,
    body: Bytes,
) -> Result<Request<Bytes>> {
    let mut b = Request::builder().method(method).uri(url);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if let Some(c) = content_type {
        b = b.header("content-type", c);
    }
    Ok(b.body(body)?)
}
