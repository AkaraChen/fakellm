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

mod agent;
mod anthropic;
mod blocking;
mod cassette;
mod clock;
mod control;
mod fallback;
pub mod import;
pub mod ir;
mod openai;
mod policy;
mod reqlog;
mod script;
mod snapshot;
mod sse;
mod tokenizer;
mod upstream;
mod watch;
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

pub use agent::tool_map::{ToolMap, ToolMapping};
pub use agent::trajectory::{
    Cursor, Divergence, Expect, Step, ToolResultExpect, Trajectories, Trajectory, TrajectoryError, TrajectoryFormat, TrajectoryInfo,
    UnknownTool,
};
pub use blocking::BlockingServer;
pub use cassette::format::{
    Cassette, CassetteFormat, CassetteOutcome, DEFAULT_IGNORE, Exchange, ExchangeMeta, Normalizer, ToTrajectory, reply_spec,
};
pub use cassette::{CassetteError, CassetteInfo, Cassettes, RECORDING, Replay};
pub use clock::{Clock, OutOfRange, parse_duration};
pub use control::{Control, ControlError, ResetOptions};
pub use fallback::FallbackConfig;
pub use ir::{ApiError, Delivery, FinishReason, Outcome, Reply, Request, Source, Usage};
pub use policy::{Policies, PolicyError, PolicyInfo, PolicySpec, Runtime};
pub use reqlog::{DEFAULT_CAPACITY as DEFAULT_REQUEST_LOG_CAPACITY, RequestFilter, RequestLog, RequestRecord, StatusFilter};
pub use script::{Engine, Match, ReplySpec, Rule, RuleInfo, ScriptError, ScriptFormat, ScriptState, Scripts};
pub use snapshot::SnapshotInfo;
pub use upstream::client::{ClientError, HttpClient};
pub use watch::WatchConfig;

/// Response body type used throughout: fully buffered JSON, or a streamed SSE body.
/// Boxed so a body can fail mid-flight (the reset faults): hyper then drops the connection
/// without the terminating chunk.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = BoxBody<Bytes, BoxError>;
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
    "text-embedding-3-small",
    "text-embedding-3-large",
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
    /// A path the API is also served under, e.g. `/anthropic` makes `/anthropic/v1/messages` work.
    /// Paths that end in a known route (`/v1/messages`, ...) after any leading segments are
    /// accepted regardless; this exists for prefixes in front of routes without `/v1`.
    pub path_prefix: Option<String>,
    /// Trajectories registered at start (`--trajectory FILE`, repeatable); the control plane can replace them.
    pub trajectories: Vec<Trajectory>,
    /// Policy scripts registered at start (`--policy FILE`; the name is the file stem). Starting fails
    /// when a module does not load.
    pub policies: Vec<PolicySpec>,
    /// Runtime for policies that name none; `None` picks `tjs`, `node` or `bun`, in that order, from `PATH`.
    pub policy_runtime: Option<Runtime>,
    /// How long a policy may take to decide, in host-clock milliseconds.
    pub policy_timeout_ms: u64,
    /// Cassettes registered at start (`--cassette FILE`, repeatable); the control plane can replace them.
    pub cassettes: Vec<Cassette>,
    /// Relay requests that no script rule and no cassette answers to this server (`--upstream URL`), in the request's
    /// own protocol, and record what comes back. `http://` or `https://`, optionally with a path (`/v1` is not
    /// repeated). `http::Uri` stands in for a URL type: the crate has no `url` dependency.
    pub upstream: Option<http::Uri>,
    /// Replaces the client's credentials toward the upstream (`authorization: Bearer <key>`, plus `x-api-key` for
    /// Anthropic requests). `None` passes the client's own headers through.
    pub upstream_api_key: Option<String>,
    /// Append every recorded exchange to this file as JSONL (`--record FILE`); needs `upstream`.
    pub record: Option<std::path::PathBuf>,
    /// Also record the upstream's 401, 403, 408, 409, 429 and 5xx answers (`--record-errors`); by default they are
    /// relayed but not recorded, so a retry that succeeded is not shadowed on replay.
    pub record_errors: bool,
    /// Seconds the upstream may take to send response headers (`--upstream-timeout`); 0 waits without limit. A
    /// non-streaming completion sends them only when it is done.
    pub upstream_timeout_secs: u64,
    /// Re-read these files when they change; `None` (the default) watches nothing. The binary fills it from the
    /// startup flags unless `--no-watch`.
    pub watch: Option<WatchConfig>,
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
            path_prefix: None,
            trajectories: Vec::new(),
            policies: Vec::new(),
            policy_runtime: None,
            policy_timeout_ms: policy::DEFAULT_TIMEOUT_MS,
            cassettes: Vec::new(),
            upstream: None,
            upstream_api_key: None,
            record: None,
            record_errors: false,
            upstream_timeout_secs: 600,
            watch: None,
        }
    }
}

