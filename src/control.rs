//! The test control plane: reset, scripts, clock and request log, as methods on [`Control`]
//! (reachable through `Server`) and as an HTTP API under `/_fakellm/` (see docs/design.md).
//! No real API path starts with `/_fakellm`, so there is no collision.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use http_body_util::{BodyExt, Limited};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::trajectory::{Trajectory, TrajectoryFormat};
use crate::cassette::format::{Cassette, CassetteFormat};
use crate::clock::{self, Clock};
use crate::ir::{Outcome, Protocol, Source};
use crate::openai::responses_store::ResponseStore;
use crate::policy::{Policies, PolicySpec};
use crate::reqlog::{RequestFilter, RequestLog, RequestRecord, StatusFilter};
use crate::script::{Engine, Rule, ScriptFormat};
use crate::snapshot::Snapshots;
use crate::upstream::Upstream;
use crate::{Config, Response, json_response};

pub const PREFIX: &str = "/_fakellm";

/// Request bodies larger than this are refused.
pub(crate) const MAX_BODY: usize = 4 << 20;

/// Shared state behind one server's control plane.
pub struct Control {
    pub(crate) clock: Arc<Clock>,
    pub(crate) log: RequestLog,
    pub(crate) engine: Engine,
    /// Stored Responses API objects, see `openai::responses_store`.
    pub(crate) responses: ResponseStore,
    /// Named snapshots of the state above, see `crate::snapshot`.
    pub(crate) snapshots: Snapshots,
    /// Where requests that nothing else answers go, when `Config::upstream` is set.
    pub(crate) upstream: Option<Upstream>,
    /// Serializes everything that replaces or restores registries: mutating control-plane routes (except the snapshot
    /// ones, whose methods take it themselves), hot reload and snapshot restore. See docs/m4-spec.md section 9.
    admin: tokio::sync::Mutex<()>,
    /// Mixed into response ids and `x-request-id` so that identical requests (retries, parallel
    /// agents) get distinct ids, while a fresh or reset server repeats the same sequence.
    counter: AtomicU64,
    /// Bumped by every reset; requests parked across one must not write into the cleared log.
    generation: AtomicU64,
    /// Woken by every reset so that hung requests finish.
    pub(crate) release: tokio::sync::Notify,
}

/// What [`Control::reset_with`] clears. Everything is cleared by default.
#[derive(Clone, Copy, Debug)]
pub struct ResetOptions {
    /// Put the clock back on the host clock.
    pub clock: bool,
    /// Empty the request log.
    pub requests: bool,
    /// Remove all script rules, trajectories and policies (policy subprocesses are stopped).
    pub scripts: bool,
    /// Forget stored Responses API objects.
    pub responses: bool,
    /// Also delete all snapshots. Off by default, so "take a baseline snapshot, reset or restore before each
    /// test" works.
    pub snapshots: bool,
}

