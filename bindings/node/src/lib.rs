//! Node.js binding: runs fakellm inside the Node process.
//!
//! A thin shell over [`fakellm::Server`]. Everything calls the library directly, in process; the
//! request and response logs use the same JSON builders as the control plane's HTTP API
//! (`Control::requests_json`), so the JS objects are exactly the JSON it serves. [`control`] holds
//! the sub-objects `scripts`, `requests`, `responses` and `clock`.

mod control;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use napi::bindgen_prelude::*;
use napi_derive::napi;

/// Options for [`start`]. Every field is optional.
#[napi(object)]
pub struct StartOptions {
    /// Address to listen on. Default `127.0.0.1`.
    pub host: Option<String>,
    /// Port to listen on. Default `0`, which picks a free port.
    pub port: Option<u16>,
    /// When set, requests must carry it as `Authorization: Bearer` or `x-api-key`; others get 401.
    pub api_key: Option<String>,
    /// Models `GET /v1/models` lists. Default: a built-in list.
    pub models: Option<Vec<String>>,
    /// Fail requests no script rule matches (400) instead of answering with the default reply.
    pub strict: Option<bool>,
    /// Rules loaded at start: an array of rule objects, or a JSON / YAML document as a string.
    pub scripts: Option<serde_json::Value>,
    /// Delay before the first token: milliseconds, or a string such as `"200ms"`. Default 0.
    pub ttft: Option<Either<f64, String>>,
    /// Streaming speed for replies that do not set their own; 0 (default) means no pacing.
    pub tokens_per_sec: Option<f64>,
    /// Length of the generated fallback text, in tokens. Default 32.
    pub fallback_tokens: Option<u32>,
    /// Tokens per streamed content chunk. Default 1.
    pub chunk_tokens: Option<u32>,
    /// Bytes of tool-call arguments per streamed delta. Default 16.
    pub args_chunk_bytes: Option<u32>,
    /// Record API requests for `server.requests`. Default `true`.
    pub request_log: Option<bool>,
    /// How many requests the log keeps; the oldest are dropped first. Default 10000.
    pub request_log_capacity: Option<u32>,
    /// Store `"<redacted>"` instead of credential header values in the request log.
    pub redact_auth: Option<bool>,
}

/// What `reset()` clears; everything by default.
#[napi(object)]
pub struct ResetOptions {
    pub clock: Option<bool>,
    pub requests: Option<bool>,
    pub scripts: Option<bool>,
    pub responses: Option<bool>,
}

/// A running in-process server.
#[napi(js_name = "FakeLlmServer")]
pub struct FakeLlmServer {
    server: Slot,
    endpoint: String,
    base_url: String,
    host: String,
    addr: SocketAddr,
}

#[napi]
impl FakeLlmServer {
    /// `http://host:port`. The Anthropic SDK wants this as `baseURL`; it appends `/v1/messages`.
    #[napi(getter)]
    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    /// `http://host:port/v1`, for the OpenAI SDK's `baseURL`.
    #[napi(getter)]
    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    /// Same as `endpoint`, named for the Anthropic SDK.
    #[napi(getter)]
    pub fn anthropic_base_url(&self) -> String {
        self.endpoint.clone()
    }

    /// The host the server is bound to.
    #[napi(getter)]
    pub fn host(&self) -> String {
        self.host.clone()
    }

    /// The port the server is bound to; the real one when `0` was requested.
    #[napi(getter)]
    pub fn port(&self) -> u32 {
        self.addr.port().into()
    }

    /// The script rules: replace, append, list with hit counts, clear.
    #[napi(getter)]
    pub fn scripts(&self) -> control::ScriptsControl {
        control::ScriptsControl::new(self.server.clone())
    }

    /// The log of API requests the server has received.
    #[napi(getter)]
    pub fn requests(&self) -> control::RequestsControl {
        control::RequestsControl::new(self.server.clone())
    }

    /// Stored Responses API objects.
    #[napi(getter)]
    pub fn responses(&self) -> control::ResponsesControl {
        control::ResponsesControl::new(self.server.clone())
    }

    /// The server's clock: freeze it, move it, or let it run.
    #[napi(getter)]
    pub fn clock(&self) -> control::ClockControl {
        control::ClockControl::new(self.server.clone())
    }

    /// Clears scripts, the request log, the clock and stored responses; `options` keeps some.
    #[napi]
    pub fn reset(&self, options: Option<ResetOptions>) -> Result<()> {
        let d = fakellm::ResetOptions::default();
        let opts = match options {
            None => d,
            Some(o) => fakellm::ResetOptions {
                clock: o.clock.unwrap_or(d.clock),
                requests: o.requests.unwrap_or(d.requests),
                scripts: o.scripts.unwrap_or(d.scripts),
                responses: o.responses.unwrap_or(d.responses),
            },
        };
        with_server(&self.server, |s| s.control().reset_with(opts))
    }

    /// What `GET /_fakellm` returns: `{ version, clock, scripts, responses, strict, request_log }`.
    #[napi]
    pub fn status(&self) -> Result<serde_json::Value> {
        with_server(&self.server, |s| s.control().status())
    }

