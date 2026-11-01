//! Upstream mode (`--upstream URL`): a request no script rule and no cassette answers is relayed to a real
//! OpenAI-compatible or Anthropic server in its own protocol, the response streams back to the client unchanged,
//! and a copy is reassembled into an IR outcome that is recorded (`--record FILE` and the in-memory `recording`
//! cassette). Scripts still run first, so faults can be injected on top of a real upstream. See docs/m4-spec.md section 4.
//!
//! - [`client`]: the HTTP client (also used by the Hugging Face importer);
//! - [`reassemble`]: SSE and sync bodies back into replies;
//! - [`record`]: the record file and the step that runs when a relayed response is done.

pub mod client;
pub mod reassemble;
pub mod record;

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::Uri;
use http::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use http_body_util::{BodyExt, Full};
use hyper::body::{Body as _, Frame, Incoming};
use parking_lot::Mutex;

use crate::cassette::format::CassetteOutcome;
use crate::ir::{ApiError, Protocol};
use crate::upstream::client::HttpClient;
use crate::upstream::reassemble::Reassembler;
use crate::upstream::record::Recorder;

/// Carries the ids of the fakellm instances a request has been relayed through, comma separated, so a loop (an
/// upstream that points back at this instance or at a cycle of instances) is refused instead of relayed forever.
pub const HOP_HEADER: &str = "x-fakellm-hop";

/// How long the upstream body may be silent between two frames before the relay gives up on it.
pub const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Longest chain of fakellm instances a request may pass through.
pub const MAX_HOPS: usize = 8;

/// `anthropic-version` sent when an Anthropic request carries none and no client header can be passed through.
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// What `forward` needs of the client's request. `path_and_query` is the routed API path (after `api_path`, so
/// `/v1/chat/completions`, not `/gateway/v1/chat/completions`) plus the original query string.
#[derive(Clone, Debug)]
pub struct Call {
    pub method: http::Method,
    pub path_and_query: String,
    /// The client's headers, as received.
    pub headers: http::HeaderMap,
    /// The client's body, byte for byte.
    pub body: Bytes,
    pub protocol: Protocol,
}

/// How a relayed response ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Completed {
    /// The reassembled outcome; `None` when the body was cut short, the client went away mid-stream, or the body
    /// was not something the protocol sends.
    pub outcome: Option<CassetteOutcome>,
    /// For Responses: the final response object, see `Reassembled::response`.
    pub response: Option<serde_json::Value>,
    /// The upstream's HTTP status.
    pub status: u16,
    /// Host-clock time from sending the request to the end of the body.
    pub duration: Duration,
    /// Why `outcome` is `None`, for the log.
    pub problem: Option<String>,
}

/// Called exactly once per relayed response, synchronously: when the upstream body has ended (before the client is
/// told the stream ended), or when the response is dropped early.
pub type OnComplete = Box<dyn FnOnce(Completed) + Send + 'static>;

fn shown_url(base: &Uri) -> String {
    let scheme = base.scheme_str().unwrap_or("http");
    let authority = base.authority().map_or("", |a| a.as_str());
    format!("{scheme}://{authority}{}", base.path().trim_end_matches('/'))
}

/// The configured upstream server.
pub struct Upstream {
    base: Uri,
    api_key: Option<String>,
    /// fakellm's own `--api-key`, which must never reach the upstream.
    local_key: Option<String>,
    /// Unique per process and `Upstream`, sent in [`HOP_HEADER`].
    id: String,
    client: HttpClient,
    recorder: Option<Recorder>,
    /// Whether retryable and credential errors are recorded too, see [`Upstream::records_status`].
    record_errors: bool,
    /// `--record-redact` patterns applied to every recorded text, see [`Upstream::redact`].
    redact: Vec<crate::script::Re>,
}

impl Upstream {
    /// Validates `base` (`http` or `https`, with a host) and opens the record file if one is given.
    pub fn new(base: &Uri, api_key: Option<String>, record: Option<&std::path::Path>) -> Result<Self, String> {
        match (base.scheme_str(), base.authority()) {
            (Some("http" | "https"), Some(_)) => {}
            _ => {
                return Err(format!(
                    "invalid upstream {base}: expected http://host[:port][/path] or https://..."
                ));
            }
        }
        if base.authority().is_some_and(|a| a.as_str().contains('@')) {
            return Err("invalid upstream: credentials in the URL are not supported, use --upstream-api-key".into());
        }
        if let Some(key) = &api_key
            && HeaderValue::from_str(&format!("Bearer {key}")).is_err()
        {
            return Err("invalid upstream API key: it cannot be sent as an HTTP header value".into());
        }
        let shown = shown_url(base);
        let recorder = record
            .map(|p| Recorder::open(p, &shown).map_err(|e| format!("cannot open record file {}: {e}", p.display())))
            .transpose()?;
        Ok(Self {
            base: base.clone(),
            api_key,
            local_key: None,
            id: instance_id(),
            client: HttpClient::new(),
            recorder,
            record_errors: false,
            redact: Vec::new(),
        })
    }

