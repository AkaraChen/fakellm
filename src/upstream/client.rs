//! The HTTP client shared by the upstream proxy and the Hugging Face importer: hyper's legacy client over
//! `hyper-rustls` (http/1.1 only; TLS 1.2 and 1.3 via `ring`, roots from the bundled `webpki-roots` so it works in
//! the alpine image without `ca-certificates`). Chosen over `reqwest`, which would add tower, an h2 stack and a
//! cookie/redirect layer on top of the same hyper-rustls for nothing this crate uses.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

/// How long the upstream may take to send response headers (connect and TLS included) unless configured:
/// a non-streaming completion sends them only when generation is done.
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(600);

/// A request could not be sent or timed out; the text is for humans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientError(pub String);

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClientError {}

/// A fully buffered response, for the small JSON calls of the importer.
#[derive(Debug)]
pub struct Fetched {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

/// Cheap to clone; connections are pooled inside.
#[derive(Clone)]
pub struct HttpClient {
    inner: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    /// `None`: wait as long as the client does.
    header_timeout: Option<Duration>,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    pub fn new() -> Self {
        let https = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        Self {
            inner: Client::builder(TokioExecutor::new()).build(https),
            header_timeout: Some(HEADER_TIMEOUT),
        }
    }

    /// Sets the response-head limit; `None` waits without limit.
    pub fn with_header_timeout(mut self, limit: Option<Duration>) -> Self {
        self.header_timeout = limit;
        self
    }

    /// Sends `req` and returns the response as soon as its head arrives; the body streams. Fails after
    /// the configured limit ([`HEADER_TIMEOUT`] by default) without a response head. Redirects are not followed.
    pub async fn send(&self, req: http::Request<Full<Bytes>>) -> Result<http::Response<Incoming>, ClientError> {
        let Some(limit) = self.header_timeout else {
            return self.inner.request(req).await.map_err(|e| ClientError(describe(&e)));
        };
        match tokio::time::timeout(limit, self.inner.request(req)).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(ClientError(describe(&e))),
            Err(_) => Err(ClientError(format!("no response within {} s", limit.as_secs()))),
        }
    }

    /// `GET url` with `headers`, body collected (at most `limit` bytes; more is an error).
    pub async fn get(&self, url: &str, headers: &[(&str, &str)], limit: usize) -> Result<Fetched, ClientError> {
        let mut req = http::Request::builder().method("GET").uri(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let req = req
            .body(Full::new(Bytes::new()))
            .map_err(|e| ClientError(format!("invalid request for {url}: {e}")))?;
        let resp = self.send(req).await?;
        let (parts, body) = resp.into_parts();
        let body = http_body_util::Limited::new(body, limit)
            .collect()
            .await
            .map_err(|e| ClientError(format!("reading the response of {url}: {e}")))?
            .to_bytes();
        Ok(Fetched {
            status: parts.status.as_u16(),
            headers: parts.headers,
            body,
        })
    }
}

/// The error and its causes on one line (hyper's own message alone is just "client error (Connect)").
fn describe(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        out.push_str(": ");
        out.push_str(&c.to_string());
        cause = c.source();
    }
    out
}
