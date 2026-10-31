//! fakellm: an in-memory, OpenAI-compatible LLM API server for tests.
//!
//! It speaks the wire protocols (OpenAI Chat Completions and Responses, Anthropic Messages)
//! without running a model: replies come from scripts, or from a deterministic default.
//! A control plane under `/_fakellm/` resets state, registers scripts, moves the server clock and
//! reads a log of parsed requests. The binary is a thin CLI over this library.
//!
//! ```no_run
//! # async fn demo() -> std::io::Result<()> {
//! let server = fakellm::Server::start(fakellm::Config::default()).await?;
//! let base_url = format!("{}/v1", server.endpoint()); // "http://127.0.0.1:<random port>/v1"
//! // ... point an OpenAI client at `base_url` with any API key ...
//! server.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! Synchronous code can use [`Server::start_blocking`], which runs the server on a background
//! runtime and stops it when the returned handle is dropped.

// `ApiError` carries headers now and is returned by every parser; errors are the cold path.
#![allow(clippy::result_large_err)]

mod anthropic;
mod blocking;
mod clock;
mod control;
mod fallback;
pub mod ir;
mod openai;
mod reqlog;
mod script;
mod sse;
mod tokenizer;
pub mod wire;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub use blocking::BlockingServer;
pub use clock::{Clock, OutOfRange, parse_duration};
pub use control::{Control, ControlError, ResetOptions};
pub use fallback::FallbackConfig;
pub use ir::{ApiError, Delivery, FinishReason, Outcome, Reply, Request, Source, Usage};
pub use reqlog::{DEFAULT_CAPACITY as DEFAULT_REQUEST_LOG_CAPACITY, RequestFilter, RequestLog, RequestRecord, StatusFilter};
pub use script::{Engine, Match, ReplySpec, Rule, RuleInfo, ScriptError, ScriptFormat, Scripts};

/// Response body type used throughout: fully buffered JSON, or a streamed SSE body.
pub type Body = BoxBody<Bytes, Infallible>;
pub type Response = http::Response<Body>;

/// Models `GET /v1/models` lists when [`Config::models`] is left empty. Any model name is
/// accepted by the chat endpoint regardless; the list only matters to clients that fetch it.
pub const DEFAULT_MODELS: &[&str] = &[
    "gpt-4o",
    "gpt-4o-mini",
    "gpt-4.1",
    "gpt-4.1-mini",
    "gpt-4.1-nano",
    "gpt-5",
    "gpt-5-mini",
    "gpt-5-nano",
    "o3",
    "o4-mini",
    "gpt-5.5",
    "claude-sonnet-4-5",
    "claude-opus-4-1",
    "claude-haiku-4-5",
];

/// Server settings. `Default` listens on a random localhost port, accepts any API key, answers
/// unmatched requests with the deterministic default reply, and streams without delay.
#[derive(Clone, Debug)]
pub struct Config {
    /// Address to listen on.
    pub host: String,
    /// 0 picks a free port.
    pub port: u16,
    /// When set, `Authorization: Bearer <key>` or `x-api-key: <key>` must match exactly; anything
    /// else gets a 401 `invalid_api_key`. When unset, any key (or none) is accepted.
    pub api_key: Option<String>,
    /// Models returned by `GET /v1/models`; empty means [`DEFAULT_MODELS`].
    pub models: Vec<String>,
    /// Fail requests no script rule matches (400 `fakellm_no_matching_rule`) instead of answering
    /// with the default reply.
    pub strict: bool,
    /// Shape of the default reply.
    pub fallback: FallbackConfig,
    /// Timing and chunking used when a rule does not specify its own.
    pub delivery: Delivery,
    /// Rules loaded at start (`--scripts FILE`); the control plane can replace them.
    pub scripts: Vec<Rule>,
    /// Record API requests for the control plane's request log.
    pub request_log: bool,
    /// Requests kept by the log; the oldest are dropped first.
    pub request_log_capacity: usize,
    /// Store `"<redacted>"` instead of the values of `authorization`, `proxy-authorization` and
    /// `x-api-key` in the request log, so the log can be shared.
    pub redact_auth: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 0,
            api_key: None,
            models: Vec::new(),
            strict: false,
            fallback: FallbackConfig::default(),
            delivery: Delivery::default(),
            scripts: Vec::new(),
            request_log: true,
            request_log_capacity: DEFAULT_REQUEST_LOG_CAPACITY,
            redact_auth: false,
        }
    }
}