    /// Declares fakellm's own `--api-key`: a client credential equal to it is dropped instead of forwarded when there
    /// is no upstream API key to replace it.
    pub fn with_local_key(mut self, key: Option<String>) -> Self {
        self.local_key = key;
        self
    }

    /// Limit for the upstream's response head, in seconds; 0 waits without limit (`--upstream-timeout`).
    pub fn with_header_timeout_secs(mut self, secs: u64) -> Self {
        let limit = (secs > 0).then(|| std::time::Duration::from_secs(secs));
        self.client = self.client.clone().with_header_timeout(limit);
        self
    }

    /// Also record the upstream's transient and credential errors (`--record-errors`).
    pub fn with_record_errors(mut self, on: bool) -> Self {
        self.record_errors = on;
        self
    }

    /// Replaces every match of `patterns` in the recorded texts with `<redacted>` (`--record-redact`). An
    /// invalid pattern is an error naming it.
    pub fn with_record_redact(mut self, patterns: &[String]) -> Result<Self, String> {
        self.redact = patterns
            .iter()
            .map(|p| {
                crate::script::Pattern(p.clone())
                    .compile()
                    .map_err(|e| format!("invalid --record-redact pattern {p:?}: {e}"))
            })
            .collect::<Result<_, _>>()?;
        Ok(self)
    }

    /// `exchange` with the `--record-redact` patterns applied to the texts of its request and outcome.
    /// `None` when it cannot be redacted safely (the exchange is then not recorded).
    pub fn redact(&self, exchange: crate::cassette::format::Exchange) -> Option<crate::cassette::format::Exchange> {
        record::redact_exchange(exchange, &self.redact)
    }

    /// Whether an exchange the upstream answered with `status` is recorded. Transient errors (408, 409, 429, 5xx)
    /// and credential errors (401, 403) are not, unless `--record-errors`: a retry that succeeded would otherwise be
    /// shadowed by the recorded failure, which has the same key.
    pub fn records_status(&self, status: u16) -> bool {
        self.record_errors || !matches!(status, 401 | 403 | 408 | 409 | 429 | 500..)
    }

    #[cfg(test)]
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn recorder(&self) -> Option<&Recorder> {
        self.recorder.as_ref()
    }

    /// The base URL without its query, for logs, errors, the record header and the control plane (a query may carry a key).
    pub fn display_url(&self) -> String {
        shown_url(&self.base)
    }

    /// The URL a client path maps to: the base's path (without a trailing `/`) followed by `path_and_query`, except
    /// that a leading `/v1` of the client path is dropped when the base path already ends in `/v1`, so both
    /// `https://api.openai.com` and `https://api.openai.com/v1` work for `/v1/chat/completions`.
    pub fn target(&self, path_and_query: &str) -> String {
        let base_path = self.base.path().trim_end_matches('/');
        let rest = match path_and_query.strip_prefix("/v1") {
            Some(r) if base_path.ends_with("/v1") && (r.is_empty() || r.starts_with(['/', '?'])) => r,
            _ => path_and_query,
        };
        let scheme = self.base.scheme_str().unwrap_or("http");
        let authority = self.base.authority().map_or("", |a| a.as_str());
        let mut url = format!("{scheme}://{authority}{base_path}{rest}");
        if let Some(query) = self.base.query().filter(|q| !q.is_empty()) {
            url.push(if url.contains('?') { '&' } else { '?' });
            url.push_str(query);
        }
        url
    }

    /// The new [`HOP_HEADER`] value for a request relayed by this instance: the ids already in the client's header
    /// plus this instance's. `ApiError::upstream_loop` (508) when this instance's id is already there (the upstream
    /// leads back here) or the chain is [`MAX_HOPS`] long.
    pub(crate) fn next_hop(&self, headers: &HeaderMap) -> Result<String, ApiError> {
        let mut ids: Vec<&str> = headers
            .get_all(HOP_HEADER)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if ids.contains(&self.id.as_str()) || ids.len() >= MAX_HOPS {
            return Err(ApiError::upstream_loop(&self.display_url()));
        }
        ids.push(&self.id);
        Ok(ids.join(","))
    }