    /// Client configuration for a tool, as `fakellm wire` prints it: `{ env, files, snippet }`.
    /// With `format` (`env`, `json`, `toml` or `shell`) it returns that rendering as a string instead.
    /// Targets: claude, codex, opencode, pi, openai-sdk, ai-sdk, anthropic-sdk.
    #[napi]
    pub fn wire(&self, target: String, format: Option<String>) -> Result<serde_json::Value> {
        let out = fakellm::wire::wire(&target, &self.endpoint).map_err(|e| Error::new(Status::InvalidArg, e))?;
        if let Some(f) = format.filter(|f| f != "json") {
            let f: fakellm::wire::Format = f.parse().map_err(control_error)?;
            return fakellm::wire::format(&out, f).map(serde_json::Value::String).map_err(control_error);
        }
        let map = |pairs: Vec<(String, String)>| {
            pairs
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect::<serde_json::Map<_, _>>()
        };
        Ok(serde_json::json!({ "env": map(out.env), "files": map(out.files), "snippet": out.snippet }))
    }

    /// Closes the listener and every open connection (idle keep-alive sockets, streams, hung
    /// requests), which releases the port. Idempotent. A server object that is garbage-collected
    /// without `stop()` is stopped too, so keep a reference for as long as the URL is in use.
    #[napi]
    pub fn stop<'env>(&self, env: &'env Env) -> Result<PromiseRaw<'env, ()>> {
        let slot = self.server.clone();
        env.spawn_future(async move {
            let server = slot.lock().map_err(poisoned)?.take();
            if let Some(server) = server {
                server.shutdown().await;
            }
            Ok(())
        })
    }
}

/// The server shared with the objects handed out by `scripts`, `requests` and so on; `None` once stopped.
pub(crate) type Slot = Arc<Mutex<Option<fakellm::Server>>>;

/// Runs `f` on the running server. Fails once the server is stopped.
pub(crate) fn with_server<T>(slot: &Slot, f: impl FnOnce(&fakellm::Server) -> T) -> Result<T> {
    let guard = slot.lock().map_err(poisoned)?;
    match guard.as_ref() {
        Some(server) => Ok(f(server)),
        None => Err(Error::new(Status::GenericFailure, "server is stopped")),
    }
}

pub(crate) fn poisoned<T>(_: T) -> Error {
    Error::new(Status::GenericFailure, "server state is poisoned")
}

pub(crate) fn control_error(e: impl std::fmt::Display) -> Error {
    Error::new(Status::InvalidArg, e.to_string())
}

/// A duration given as milliseconds or as text such as `"200ms"`, in nanoseconds.
pub(crate) fn nanos(d: Either<f64, String>) -> Result<i64> {
    match d {
        Either::A(ms) if ms.is_finite() => Ok((ms * 1e6) as i64),
        Either::A(ms) => Err(control_error(format!("invalid duration: {ms}"))),
        Either::B(s) => fakellm::parse_duration(&s).map_err(control_error),
    }
}

/// Starts a server on the Node process's async runtime and resolves once it is listening.
#[napi]
pub async fn start(options: Option<StartOptions>) -> Result<FakeLlmServer> {
    let mut config = fakellm::Config::default();
    if let Some(o) = options {
        config.host = o.host.unwrap_or(config.host);
        config.port = o.port.unwrap_or(config.port);
        config.api_key = o.api_key.or(config.api_key);
        config.models = o.models.unwrap_or(config.models);
        config.strict = o.strict.unwrap_or(config.strict);
        config.request_log = o.request_log.unwrap_or(config.request_log);
        config.request_log_capacity = o.request_log_capacity.map_or(config.request_log_capacity, |n| n as usize);
        config.redact_auth = o.redact_auth.unwrap_or(config.redact_auth);
        if let Some(v) = o.scripts {
            config.scripts = control::rules(v)?;
        }
        if let Some(t) = o.ttft {
            let n = nanos(t)?;
            if n < 0 {
                return Err(control_error("ttft must not be negative"));
            }
            config.delivery.ttft = std::time::Duration::from_nanos(n as u64);
        }
        if let Some(n) = o.fallback_tokens {
            config.fallback.tokens = n;
        }
        if let Some(n) = o.chunk_tokens {
            config.delivery.chunk_tokens = n as usize;
        }
        if let Some(n) = o.args_chunk_bytes {
            config.delivery.args_chunk_bytes = n as usize;
        }
        if let Some(tps) = o.tokens_per_sec {
            config.delivery.token_interval = if tps > 0.0 {
                std::time::Duration::try_from_secs_f64(1.0 / tps).unwrap_or(std::time::Duration::MAX)
            } else {
                std::time::Duration::ZERO
            };
        }
    }
    let server = fakellm::Server::start(config.clone()).await.map_err(|e| {
        Error::new(
            Status::GenericFailure,
            format!("failed to start fakellm on {}:{}: {e}", config.host, config.port),
        )
    })?;
    let mut addr = server.addr();
    if addr.ip().is_unspecified() {
        addr.set_ip(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    Ok(FakeLlmServer {
        endpoint: server.endpoint(),
        base_url: server.base_url(),
        host: config.host,
        addr,
        server: Arc::new(Mutex::new(Some(server))),
    })
}