/// A running server. [`Server::shutdown`] stops it and waits; dropping it also closes the
/// listener, but does not wait. Connections that are already open keep being served until their
/// clients hang up or the Tokio runtime shuts down.
pub struct Server {
    addr: SocketAddr,
    control: Arc<Control>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Server {
    /// Binds the listener and starts serving on the current Tokio runtime.
    pub async fn start(config: Config) -> std::io::Result<Self> {
        let listener = TcpListener::bind((config.host.as_str(), config.port)).await?;
        let addr = listener.local_addr()?;
        let control = Arc::new(Control::new(&config).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?);
        let (stop, mut stopped) = oneshot::channel();
        let shared = Arc::new(Shared {
            control: control.clone(),
            api_key: config.api_key.clone(),
            models: if config.models.is_empty() {
                DEFAULT_MODELS.iter().map(|s| (*s).to_owned()).collect()
            } else {
                config.models.clone()
            },
        });
        let task = tokio::spawn(async move {
            let http = ConnBuilder::new(TokioExecutor::new());
            loop {
                let socket = tokio::select! {
                    r = listener.accept() => match r {
                        Ok((socket, _peer)) => socket,
                        Err(e) => {
                            tracing::warn!("accept: {e}");
                            continue;
                        }
                    },
                    _ = &mut stopped => break,
                };
                let _ = socket.set_nodelay(true);
                let shared = shared.clone();
                let service = hyper::service::service_fn(move |req| {
                    let shared = shared.clone();
                    async move { Ok::<_, Infallible>(shared.handle(req).await) }
                });
                let conn = http.serve_connection_with_upgrades(TokioIo::new(socket), service).into_owned();
                tokio::spawn(async move {
                    let _ = conn.await;
                });
            }
        });
        Ok(Self {
            addr,
            control,
            stop: Some(stop),
            task,
        })
    }

    /// The address actually bound; the real port when `Config::port` was 0.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://host:port`. OpenAI clients want `base_url = "{endpoint}/v1"`.
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// `http://host:port/v1`, ready to use as an OpenAI `base_url`.
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// The server's clock, for tests that need to move time.
    pub fn clock(&self) -> &Clock {
        &self.control.clock
    }

    /// Reset, scripts and the request log: the same operations as the `/_fakellm/` HTTP API.
    pub fn control(&self) -> &Control {
        &self.control
    }

    /// The script rules and their hit counts.
    pub fn scripts(&self) -> &Scripts {
        &self.control.engine.scripts
    }

    /// Clears scripts, the request log, the clock and stored responses.
    pub fn reset(&self) {
        self.control.reset();
    }

    /// Logged requests matching `filter`, oldest first, and the match count before `limit`.
    pub fn requests(&self, filter: &RequestFilter) -> (Vec<RequestRecord>, usize) {
        self.control.log.list(filter)
    }

    /// Ids and creation times (unix seconds) of the stored Responses API objects.
    pub fn response_ids(&self) -> Vec<(String, i64)> {
        self.control.response_ids()
    }

    /// Forgets all stored Responses API objects.
    pub fn clear_responses(&self) -> usize {
        self.control.clear_responses()
    }

    /// Stops accepting connections. Requests already in flight finish on their own.
    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = (&mut self.task).await;
    }

    /// Serves until the process exits.
    pub async fn wait(self) {
        let _ = self.task.await;
    }
}

/// Everything a connection handler needs.
pub(crate) struct Shared {
    pub(crate) control: Arc<Control>,
    pub(crate) api_key: Option<String>,
    pub(crate) models: Vec<String>,
}

impl Shared {
    async fn handle(&self, req: http::Request<hyper::body::Incoming>) -> Response {
        if req.uri().path().starts_with(control::PREFIX) {
            return self.control.handle(req).await;
        }
        openai::handle(self, req).await
    }
}

/// Builds a JSON response with `Content-Length`.
pub(crate) fn json_response(status: u16, body: &serde_json::Value) -> Response {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    http::Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::CONTENT_LENGTH, bytes.len())
        .body(full_body(Bytes::from(bytes)))
        .expect("static response parts are valid")
}

pub(crate) fn full_body(bytes: Bytes) -> Body {
    use http_body_util::BodyExt;
    http_body_util::Full::new(bytes).boxed()
}

/// Status, headers and `body` for `err`; shared by every protocol, which only differ in `body`.
/// `retry-after` comes first, then `err.headers`; names and values that are not valid HTTP are
/// skipped.
pub(crate) fn error_response(err: &ir::ApiError, body: &serde_json::Value) -> Response {
    let bytes = Bytes::from(serde_json::to_vec(body).expect("error body serializes"));
    let mut b = http::Response::builder()
        .status(http::StatusCode::from_u16(err.status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::CONTENT_LENGTH, bytes.len());
    for (name, value) in error_headers(err) {
        if let (Ok(n), Ok(v)) = (http::HeaderName::from_bytes(name.as_bytes()), http::HeaderValue::from_str(&value)) {
            b = b.header(n, v);
        }
    }
    b.body(full_body(bytes)).expect("valid response")
}

/// The extra headers an error carries: `retry-after` (whole seconds) when set, then `err.headers`.
pub(crate) fn error_headers(err: &ir::ApiError) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(d) = err.retry_after {
        out.push(("retry-after".to_owned(), retry_after_secs(d).to_string()));
    }
    out.extend(err.headers.iter().cloned());
    out
}

/// Whole seconds, rounded up, for `retry-after`.
pub(crate) fn retry_after_secs(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}