    /// The headers `forward` sends: the client's end-to-end headers, no hop-by-hop ones, `host` or `content-length`
    /// (hyper sets those), `accept-encoding: identity` (the copy must be parseable), [`HOP_HEADER`] extended by this
    /// instance, and credentials by this table:
    ///
    /// | upstream API key | client credentials (`authorization`, `x-api-key`) | sent |
    /// |---|---|---|
    /// | set | any, both dropped | `authorization: Bearer <key>`, plus `x-api-key: <key>` for Anthropic requests |
    /// | unset | any | passed through as they are (both when both are present), except a credential equal to fakellm's own `--api-key`, which is dropped |
    ///
    /// An Anthropic request without `anthropic-version` gets [`DEFAULT_ANTHROPIC_VERSION`]; no other header is added.
    pub(crate) fn request_headers(&self, call: &Call) -> Result<HeaderMap, ApiError> {
        let hop = self.next_hop(&call.headers)?;
        let anthropic = call.protocol == Protocol::AnthropicMessages;
        let x_api_key = HeaderName::from_static("x-api-key");
        let mut out = HeaderMap::new();
        for (name, value) in &call.headers {
            let n = name.as_str();
            if HOP_BY_HOP.contains(&n) || matches!(n, "host" | "content-length" | "accept-encoding" | HOP_HEADER) {
                continue;
            }
            out.append(name.clone(), value.clone());
        }
        out.insert(http::header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        out.insert(
            HeaderName::from_static(HOP_HEADER),
            HeaderValue::from_str(&hop).expect("ids are hex"),
        );
        if let Some(key) = &self.api_key {
            out.remove(&AUTHORIZATION);
            out.remove(&x_api_key);
            out.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {key}")).expect("checked in new"),
            );
            if anthropic {
                out.insert(x_api_key, HeaderValue::from_str(key).expect("checked in new"));
            }
        } else if let Some(own) = &self.local_key {
            let is_own = |v: &HeaderValue, bearer: bool| {
                v.to_str().is_ok_and(|v| match bearer {
                    true => v
                        .strip_prefix("Bearer ")
                        .or_else(|| v.strip_prefix("bearer "))
                        .is_some_and(|k| k == own),
                    false => v == own,
                })
            };
            if out.get(&AUTHORIZATION).is_some_and(|v| is_own(v, true)) {
                out.remove(&AUTHORIZATION);
            }
            if out.get(&x_api_key).is_some_and(|v| is_own(v, false)) {
                out.remove(&x_api_key);
            }
        }
        if anthropic && !out.contains_key("anthropic-version") {
            out.insert("anthropic-version", HeaderValue::from_static(DEFAULT_ANTHROPIC_VERSION));
        }
        Ok(out)
    }

    /// Relays `call` and returns the response to send to the client; `on_complete` fires when the body is done.
    ///
    /// Request: method, `target(call.path_and_query)`, `call.body` byte for byte, and `request_headers(&call)?`
    /// (which also refuses a request that has looped back here with `ApiError::upstream_loop`, before anything is sent).
    ///
    /// Response: status and headers as received minus hop-by-hop ones; the body is relayed frame by frame with no
    /// buffering, each frame also fed to a `Reassembler`. HTTP errors from the upstream are relayed as they are too.
    /// Errors (`Err`): a loop (above), no connection or no response head in time (`ApiError::upstream_unreachable`);
    /// nothing was sent to the client yet.
    pub async fn forward(&self, call: Call, on_complete: OnComplete) -> Result<crate::Response, ApiError> {
        let headers = self.request_headers(&call)?;
        let url = self.target(&call.path_and_query);
        let mut builder = http::Request::builder().method(call.method.clone()).uri(&url);
        *builder.headers_mut().expect("builder is valid so far") = headers;
        let req = builder
            .body(Full::new(call.body.clone()))
            .map_err(|e| ApiError::upstream_unreachable(&self.display_url(), &format!("invalid request: {e}")))?;
        let started = Instant::now();
        let resp = self
            .client
            .send(req)
            .await
            .map_err(|e| ApiError::upstream_unreachable(&self.display_url(), &e.0))?;
        let (parts, incoming) = resp.into_parts();
        let mut headers = parts.headers;
        for name in HOP_BY_HOP {
            headers.remove(name);
        }
        let mut tee = Tee {
            inner: incoming,
            reassembler: Some(Reassembler::new(call.protocol)),
            status: parts.status.as_u16(),
            headers: headers.clone(),
            started,
            on_complete: Mutex::new(Some(on_complete)),
            idle: Box::pin(tokio::time::sleep(BODY_IDLE_TIMEOUT)),
        };
        // An empty body may never be polled.
        tee.complete_if_ended();
        let mut out = http::Response::new(tee.boxed());
        *out.status_mut() = parts.status;
        *out.headers_mut() = headers;
        Ok(out)
    }
}