/// Whether `url` names the address `addr` this server is bound to: same port, and a host that is that IP, `localhost`
/// or a loopback address when bound to loopback, or any local-looking host when bound to an unspecified address. Names
/// that need DNS are not resolved; those loops are caught at run time by the hop header (`upstream::HOP_HEADER`).
fn points_at(url: &http::Uri, addr: SocketAddr) -> bool {
    let Some(host) = url.host() else { return false };
    let port = url.port_u16().unwrap_or(if url.scheme_str() == Some("https") { 443 } else { 80 });
    if port != addr.port() {
        return false;
    }
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let local = host.eq_ignore_ascii_case("localhost");
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => {
            ip == addr.ip()
                || (addr.ip().is_unspecified() && (ip.is_loopback() || ip.is_unspecified()))
                || (addr.ip().is_loopback() && ip.is_loopback())
        }
        Err(_) => local && (addr.ip().is_loopback() || addr.ip().is_unspecified()),
    }
}

/// A running server. [`Server::shutdown`] stops it and waits; dropping it stops it too, but does
/// not wait. Stopping closes the listener, aborts every open connection (idle keep-alive sockets,
/// streams, `hang` requests) and wakes parked requests.
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
        if let Some(upstream) = &config.upstream
            && points_at(upstream, addr)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("upstream {upstream} is this server's own address ({addr}): requests would be relayed to themselves"),
            ));
        }
        let control = Arc::new(Control::new(&config).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?);
        if !config.policies.is_empty() {
            control
                .engine
                .policies
                .replace(config.policies.clone())
                .await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        }
        // The record file grows with every exchange; reloading it would reset hit counters and `once` flags.
        let record = config
            .record
            .as_ref()
            .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()));
        let watcher = config
            .watch
            .clone()
            .map(|mut w| {
                if let Some(record) = &record {
                    w.cassettes
                        .retain(|c| &std::fs::canonicalize(c).unwrap_or_else(|_| c.clone()) != record);
                }
                w
            })
            .filter(|w| !w.is_empty())
            .map(|w| watch::spawn(control.clone(), w));
        let (stop, mut stopped) = oneshot::channel();
        let shared = Arc::new(Shared {
            control: control.clone(),
            api_key: config.api_key.clone(),
            path_prefix: config.path_prefix.clone(),
            models: if config.models.is_empty() {
                DEFAULT_MODELS.iter().map(|s| (*s).to_owned()).collect()
            } else {
                config.models.clone()
            },
        });
        let stop_control = control.clone();
        let task = tokio::spawn(async move {
            let http = ConnBuilder::new(TokioExecutor::new());
            let mut conns = tokio::task::JoinSet::new();
            loop {
                let socket = tokio::select! {
                    Some(_) = conns.join_next(), if !conns.is_empty() => continue,
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
                let rst = reset_handle(&socket);
                let service = hyper::service::service_fn(move |req| {
                    let shared = shared.clone();
                    let rst = rst.clone();
                    async move { Ok::<_, Infallible>(rst_on_body_error(shared.handle(req).await, rst)) }
                });
                let conn = http.serve_connection_with_upgrades(TokioIo::new(socket), service).into_owned();
                conns.spawn(async move {
                    let _ = conn.await;
                });
            }
            conns.shutdown().await;
            if let Some(w) = watcher {
                // Await it, so a tick that is committing is finished before shutdown clears the policies.
                w.abort();
                let _ = w.await;
            }
            stop_control.release.notify_waiters();
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

    /// The registered trajectories.
    pub fn trajectories(&self) -> &Trajectories {
        &self.control.engine.trajectories
    }

    /// The loaded cassettes and the recording.
    pub fn cassettes(&self) -> &Cassettes {
        &self.control.engine.cassettes
    }

    /// The registered policy scripts.
    pub fn policies(&self) -> &Policies {
        &self.control.engine.policies
    }

    /// Clears scripts, the request log, the clock and stored responses.
    pub fn reset(&self) {
        self.control.reset();
    }

    /// Like [`Server::reset`], but [`ResetOptions`] picks what is cleared.
    pub fn reset_with(&self, opts: ResetOptions) {
        self.control.reset_with(opts);
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

    /// Closes the listener and every open connection, then waits for the accept task to end.
    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = (&mut self.task).await;
        self.control.engine.policies.clear();
    }

    /// Like [`Server::wait`] but keeps the server, so the caller can still `shutdown` it.
    pub async fn wait_ref(&mut self) {
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
    pub(crate) path_prefix: Option<String>,
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
/// A second handle on the connection's socket, kept to turn the close into an RST: the socket
/// itself belongs to hyper, which gives no way back to it. `None` when duplicating fails.
type ResetHandle = Arc<std::sync::Mutex<Option<tokio::net::TcpStream>>>;

#[cfg(unix)]
fn reset_handle(socket: &tokio::net::TcpStream) -> Option<ResetHandle> {
    use std::os::fd::AsFd;
    let std = socket.as_fd().try_clone_to_owned().ok()?;
    let std = std::net::TcpStream::from(std);
    let dup = tokio::net::TcpStream::from_std(std).ok()?;
    Some(Arc::new(std::sync::Mutex::new(Some(dup))))
}

#[cfg(not(unix))]
fn reset_handle(_: &tokio::net::TcpStream) -> Option<ResetHandle> {
    None
}

/// The faults that drop a connection fail the response body. When that happens, `SO_LINGER{on, 0}`
/// is set (on the shared socket) and the duplicate closed, so hyper's close of the original is the
/// last one and sends RST instead of FIN: clients see `ECONNRESET`, not a clean EOF. Linger is
/// never set on connections that end normally, which would lose unsent data.
fn rst_on_body_error(resp: Response, handle: Option<ResetHandle>) -> Response {
    let Some(handle) = handle else { return resp };
    let (parts, body) = resp.into_parts();
    let body = http_body_util::BodyExt::map_err(body, move |e| {
        if let Some(dup) = handle.lock().ok().and_then(|mut g| g.take()) {
            let _ = dup.set_zero_linger();
        }
        e
    });
    http::Response::from_parts(parts, http_body_util::BodyExt::boxed(body))
}

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
    http_body_util::Full::new(bytes).map_err(|e| match e {}).boxed()
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

#[cfg(test)]
mod points_at_tests {
    use super::points_at;

    fn at(url: &str, addr: &str) -> bool {
        points_at(&url.parse().unwrap(), addr.parse().unwrap())
    }

    #[test]
    fn detects_own_address() {
        assert!(at("http://127.0.0.1:8080", "127.0.0.1:8080"));
        assert!(at("http://localhost:8080/v1", "127.0.0.1:8080"));
        assert!(at("http://[::1]:8080", "[::1]:8080"));
        assert!(at("http://127.0.0.1:8080", "0.0.0.0:8080"));
        assert!(at("http://localhost/", "127.0.0.1:80"));
        assert!(!at("http://127.0.0.1:8081", "127.0.0.1:8080"));
        assert!(!at("http://example.com:8080", "127.0.0.1:8080"));
    }
}
