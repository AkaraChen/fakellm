//! Node.js binding: runs fakellm inside the Node process.
//!
//! A thin shell over [`fakellm::Server`]. Everything calls the library directly, in process; the
//! request and response logs use the same JSON builders as the control plane's HTTP API
//! (`Control::requests_json`), so the JS objects are exactly the JSON it serves. [`control`] holds
//! the sub-objects `scripts`, `trajectories`, `cassettes`, `snapshots`, `policies`, `requests`, `responses` and `clock`.

mod control;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock};

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
    /// Trajectories registered at start: an array of trajectory objects, or a JSON / YAML / JSONL string.
    pub trajectories: Option<serde_json::Value>,
    /// Cassettes loaded at start: an array of cassette objects, or a JSON / YAML / JSONL string.
    pub cassettes: Option<serde_json::Value>,
    /// Relay requests no rule and no cassette answers to this OpenAI-compatible or Anthropic server
    /// (`http://` or `https://`) and record what it says; see `server.cassettes` for the recording.
    pub upstream: Option<String>,
    /// Key sent to the upstream instead of the client's own credentials. Needs `upstream`.
    pub upstream_api_key: Option<String>,
    /// Append every recorded exchange to this JSONL file. Needs `upstream`.
    pub record: Option<String>,
    /// Regexes: every match in the recorded texts (messages, tool arguments and results, replies) becomes
    /// `<redacted>`. Needs `upstream`.
    pub record_redact: Option<Vec<String>>,
    /// Rules loaded from this JSON / YAML file at start; rules from `scripts` are appended. A reload of the
    /// watched file replaces all rules with the file's.
    pub scripts_file: Option<String>,
    /// Trajectory files (JSON, YAML or JSONL) loaded at start, in order.
    pub trajectory_files: Option<Vec<String>>,
    /// Policy modules loaded at start; each is named after its file stem and uses `policyRuntime`.
    pub policy_files: Option<Vec<String>>,
    /// Cassette files (JSON, JSONL or YAML) loaded at start, in order.
    pub cassette_files: Option<Vec<String>>,
    /// Re-read files when they change and replace what they loaded. `true` or unset: every file given as
    /// `scriptsFile`, `trajectoryFiles`, `policyFiles` or `cassetteFiles` is watched (as the CLI and the
    /// JS SDK do); `false`: nothing; an object names the files to watch explicitly.
    pub watch: Option<Either<bool, WatchOptions>>,
    /// Policies registered at start: an array of `{ name, source, runtime? }`, or a JSON / YAML string.
    /// A module that does not load rejects `start()`.
    pub policies: Option<serde_json::Value>,
    /// Runtime for policies that name none: `tjs`, `node`, `bun` or a path. Default: whichever of
    /// tjs, node, bun is on PATH first.
    pub policy_runtime: Option<String>,
    /// How long a policy may take to decide, in milliseconds on the host clock. Default 5000.
    pub policy_timeout_ms: Option<u32>,
    /// Most named snapshots held at once; the next `snapshots.create()` throws. Default 100.
    pub max_snapshots: Option<u32>,
    /// Also record the upstream's 401, 403, 408, 409, 429 and 5xx answers. Needs `upstream`.
    pub record_errors: Option<bool>,
    /// Seconds the upstream may take to send response headers; 0 waits without limit. Default 600.
    pub upstream_timeout: Option<u32>,
    /// Also serve the API under this path prefix, e.g. `/anthropic`.
    pub path_prefix: Option<String>,
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

/// Files to reload on change (`StartOptions.watch`). Each kind reloads when any of its files changed.
#[napi(object)]
pub struct WatchOptions {
    /// A JSON / YAML rules file.
    pub scripts: Option<String>,
    pub trajectories: Option<Vec<String>>,
    pub policies: Option<Vec<String>>,
    pub cassettes: Option<Vec<String>>,
}

/// What `reset()` clears; everything by default.
#[napi(object)]
pub struct ResetOptions {
    pub clock: Option<bool>,
    pub requests: Option<bool>,
    pub scripts: Option<bool>,
    pub responses: Option<bool>,
    /// Also delete all snapshots. Default `false`.
    pub snapshots: Option<bool>,
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

    /// Trajectories: scripted agent runs replayed one step per request.
    #[napi(getter)]
    pub fn trajectories(&self) -> control::TrajectoriesControl {
        control::TrajectoriesControl::new(self.server.clone())
    }