/// The upstream body, relayed frame by frame; a copy goes to the [`Reassembler`] and `on_complete` runs once when the
/// body has ended, failed, or been dropped.
struct Tee {
    inner: Incoming,
    reassembler: Option<Reassembler>,
    status: u16,
    headers: HeaderMap,
    started: Instant,
    on_complete: Mutex<Option<OnComplete>>,
    /// Fires when no frame arrived for [`BODY_IDLE_TIMEOUT`]; reset by every frame.
    idle: Pin<Box<tokio::time::Sleep>>,
}

impl Tee {
    /// Runs the callback if it has not run yet; `problem` is why the body did not end cleanly.
    fn complete(&mut self, problem: Option<String>) {
        let Some(done) = self.on_complete.get_mut().take() else { return };
        let duration = self.started.elapsed();
        let (outcome, response, problem) = match (problem, self.reassembler.take()) {
            (None, Some(r)) => match r.finish(self.status, &self.headers) {
                Ok(r) => (Some(r.outcome), r.response, None),
                Err(e) => (None, None, Some(e)),
            },
            (problem, _) => (None, None, problem),
        };
        done(Completed {
            outcome,
            response,
            status: self.status,
            duration,
            problem,
        });
    }

    fn complete_if_ended(&mut self) {
        if self.inner.is_end_stream() {
            self.complete(None);
        }
    }
}