impl Default for ResetOptions {
    fn default() -> Self {
        Self {
            clock: true,
            requests: true,
            scripts: true,
            responses: true,
            snapshots: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    BadRequest(String),
    NotFound(String),
    /// 409: the name is taken (snapshots).
    Conflict(String),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRequest(m) | Self::NotFound(m) | Self::Conflict(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ControlError {}

impl From<crate::clock::OutOfRange> for ControlError {
    fn from(e: crate::clock::OutOfRange) -> Self {
        Self::BadRequest(e.to_string())
    }
}

impl From<crate::agent::trajectory::TrajectoryError> for ControlError {
    fn from(e: crate::agent::trajectory::TrajectoryError) -> Self {
        Self::BadRequest(e.to_string())
    }
}

impl From<crate::cassette::CassetteError> for ControlError {
    fn from(e: crate::cassette::CassetteError) -> Self {
        Self::BadRequest(e.to_string())
    }
}

impl From<crate::policy::PolicyError> for ControlError {
    fn from(e: crate::policy::PolicyError) -> Self {
        Self::BadRequest(e.to_string())
    }
}

impl From<crate::script::ScriptError> for ControlError {
    fn from(e: crate::script::ScriptError) -> Self {
        Self::BadRequest(e.to_string())
    }
}

impl Control {
    /// Builds the state for a server; fails if `config.scripts` has an invalid rule.
    pub(crate) fn new(config: &Config) -> Result<Self, ControlError> {
        let policies = Policies::new(config.policy_runtime.clone(), Duration::from_millis(config.policy_timeout_ms));
        let engine = Engine::new(config.fallback.clone(), config.strict, config.delivery.clone()).with_policies(policies);
        engine.scripts.replace(config.scripts.clone())?;
        if !config.trajectories.is_empty() {
            engine.trajectories.replace(config.trajectories.clone())?;
        }
        if !config.cassettes.is_empty() {
            engine.cassettes.replace(config.cassettes.clone())?;
        }
        let upstream = match &config.upstream {
            Some(base) => {
                engine.cassettes.start_recording(Some(base.to_string()));
                if config.api_key.is_some() && config.upstream_api_key.is_none() {
                    tracing::warn!(
                        target: "fakellm::upstream",
                        "--api-key is set without --upstream-api-key: fakellm's own key is not forwarded, so relayed requests reach {base} without credentials"
                    );
                }
                Some(
                    Upstream::new(base, config.upstream_api_key.clone(), config.record.as_deref())
                        .map_err(ControlError::BadRequest)?
                        .with_local_key(config.api_key.clone())
                        .with_record_errors(config.record_errors)
                        .with_header_timeout_secs(config.upstream_timeout_secs),
                )
            }
            None if config.record.is_some() => return Err(ControlError::BadRequest("`record` needs `upstream`".into())),
            None => None,
        };
        let log = RequestLog::new(config.request_log, config.request_log_capacity);
        log.set_redact_auth(config.redact_auth);
        Ok(Self {
            clock: Arc::default(),
            log,
            engine,
            responses: ResponseStore::default(),
            snapshots: Snapshots::default(),
            upstream,
            admin: tokio::sync::Mutex::new(()),
            counter: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            release: tokio::sync::Notify::new(),
        })
    }

    /// The admin lock. Hold it for the whole of a registry replacement or restore (parse and prepare first, commit
    /// last) so that PUTs, hot reloads and snapshot restores never interleave. Not reentrant.
    pub(crate) async fn exclusive(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.admin.lock().await
    }

    /// Clears scripts, the request log and the clock.
    pub fn reset(&self) {
        self.reset_with(ResetOptions::default());
    }

    pub fn reset_with(&self, opts: ResetOptions) {
        if opts.scripts {
            self.engine.scripts.clear();
            self.engine.trajectories.clear();
            self.engine.policies.clear();
            self.engine.cassettes.clear_all();
        }
        if opts.snapshots {
            self.snapshots.clear();
        }
        if opts.clock {
            self.clock.reset();
        }
        if opts.requests {
            self.generation.fetch_add(1, Ordering::Relaxed);
            self.log.clear();
        }
        if opts.responses {
            self.responses.clear();
        }
        self.release.notify_waiters();
        // Response ids derive from the counter; restarting it while stored responses survive
        // would let a replayed request overwrite an old entry with the same id.
        if opts.requests && opts.responses {
            self.counter.store(0, Ordering::Relaxed);
        }
    }

    /// What `GET /_fakellm` returns: version, clock, rule and response counts, strict flag, log settings.
    pub fn status(&self) -> Value {
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "clock": clock_json(&self.clock),
            "scripts": { "count": self.engine.scripts.len() },
            "trajectories": { "count": self.engine.trajectories.len() },
            "policies": { "count": self.engine.policies.len() },
            "cassettes": { "count": self.engine.cassettes.len() },
            "snapshots": { "count": self.snapshots.len() },
            "upstream": self.upstream.as_ref().map(|u| json!({
                "url": u.display_url(),
                "record": u.recorder().map(|r| r.path().display().to_string()),
                "recorded": self.engine.cassettes.recording().exchanges.len(),
            })),
            "responses": { "count": self.responses.len() },
            "strict": self.engine.strict,
            "request_log": log_config_json(&self.log),
        })
    }

    /// What `GET /_fakellm/requests?<query>` returns, for query pairs such as `("status", "4xx")`.
    pub fn requests_json(&self, query: &[(String, String)]) -> Result<Value, ControlError> {
        let full = query_flag(query, "full", false)?;
        let filter = request_filter(query)?;
        let (rows, total) = self.requests(&filter);
        Ok(json!({
            "requests": rows.iter().map(|r| record_json(r, full)).collect::<Vec<_>>(),
            "total": total,
            "dropped": self.log.dropped(),
        }))
    }

    /// What `GET /_fakellm/responses` returns.
    pub fn responses_json(&self) -> Value {
        let items = self.response_ids();
        json!({
            "responses": items.iter().map(|(id, at)| json!({ "id": id, "created_at": fmt_unix(*at) })).collect::<Vec<_>>(),
            "count": items.len(),
        })
    }

    /// The request log's `{enabled, capacity, stored, dropped}`.
    pub fn log_config(&self) -> Value {
        log_config_json(&self.log)
    }

    /// Switches the request log on or off and/or resizes it (at least 1), then returns [`Control::log_config`].
    pub fn set_log_config(&self, enabled: Option<bool>, capacity: Option<usize>) -> Result<Value, ControlError> {
        if capacity == Some(0) {
            return Err(ControlError::BadRequest("capacity must be at least 1".into()));
        }
        if let Some(on) = enabled {
            self.log.set_enabled(on);
        }
        if let Some(c) = capacity {
            self.log.set_capacity(c);
        }
        Ok(log_config_json(&self.log))
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// The id counter's current value, for snapshots.
    pub(crate) fn counter_value(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }

    /// Puts the id counter back to `value`, for snapshots.
    pub(crate) fn set_counter(&self, value: u64) {
        self.counter.store(value, Ordering::Relaxed);
    }

    /// The next value of the per-server id counter, starting at 0.
    pub(crate) fn next_salt(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn scripts(&self) -> &crate::script::Scripts {
        &self.engine.scripts
    }

    pub fn trajectories(&self) -> &crate::agent::trajectory::Trajectories {
        &self.engine.trajectories
    }

    pub fn policies(&self) -> &Policies {
        &self.engine.policies
    }

    pub fn cassettes(&self) -> &crate::cassette::Cassettes {
        &self.engine.cassettes
    }

    /// What `GET /_fakellm/snapshots` returns.
    pub fn snapshots_json(&self) -> Value {
        json!({ "snapshots": self.snapshots.list() })
    }

    /// Deletes a snapshot; `NotFound` when there is none by that name.
    pub fn delete_snapshot(&self, name: &str) -> Result<(), ControlError> {
        if self.snapshots.delete(name) {
            Ok(())
        } else {
            Err(ControlError::NotFound(format!("no snapshot named {name:?}")))
        }
    }

    /// Logged requests matching `filter`, oldest first, and the match count before `limit`.
    pub fn requests(&self, filter: &RequestFilter) -> (Vec<RequestRecord>, usize) {
        self.log.list(filter)
    }

    pub fn clear_requests(&self) {
        self.log.clear();
    }

    /// Ids and creation times (unix seconds) of the stored Responses API objects.
    pub fn response_ids(&self) -> Vec<(String, i64)> {
        self.responses.list()
    }

    /// Forgets all stored Responses API objects and returns how many there were.
    pub fn clear_responses(&self) -> usize {
        let n = self.responses.len();
        self.responses.clear();
        n
    }

    /// Answers a request under [`PREFIX`]. Routes (see docs/design.md):
    ///
    /// - `GET  /_fakellm`                      status: version, clock, scripts count, log config
    /// - `POST /_fakellm/reset?clock=&requests=&scripts=&responses=&snapshots=`   booleans, all default true except `snapshots`
    /// - `GET  /_fakellm/scripts`              `{"rules": [RuleInfo...]}`
    /// - `PUT  /_fakellm/scripts[?keep_hits=true]`  replace; body is a JSON array / `{"rules":[...]}`,
    ///   or YAML when `Content-Type` is `application/yaml` or `text/yaml`
    /// - `POST /_fakellm/scripts`              append, same body
    /// - `DELETE /_fakellm/scripts`            clear
    /// - `GET  /_fakellm/trajectories`         `{"trajectories": [TrajectoryInfo...]}`
    /// - `PUT  /_fakellm/trajectories`         replace; body is a trajectory object, a JSON/YAML array of them,
    ///   `{"trajectories":[...]}`, or JSONL (`Content-Type: application/x-ndjson`)
    /// - `POST /_fakellm/trajectories`         add, a same-named trajectory is replaced in place
    /// - `DELETE /_fakellm/trajectories`       clear
    /// - `GET  /_fakellm/trajectories/{name}[?format=jsonl]`, `DELETE /_fakellm/trajectories/{name}`
    /// - `GET  /_fakellm/cassettes`            `{"cassettes": [CassetteInfo...]}` (the recording last, when it exists)
    /// - `PUT|POST /_fakellm/cassettes`        replace / add; body is a cassette, an array, `{"cassettes":[...]}` or JSONL
    /// - `DELETE /_fakellm/cassettes`          clear the registered cassettes (not the recording)
    /// - `GET  /_fakellm/cassettes/{name}[?format=jsonl]`, `DELETE /_fakellm/cassettes/{name}`; the name `recording`
    ///   is the in-memory recording
    /// - `POST /_fakellm/snapshots`            `{"name"?}` take a snapshot (201; 409 when the name is taken)
    /// - `GET  /_fakellm/snapshots[/{name}]`, `DELETE /_fakellm/snapshots[/{name}]`,
    ///   `POST /_fakellm/snapshots/{name}/restore`
    /// - `GET  /_fakellm/policies`             `{"policies": [PolicyInfo...]}`
    /// - `PUT|POST /_fakellm/policies`         body is `{name, source, runtime?}`, an array of them or
    ///   `{"policies":[...]}` (JSON or YAML); every module is dry-run loaded first, a load error is a 400
    /// - `DELETE /_fakellm/policies`, `GET|DELETE /_fakellm/policies/{name}`
    /// - `GET  /_fakellm/clock`, `POST /_fakellm/clock/{freeze,unfreeze,set,advance,reset}`
    ///   bodies `{"at": "<RFC 3339>"}` / `{"by": "<duration>"}`, same as fakes3
    /// - `GET  /_fakellm/responses`, `DELETE /_fakellm/responses`  stored Responses API objects
    /// - `GET  /_fakellm/requests?protocol=&model=&has_tool=&stream=&status=&source=&rule=&path=&since=&after=&limit=&full=`
    ///   `{"requests": [...], "total": n, "dropped": n}`; each record has `seq, time, method,
    ///   path, status, latency_us, protocol, model, stream, source, rule, request (IR), reply`
    ///   (`{finish, text, tool_calls:[{id,name,arguments}], usage}` or `error`); `full=true`
    ///   adds `headers` and `raw`
    /// - `DELETE /_fakellm/requests`, `GET/PUT /_fakellm/requests/config` (`{"enabled","capacity"}`)
    ///
    /// Non-GET requests carrying an `Origin` header get 403, like fakes3. Errors are
    /// `{"error": {"code": "BadRequest"|"NotFound"|"MethodNotAllowed"|"TooLarge"|"Forbidden", "message"}}`.
    pub(crate) async fn handle(&self, req: http::Request<hyper::body::Incoming>) -> Response {
        let (parts, body) = req.into_parts();
        // A web page can make a browser send simple cross-origin POSTs to localhost; refuse them
        // so that visiting a page cannot reset the server. Test tooling never sends `Origin`.
        if parts.method != http::Method::GET && parts.headers.contains_key(http::header::ORIGIN) {
            return error(403, "Forbidden", "cross-origin requests are not allowed");
        }
        let body = match Limited::new(body, MAX_BODY).collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return error(413, "TooLarge", "request body too large"),
        };
        let query: Query = form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
        let formats = Formats {
            script: script_format(&parts.headers),
            trajectory: trajectory_format(&parts.headers),
            cassette: cassette_format(&parts.headers),
        };
        self.route(&parts.method, parts.uri.path(), &query, &body, formats)
            .await
            .unwrap_or_else(Response::from)
    }

    async fn route(&self, method: &http::Method, path: &str, q: &Query, body: &[u8], formats: Formats) -> Result<Response, ControlError> {
        let Formats {
            script: format,
            trajectory: tformat,
            cassette: cformat,
        } = formats;
        use http::Method as M;
        let rest = path.strip_prefix(PREFIX).unwrap_or("").trim_end_matches('/');
        let segs: Vec<&str> = rest.split('/').skip(1).collect();
        let not_allowed = || Ok(error(405, "MethodNotAllowed", format!("{method} is not allowed on {path}")));
        // Snapshot and restore take the lock inside `snapshot()` / `restore()`, which library users call directly too.
        let _admin = if *method != M::GET && segs.first() != Some(&"snapshots") {
            Some(self.exclusive().await)
        } else {
            None
        };
        match segs.as_slice() {
            [""] | [] => match *method {
                M::GET => Ok(reply(200, self.status())),
                _ => not_allowed(),
            },
            ["reset"] => match *method {
                M::POST => {
                    let defaults = ResetOptions::default();
                    self.reset_with(ResetOptions {
                        clock: query_flag(q, "clock", defaults.clock)?,
                        requests: query_flag(q, "requests", defaults.requests)?,
                        scripts: query_flag(q, "scripts", defaults.scripts)?,
                        responses: query_flag(q, "responses", defaults.responses)?,
                        snapshots: query_flag(q, "snapshots", defaults.snapshots)?,
                    });
                    Ok(reply(200, json!({ "reset": true })))
                }
                _ => not_allowed(),
            },
            ["scripts"] => match *method {
                M::GET => Ok(reply(200, json!({ "rules": self.engine.scripts.list() }))),
                M::PUT | M::POST => {
                    let text = std::str::from_utf8(body).map_err(|_| ControlError::BadRequest("body must be UTF-8".into()))?;
                    let rules = Rule::parse_many(text, format)?;
                    let count = rules.len();
                    if *method == M::PUT {
                        if query_flag(q, "keep_hits", false)? {
                            self.engine.scripts.replace_keep_hits(rules)?;
                        } else {
                            self.engine.scripts.replace(rules)?;
                        }
                    } else {
                        self.engine.scripts.append(rules)?;
                    }
                    Ok(reply(200, json!({ "added": count, "count": self.engine.scripts.len() })))
                }
                M::DELETE => {
                    self.engine.scripts.clear();
                    Ok(reply(200, json!({ "cleared": true })))
                }
                _ => not_allowed(),
            },
            ["trajectories"] => match *method {
                M::GET => Ok(reply(200, json!({ "trajectories": self.engine.trajectories.list() }))),
                M::PUT | M::POST => {
                    let text = std::str::from_utf8(body).map_err(|_| ControlError::BadRequest("body must be UTF-8".into()))?;
                    let items = Trajectory::parse_many(text, tformat)?;
                    let count = items.len();
                    if *method == M::PUT {
                        self.engine.trajectories.replace(items)?;
                    } else {
                        self.engine.trajectories.upsert(items)?;
                    }
                    Ok(reply(200, json!({ "added": count, "count": self.engine.trajectories.len() })))
                }
                M::DELETE => {
                    self.engine.trajectories.clear();
                    Ok(reply(200, json!({ "cleared": true })))
                }
                _ => not_allowed(),
            },
            ["trajectories", name] => {
                let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy();
                match *method {
                    M::GET => {
                        let t = self
                            .engine
                            .trajectories
                            .get(&name)
                            .ok_or_else(|| ControlError::NotFound(format!("no trajectory named {name:?}")))?;
                        match q.iter().find(|(k, _)| k == "format").map(|(_, v)| v.as_str()) {
                            None | Some("json") => Ok(reply(200, t.to_json())),
                            Some("jsonl") => Ok(text_response(200, "application/x-ndjson", t.to_jsonl())),
                            Some(_) => Err(ControlError::BadRequest("format must be json or jsonl".into())),
                        }
                    }
                    M::DELETE if self.engine.trajectories.remove(&name) => Ok(reply(200, json!({ "deleted": true }))),
                    M::DELETE => Err(ControlError::NotFound(format!("no trajectory named {name:?}"))),
                    _ => not_allowed(),
                }
            }
            ["cassettes"] => match *method {
                M::GET => Ok(reply(200, json!({ "cassettes": self.engine.cassettes.list() }))),
                M::PUT | M::POST => {
                    let text = std::str::from_utf8(body).map_err(|_| ControlError::BadRequest("body must be UTF-8".into()))?;
                    let items = Cassette::parse_many(text, cformat)?;
                    let count = items.len();
                    if *method == M::PUT {
                        self.engine.cassettes.replace(items)?;
                    } else {
                        self.engine.cassettes.upsert(items)?;
                    }
                    Ok(reply(200, json!({ "added": count, "count": self.engine.cassettes.len() })))
                }
                M::DELETE => {
                    self.engine.cassettes.clear();
                    Ok(reply(200, json!({ "cleared": true })))
                }
                _ => not_allowed(),
            },
            ["cassettes", name] => {
                let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy();
                match *method {
                    M::GET => {
                        let c = self
                            .engine
                            .cassettes
                            .get(&name)
                            .ok_or_else(|| ControlError::NotFound(format!("no cassette named {name:?}")))?;
                        match q.iter().find(|(k, _)| k == "format").map(|(_, v)| v.as_str()) {
                            None | Some("json") => Ok(reply(200, c.to_json())),
                            Some("jsonl") => Ok(text_response(200, "application/x-ndjson", c.to_jsonl())),
                            Some(_) => Err(ControlError::BadRequest("format must be json or jsonl".into())),
                        }
                    }
                    M::DELETE if self.engine.cassettes.remove(&name) => Ok(reply(200, json!({ "deleted": true }))),
                    M::DELETE => Err(ControlError::NotFound(format!("no cassette named {name:?}"))),
                    _ => not_allowed(),
                }
            }
            ["snapshots"] => match *method {
                M::GET => Ok(reply(200, self.snapshots_json())),
                M::POST => {
                    let b: SnapshotBody = parse_body(body)?;
                    let name = b.name.or_else(|| q.iter().find(|(k, _)| k == "name").map(|(_, v)| v.clone()));
                    Ok(reply(
                        201,
                        serde_json::to_value(self.snapshot(name.as_deref()).await?).unwrap_or(Value::Null),
                    ))
                }
                M::DELETE => {
                    self.snapshots.clear();
                    Ok(reply(200, json!({ "cleared": true })))
                }
                _ => not_allowed(),
            },
            ["snapshots", name] => {
                let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy();
                match *method {
                    M::GET => {
                        let info = self
                            .snapshots
                            .info(&name)
                            .ok_or_else(|| ControlError::NotFound(format!("no snapshot named {name:?}")))?;
                        Ok(reply(200, serde_json::to_value(info).unwrap_or(Value::Null)))
                    }
                    M::DELETE => {
                        self.delete_snapshot(&name)?;
                        Ok(reply(200, json!({ "deleted": true })))
                    }
                    _ => not_allowed(),
                }
            }
            ["snapshots", name, "restore"] => {
                if *method != M::POST {
                    return not_allowed();
                }
                let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy();
                Ok(reply(200, serde_json::to_value(self.restore(&name).await?).unwrap_or(Value::Null)))
            }
            ["policies"] => match *method {
                M::GET => Ok(reply(200, json!({ "policies": self.engine.policies.list() }))),
                M::PUT | M::POST => {
                    let text = std::str::from_utf8(body).map_err(|_| ControlError::BadRequest("body must be UTF-8".into()))?;
                    let specs = PolicySpec::parse_many(text, format)?;
                    let count = specs.len();
                    if *method == M::PUT {
                        self.engine.policies.replace(specs).await?;
                    } else {
                        self.engine.policies.upsert(specs).await?;
                    }
                    Ok(reply(200, json!({ "added": count, "count": self.engine.policies.len() })))
                }
                M::DELETE => {
                    self.engine.policies.clear();
                    Ok(reply(200, json!({ "cleared": true })))
                }
                _ => not_allowed(),
            },
            ["policies", name] => {
                let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy();
                match *method {
                    M::GET => {
                        let p = self
                            .engine
                            .policies
                            .get(&name)
                            .ok_or_else(|| ControlError::NotFound(format!("no policy named {name:?}")))?;
                        Ok(reply(200, serde_json::to_value(p).unwrap_or(Value::Null)))
                    }
                    M::DELETE if self.engine.policies.remove(&name) => Ok(reply(200, json!({ "deleted": true }))),
                    M::DELETE => Err(ControlError::NotFound(format!("no policy named {name:?}"))),
                    _ => not_allowed(),
                }
            }
            ["responses"] => match *method {
                M::GET => Ok(reply(200, self.responses_json())),
                M::DELETE => Ok(reply(200, json!({ "deleted": self.clear_responses() }))),
                _ => not_allowed(),
            },
            ["clock"] => match *method {
                M::GET => Ok(reply(200, clock_json(&self.clock))),
                _ => not_allowed(),
            },
            ["clock", action] => {
                if *method != M::POST {
                    return not_allowed();
                }
                match *action {
                    "freeze" => {
                        let b: AtBody = parse_body(body)?;
                        let at = b.at.as_deref().map(parse_time).transpose()?;
                        self.clock.freeze(at)?;
                    }
                    "unfreeze" => self.clock.unfreeze()?,
                    "set" => {
                        let b: AtBody = parse_body(body)?;
                        let at = b.at.ok_or_else(|| ControlError::BadRequest("\"at\" is required".into()))?;
                        self.clock.set(parse_time(&at)?)?;
                    }
                    "advance" => {
                        let b: AdvanceBody = parse_body(body)?;
                        let by =
                            b.by.ok_or_else(|| ControlError::BadRequest("\"by\" is required, e.g. \"30d\"".into()))?;
                        self.clock.advance(clock::parse_duration(&by).map_err(ControlError::BadRequest)?)?;
                    }
                    "reset" => self.clock.reset(),
                    _ => return Ok(error(404, "NotFound", format!("no such endpoint: {path}"))),
                }
                Ok(reply(200, clock_json(&self.clock)))
            }
            ["requests"] => match *method {
                M::GET => Ok(reply(200, self.requests_json(q)?)),
                M::DELETE => {
                    self.clear_requests();
                    Ok(reply(200, json!({ "cleared": true })))
                }
                _ => not_allowed(),
            },
            ["requests", "config"] => match *method {
                M::GET => Ok(reply(200, self.log_config())),
                M::PUT => {
                    let b: LogConfigBody = parse_body(body)?;
                    Ok(reply(200, self.set_log_config(b.enabled, b.capacity)?))
                }
                _ => not_allowed(),
            },
            _ => Ok(error(404, "NotFound", format!("no such endpoint: {path}"))),
        }
    }
}

type Query = Vec<(String, String)>;

/// How the request body is to be read, by `Content-Type`, for each kind of document the control plane accepts.
#[derive(Clone, Copy)]
struct Formats {
    script: ScriptFormat,
    trajectory: TrajectoryFormat,
    cassette: CassetteFormat,
}

fn script_format(headers: &http::HeaderMap) -> ScriptFormat {
    let ct = headers.get(http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let essence = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match essence.as_str() {
        "application/yaml" | "text/yaml" | "application/x-yaml" => ScriptFormat::Yaml,
        _ => ScriptFormat::Auto,
    }
}

/// Content types that mean "one JSON object per line" for trajectory uploads; YAML types mean YAML.
fn trajectory_format(headers: &http::HeaderMap) -> TrajectoryFormat {
    let ct = headers.get(http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let essence = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match essence.as_str() {
        "application/x-ndjson" | "application/jsonl" | "application/json-lines" | "text/jsonl" => TrajectoryFormat::Jsonl,
        "application/yaml" | "text/yaml" | "application/x-yaml" => TrajectoryFormat::Yaml,
        _ => TrajectoryFormat::Auto,
    }
}

/// Same content types as for trajectories.
fn cassette_format(headers: &http::HeaderMap) -> CassetteFormat {
    match trajectory_format(headers) {
        TrajectoryFormat::Jsonl => CassetteFormat::Jsonl,
        TrajectoryFormat::Yaml => CassetteFormat::Yaml,
        _ => CassetteFormat::Auto,
    }
}

fn text_response(status: u16, content_type: &str, body: String) -> Response {
    http::Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, content_type)
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(crate::full_body(bytes::Bytes::from(body)))
        .expect("static response parts are valid")
}

fn reply(status: u16, body: Value) -> Response {
    json_response(status, &body)
}

fn error(status: u16, code: &str, message: impl fmt::Display) -> Response {
    reply(status, json!({ "error": { "code": code, "message": message.to_string() } }))
}

impl From<ControlError> for Response {
    fn from(e: ControlError) -> Self {
        match e {
            ControlError::BadRequest(m) => error(400, "BadRequest", m),
            ControlError::NotFound(m) => error(404, "NotFound", m),
            ControlError::Conflict(m) => error(409, "Conflict", m),
        }
    }
}

fn query_flag(q: &[(String, String)], name: &str, default: bool) -> Result<bool, ControlError> {
    match q.iter().find(|(k, _)| k == name) {
        None => Ok(default),
        Some((_, v)) => parse_flag(name, v),
    }
}

fn parse_flag(name: &str, v: &str) -> Result<bool, ControlError> {
    match v {
        "" | "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(ControlError::BadRequest(format!("{name} must be true or false"))),
    }
}

fn parse_body<T: for<'de> Deserialize<'de> + Default>(body: &[u8]) -> Result<T, ControlError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|e| ControlError::BadRequest(format!("invalid JSON body: {e}")))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AtBody {
    at: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdvanceBody {
    by: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotBody {
    name: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogConfigBody {
    enabled: Option<bool>,
    capacity: Option<usize>,
}

fn request_filter(q: &[(String, String)]) -> Result<RequestFilter, ControlError> {
    let mut f = RequestFilter::default();
    for (k, v) in q {
        let bad = |what: &str| ControlError::BadRequest(format!("invalid {k} {v:?}: {what}"));
        match k.as_str() {
            "model" => f.model = Some(v.clone()),
            "has_tool" => f.has_tool = Some(v.clone()),
            "stream" => f.stream = Some(parse_flag(k, v)?),
            "status" => f.status = Some(v.parse::<StatusFilter>().map_err(ControlError::BadRequest)?),
            "source" => {
                if !matches!(v.as_str(), "script" | "cassette" | "upstream" | "fallback" | "strict" | "rejected") {
                    return Err(bad("expected script, cassette, upstream, fallback, strict or rejected"));
                }
                f.source = Some(v.clone());
            }
            "rule" => f.rule = Some(v.clone()),
            "protocol" => {
                f.protocol = Some(match v.as_str() {
                    "openai.chat" => Protocol::OpenaiChat,
                    "openai.responses" => Protocol::OpenaiResponses,
                    "anthropic.messages" => Protocol::AnthropicMessages,
                    "openai.embeddings" => Protocol::OpenaiEmbeddings,
                    _ => {
                        return Err(bad(
                            "expected openai.chat, openai.responses, openai.embeddings or anthropic.messages",
                        ));
                    }
                })
            }
            "path" => f.path_prefix = Some(v.clone()),
            "since" => f.since = Some(parse_time(v)?),
            "after" => f.after = Some(v.parse().map_err(|_| bad("expected a sequence number"))?),
            "limit" => f.limit = Some(v.parse().map_err(|_| bad("expected a number"))?),
            "full" => {}
            _ => return Err(ControlError::BadRequest(format!("unknown query parameter {k:?}"))),
        }
    }
    Ok(f)
}

const TIME_FORMAT: &[time::format_description::BorrowedFormatItem<'static>] =
    time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// RFC 3339 with millisecond precision, UTC: `2030-01-02T03:04:05.000Z`.
pub(crate) fn fmt_time(t: SystemTime) -> String {
    // Formatting a representable time cannot fail; fall back to the epoch rather than panic.
    time::OffsetDateTime::from(t)
        .format(TIME_FORMAT)
        .unwrap_or_else(|_| "1970-01-01T00:00:00.000Z".into())
}

fn fmt_unix(secs: i64) -> String {
    fmt_time(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs.max(0) as u64))
}

/// Parses an RFC 3339 time.
pub(crate) fn parse_time(s: &str) -> Result<SystemTime, ControlError> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map(SystemTime::from)
        .map_err(|_| ControlError::BadRequest(format!("invalid time {s:?}, expected RFC 3339 such as 2030-01-02T03:04:05Z")))
}

fn clock_json(c: &Clock) -> Value {
    json!({ "now": fmt_time(c.now()), "frozen": c.is_frozen(), "shifted": c.is_virtual() })
}

fn log_config_json(l: &RequestLog) -> Value {
    json!({ "enabled": l.is_enabled(), "capacity": l.capacity(), "stored": l.len(), "dropped": l.dropped() })
}

/// The JSON view of one log record, as returned by `GET /_fakellm/requests`.
pub(crate) fn record_json(r: &RequestRecord, full: bool) -> Value {
    let source = r.outcome.as_deref().map(Outcome::source);
    let rule = source.and_then(Source::rule).and_then(|(_, name)| name.map(str::to_owned));
    let reply = match r.outcome.as_deref().map(Outcome::inner) {
        Some(Outcome::Reply { reply, .. }) => json!({
            "finish": reply.finish.as_str(),
            "text": reply.text,
            "tool_calls": reply.tool_calls,
            "reasoning": reply.reasoning,
            "usage": {
                "prompt_tokens": reply.usage.prompt_tokens,
                "completion_tokens": reply.usage.completion_tokens,
                "total_tokens": reply.usage.total_tokens(),
                "reasoning_tokens": reply.usage.reasoning_tokens,
                "cached_tokens": reply.usage.cached_tokens,
            },
        }),
        // The wire shape, so a test can assert the same thing the client saw.
        Some(Outcome::Error { error, .. }) => {
            let body = match r.protocol() {
                Some(Protocol::AnthropicMessages) => crate::anthropic::error::body(error),
                _ => crate::openai::error::body(error),
            };
            json!({
                "status": error.status,
                "error": body["error"],
                "retry_after": error.retry_after.map(|d| d.as_secs_f64()),
                "in_stream": error.in_stream,
                "headers": error.headers.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
            })
        }
        Some(Outcome::Hang { .. }) => unreachable!("inner() never returns a Hang"),
        None => Value::Null,
    };
    let mut reply = reply;
    if let Some(Outcome::Hang { until, .. }) = r.outcome.as_deref()
        && let Some(obj) = reply.as_object_mut()
    {
        obj.insert("hang".into(), json!({ "until_ms": until.map(|d| d.as_millis() as u64) }));
    }
    let mut out = json!({
        "seq": r.seq,
        "time": fmt_time(r.time),
        "method": r.method.as_str(),
        "path": r.path,
        "status": r.status,
        "hung": r.hung,
        "latency_us": r.latency.as_micros() as u64,
        "protocol": r.protocol().map(|p| p.as_str()),
        "model": r.model(),
        "stream": r.stream(),
        "source": source.map_or(Value::Null, |s| serde_json::to_value(s).unwrap_or(Value::Null)),
        "rule": rule,
        "request": r.request.as_deref().map_or(Value::Null, |q| serde_json::to_value(q).unwrap_or(Value::Null)),
        "reply": reply,
    });
    if full {
        let obj = out.as_object_mut().expect("object literal");
        obj.insert("headers".into(), r.headers.iter().map(|(k, v)| json!([k, v])).collect());
        match (&r.raw, &r.raw_text) {
            (Some(raw), _) => obj.insert("raw".into(), (**raw).clone()),
            (None, Some(t)) => obj.insert("raw_text".into(), json!(t)),
            (None, None) => None,
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Delivery, FinishReason, Message, Reply, Request, Role};
    use std::time::Duration;

    fn q(pairs: &[(&str, &str)]) -> Query {
        pairs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect()
    }

    fn record(source: Source) -> RequestRecord {
        RequestRecord {
            seq: 7,
            time: SystemTime::UNIX_EPOCH + Duration::from_millis(1500),
            method: http::Method::POST,
            path: "/v1/chat/completions".into(),
            status: 200,
            hung: false,
            latency: Duration::from_micros(42),
            headers: vec![("x-a".into(), "b".into())],
            raw: Some(Arc::new(json!({"model": "m"}))),
            raw_text: None,
            request: Some(Arc::new(Request::new("m", vec![Message::text(Role::User, "hi")]))),
            outcome: Some(Arc::new(Outcome::Reply {
                reply: Reply {
                    text: Some("yo".into()),
                    finish: FinishReason::Stop,
                    ..Reply::default()
                },
                delivery: Delivery::default(),
                source,
            })),
        }
    }

    #[test]
    fn time_round_trip() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_893_553_445_123);
        let s = fmt_time(t);
        assert_eq!(s, "2030-01-02T03:04:05.123Z");
        assert_eq!(parse_time(&s).unwrap(), t);
        assert_eq!(
            parse_time("2030-01-02T04:04:05+01:00").unwrap(),
            parse_time("2030-01-02T03:04:05Z").unwrap()
        );
        assert!(parse_time("yesterday").is_err());
        assert_eq!(fmt_time(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn record_json_shape() {
        let v = record_json(
            &record(Source::Script {
                index: 2,
                name: Some("greet".into()),
            }),
            false,
        );
        assert_eq!(v["seq"], 7);
        assert_eq!(v["time"], "1970-01-01T00:00:01.500Z");
        assert_eq!(v["latency_us"], 42);
        assert_eq!(v["protocol"], "openai.chat");
        assert_eq!(v["model"], "m");
        assert_eq!(v["stream"], false);
        assert_eq!(v["source"], json!({"kind": "script", "index": 2, "name": "greet"}));
        assert_eq!(v["rule"], "greet");
        assert_eq!(v["reply"]["text"], "yo");
        assert_eq!(v["reply"]["finish"], "stop");
        assert_eq!(v["reply"]["usage"]["total_tokens"], 0);
        assert_eq!(v["request"]["model"], "m");
        assert!(v.get("headers").is_none());

        let v = record_json(&record(Source::Fallback), true);
        assert_eq!(v["rule"], Value::Null);
        assert_eq!(v["headers"], json!([["x-a", "b"]]));
        assert_eq!(v["raw"], json!({"model": "m"}));
    }

    #[test]
    fn record_json_error_and_unparsed() {
        let mut r = record(Source::Rejected);
        r.request = None;
        r.raw = None;
        r.raw_text = Some("not json".into());
        r.outcome = Some(Arc::new(Outcome::rejected(crate::ir::ApiError::invalid_api_key())));
        let v = record_json(&r, true);
        assert_eq!(v["protocol"], Value::Null);
        assert_eq!(v["request"], Value::Null);
        assert_eq!(v["reply"]["status"], 401);
        assert_eq!(v["reply"]["error"]["code"], "invalid_api_key");
        assert_eq!(v["reply"]["error"]["type"], "invalid_request_error");
        assert_eq!(v["raw_text"], "not json");
        assert_eq!(v["source"]["kind"], "rejected");
    }

    #[test]
    fn filter_parsing() {
        let f = request_filter(&q(&[
            ("model", "gpt-*"),
            ("has_tool", "t"),
            ("stream", "true"),
            ("status", "4xx"),
            ("source", "script"),
            ("rule", "r"),
            ("path", "/v1"),
            ("since", "2030-01-02T03:04:05Z"),
            ("after", "3"),
            ("limit", "5"),
            ("full", "true"),
        ]))
        .unwrap();
        assert_eq!(f.model.as_deref(), Some("gpt-*"));
        assert_eq!(f.stream, Some(true));
        assert_eq!(f.status, Some(StatusFilter::Class(4)));
        assert_eq!((f.after, f.limit), (Some(3), Some(5)));
        assert!(f.since.is_some());
        assert!(request_filter(&q(&[("bogus", "1")])).is_err());
        assert!(request_filter(&q(&[("source", "other")])).is_err());
        assert!(request_filter(&q(&[("stream", "maybe")])).is_err());
        assert!(request_filter(&q(&[("limit", "x")])).is_err());
    }

    #[test]
    fn script_format_by_content_type() {
        let h = |ct: &str| {
            let mut m = http::HeaderMap::new();
            m.insert(http::header::CONTENT_TYPE, ct.parse().unwrap());
            m
        };
        assert_eq!(script_format(&h("application/yaml")), ScriptFormat::Yaml);
        assert_eq!(script_format(&h("text/yaml; charset=utf-8")), ScriptFormat::Yaml);
        assert_eq!(script_format(&h("application/x-yaml")), ScriptFormat::Yaml);
        assert_eq!(script_format(&h("application/json")), ScriptFormat::Auto);
        assert_eq!(script_format(&http::HeaderMap::new()), ScriptFormat::Auto);
    }

    #[test]
    fn flags_and_bodies() {
        assert!(query_flag(&q(&[("a", "")]), "a", false).unwrap());
        assert!(!query_flag(&q(&[("a", "0")]), "a", true).unwrap());
        assert!(query_flag(&q(&[]), "a", true).unwrap());
        assert!(query_flag(&q(&[("a", "x")]), "a", true).is_err());
        let b: AtBody = parse_body(b"  ").unwrap();
        assert!(b.at.is_none());
        assert!(parse_body::<AtBody>(b"{\"nope\":1}").is_err());
    }
}