    /// Cassettes: recorded exchanges that replay through every protocol.
    #[napi(getter)]
    pub fn cassettes(&self) -> control::CassettesControl {
        control::CassettesControl::new(self.server.clone())
    }

    /// Snapshots: save the state under a name, restore it between tests.
    #[napi(getter)]
    pub fn snapshots(&self) -> control::SnapshotsControl {
        control::SnapshotsControl::new(self.server.clone())
    }

    /// Policy scripts: JS modules that decide each reply.
    #[napi(getter)]
    pub fn policies(&self) -> control::PoliciesControl {
        control::PoliciesControl::new(self.server.clone())
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
                snapshots: o.snapshots.unwrap_or(d.snapshots),
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
            let server = slot.write().map_err(poisoned)?.take();
            if let Some(server) = server {
                server.shutdown().await;
            }
            Ok(())
        })
    }
}

/// The server shared with the objects handed out by `scripts`, `requests` and so on; `None` once stopped.
///
/// A read lock is held while a server method runs; slow work on a worker thread (loading policies) takes a handle first and
/// releases the lock, so the JS thread is never kept waiting behind a pending `stop()`.
pub(crate) type Slot = Arc<RwLock<Option<fakellm::Server>>>;

/// Runs `f` on the running server. Fails once the server is stopped.
pub(crate) fn with_server<T>(slot: &Slot, f: impl FnOnce(&fakellm::Server) -> T) -> Result<T> {
    let guard = slot.read().map_err(poisoned)?;
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
        let files = control::load_files(
            o.scripts_file.as_deref(),
            o.trajectory_files.as_deref().unwrap_or_default(),
            o.policy_files.as_deref().unwrap_or_default(),
            o.cassette_files.as_deref().unwrap_or_default(),
        )?;
        config.scripts = files.scripts;
        config.trajectories = files.trajectories;
        config.policies = files.policies;
        config.cassettes = files.cassettes;
        if let Some(v) = o.scripts {
            config.scripts.extend(control::rules(v)?);
        }
        if let Some(v) = o.trajectories {
            config.trajectories.extend(control::trajectories(v)?);
        }
        if let Some(v) = o.policies {
            config.policies.extend(control::policies(v)?);
        }
        if let Some(r) = o.policy_runtime {
            config.policy_runtime = Some(r.parse().map_err(control_error)?);
        }
        if let Some(v) = o.cassettes {
            config.cassettes.extend(control::cassettes(v)?);
        }
        if let Some(u) = o.upstream {
            config.upstream = Some(u.parse().map_err(|e| control_error(format!("invalid upstream {u:?}: {e}")))?);
        }
        config.upstream_api_key = o.upstream_api_key.or(config.upstream_api_key);
        config.record = o.record.map(Into::into).or(config.record);
        config.record_redact = o.record_redact.unwrap_or(config.record_redact);
        let strings = |v: Option<Vec<String>>| v.unwrap_or_default().into_iter().map(Into::into).collect::<Vec<_>>();
        config.watch = match o.watch {
            Some(Either::B(w)) => Some(fakellm::WatchConfig {
                scripts: w.scripts.map(Into::into),
                trajectories: strings(w.trajectories),
                policies: strings(w.policies),
                cassettes: strings(w.cassettes),
            }),
            Some(Either::A(false)) => None,
            Some(Either::A(true)) | None => {
                let w = fakellm::WatchConfig {
                    scripts: o.scripts_file.map(Into::into),
                    trajectories: strings(o.trajectory_files),
                    policies: strings(o.policy_files),
                    cassettes: strings(o.cassette_files),
                };
                (!w.is_empty()).then_some(w)
            }
        };
        config.path_prefix = o.path_prefix.or(config.path_prefix);
        config.policy_timeout_ms = o.policy_timeout_ms.map_or(config.policy_timeout_ms, u64::from);
        config.max_snapshots = o.max_snapshots.map_or(config.max_snapshots, |n| n as usize);
        config.record_errors = o.record_errors.unwrap_or(config.record_errors);
        config.upstream_timeout_secs = o.upstream_timeout.map_or(config.upstream_timeout_secs, u64::from);
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
        server: Arc::new(RwLock::new(Some(server))),
    })
}