impl hyper::body::Body for Tee {
    type Data = Bytes;
    type Error = crate::BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Pending => {
                if this.idle.as_mut().poll(cx).is_ready() {
                    let problem = format!("the upstream body was silent for {} s", BODY_IDLE_TIMEOUT.as_secs());
                    this.complete(Some(problem.clone()));
                    return Poll::Ready(Some(Err(problem.into())));
                }
                Poll::Pending
            }
            Poll::Ready(Some(Ok(frame))) => {
                this.idle.as_mut().reset(tokio::time::Instant::now() + BODY_IDLE_TIMEOUT);
                if let (Some(data), Some(r)) = (frame.data_ref(), this.reassembler.as_mut()) {
                    r.push(data);
                }
                // With a known length the server may never ask for the end.
                this.complete_if_ended();
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.complete(Some(format!("the upstream body failed: {e}")));
                Poll::Ready(Some(Err(Box::new(e))))
            }
            Poll::Ready(None) => {
                this.complete(None);
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for Tee {
    fn drop(&mut self) {
        self.complete(Some(
            "the response was dropped before the upstream body ended (the client went away)".into(),
        ));
    }
}

/// Unique per process and `Upstream`: pid, a counter and the start time, in hex.
fn instance_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    format!("{:x}-{:x}-{:x}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed), nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(base: &str) -> Upstream {
        Upstream::new(&base.parse().unwrap(), None, None).unwrap()
    }

    #[test]
    fn query_is_kept_and_userinfo_refused() {
        let u = up("https://h/openai?api-version=1");
        assert_eq!(u.target("/v1/models"), "https://h/openai/v1/models?api-version=1");
        assert_eq!(u.target("/v1/models?a=b"), "https://h/openai/v1/models?a=b&api-version=1");
        assert_eq!(u.display_url(), "https://h/openai");
        assert!(Upstream::new(&"https://u:secret@h".parse().unwrap(), None, None).is_err());
        assert!(!u.records_status(429) && !u.records_status(503) && u.records_status(400) && u.records_status(200));
        assert!(up("http://h").with_record_errors(true).records_status(429));
    }

    #[test]
    fn target_joins_base_and_path() {
        assert_eq!(
            up("https://api.openai.com").target("/v1/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            up("https://api.openai.com/v1").target("/v1/chat/completions?x=1"),
            "https://api.openai.com/v1/chat/completions?x=1"
        );
        assert_eq!(up("http://h:8080/openai/").target("/v1/models"), "http://h:8080/openai/v1/models");
        assert_eq!(up("http://h/v1/").target("/v1/messages"), "http://h/v1/messages");
        assert_eq!(up("http://h/v1").target("/v1"), "http://h/v1");
        assert_eq!(up("http://h/v1").target("/v10/x"), "http://h/v1/v10/x");
    }

    #[test]
    fn new_rejects_bad_urls() {
        for bad in ["ftp://h/", "/just/a/path", "h:80"] {
            assert!(Upstream::new(&bad.parse().unwrap(), None, None).is_err(), "{bad}");
        }
    }

    fn call(protocol: Protocol, headers: &[(&str, &str)]) -> Call {
        let mut h = HeaderMap::new();
        for (k, v) in headers {
            h.append(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        Call {
            method: http::Method::POST,
            path_and_query: "/v1/chat/completions".into(),
            headers: h,
            body: Bytes::new(),
            protocol,
        }
    }

    fn get<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
        h.get(name).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn hops_detect_loops_and_long_chains() {
        let a = up("http://h/");
        let b = up("http://h/");
        let first = a.next_hop(&HeaderMap::new()).unwrap();
        assert_eq!(first, a.id());
        // a -> b is fine, b -> a again is a loop
        let through_a = call(Protocol::OpenaiChat, &[(HOP_HEADER, &first)]);
        let second = b.next_hop(&through_a.headers).unwrap();
        assert_eq!(second, format!("{},{}", a.id(), b.id()));
        let through_both = call(Protocol::OpenaiChat, &[(HOP_HEADER, &second)]);
        let err = a.next_hop(&through_both.headers).unwrap_err();
        assert_eq!((err.status, err.code.as_deref()), (508, Some("fakellm_upstream_loop")));
        let long = (0..MAX_HOPS).map(|i| format!("x{i}")).collect::<Vec<_>>().join(",");
        assert!(a.next_hop(&call(Protocol::OpenaiChat, &[(HOP_HEADER, &long)]).headers).is_err());
    }

    #[test]
    fn request_headers_drop_hop_by_hop_and_force_identity() {
        let u = up("http://h/");
        let c = call(
            Protocol::OpenaiChat,
            &[
                ("connection", "keep-alive"),
                ("host", "me"),
                ("content-length", "5"),
                ("accept-encoding", "gzip"),
                ("openai-organization", "org"),
                ("authorization", "Bearer client"),
            ],
        );
        let h = u.request_headers(&c).unwrap();
        assert_eq!(get(&h, "accept-encoding"), Some("identity"));
        assert_eq!(get(&h, "openai-organization"), Some("org"));
        assert_eq!(get(&h, "authorization"), Some("Bearer client"));
        assert_eq!(get(&h, HOP_HEADER), Some(u.id()));
        for gone in ["connection", "host", "content-length"] {
            assert!(h.get(gone).is_none(), "{gone}");
        }
    }

    #[test]
    fn upstream_key_replaces_both_client_credentials() {
        let u = Upstream::new(&"http://h/".parse().unwrap(), Some("real".into()), None).unwrap();
        let both = [("authorization", "Bearer client"), ("x-api-key", "client")];
        let h = u.request_headers(&call(Protocol::OpenaiChat, &both)).unwrap();
        assert_eq!(get(&h, "authorization"), Some("Bearer real"));
        assert!(h.get("x-api-key").is_none());
        let h = u.request_headers(&call(Protocol::AnthropicMessages, &both)).unwrap();
        assert_eq!(get(&h, "authorization"), Some("Bearer real"));
        assert_eq!(get(&h, "x-api-key"), Some("real"));
        assert_eq!(get(&h, "anthropic-version"), Some(DEFAULT_ANTHROPIC_VERSION));
        let h = u
            .request_headers(&call(Protocol::AnthropicMessages, &[("anthropic-version", "2024-01-01")]))
            .unwrap();
        assert_eq!(get(&h, "anthropic-version"), Some("2024-01-01"));
    }

    #[test]
    fn without_an_upstream_key_client_credentials_pass_but_the_local_key_never_does() {
        let u = up("http://h/").with_local_key(Some("mine".into()));
        let h = u
            .request_headers(&call(
                Protocol::OpenaiChat,
                &[("authorization", "Bearer client"), ("x-api-key", "client")],
            ))
            .unwrap();
        assert_eq!(get(&h, "authorization"), Some("Bearer client"));
        assert_eq!(get(&h, "x-api-key"), Some("client"));
        let h = u
            .request_headers(&call(
                Protocol::OpenaiChat,
                &[("authorization", "Bearer mine"), ("x-api-key", "mine")],
            ))
            .unwrap();
        assert!(h.get("authorization").is_none() && h.get("x-api-key").is_none());
        assert!(h.get("anthropic-version").is_none());
    }

    #[test]
    fn unusable_upstream_keys_are_rejected() {
        assert!(Upstream::new(&"http://h/".parse().unwrap(), Some("a\nb".into()), None).is_err());
    }
}
