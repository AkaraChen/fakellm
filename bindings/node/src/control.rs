//! The control plane's scripts, request log, stored responses and clock.

use chrono::{DateTime, Utc};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde_json::Value;

use crate::{Slot, control_error, nanos, with_server};

pub(crate) fn parse_rules(text: &str, format: fakellm::ScriptFormat) -> Result<Vec<fakellm::Rule>> {
    fakellm::Rule::parse_many(text, format).map_err(control_error)
}

/// Rules as an array of objects or as a JSON / YAML string.
pub(crate) fn rules(input: Value) -> Result<Vec<fakellm::Rule>> {
    match input {
        Value::String(s) => parse_rules(&s, fakellm::ScriptFormat::Auto),
        v => parse_rules(&v.to_string(), fakellm::ScriptFormat::Json),
    }
}

/// What `scripts.put` and `scripts.add` return.
#[napi(object)]
pub struct ScriptsLoaded {
    /// Rules this call loaded.
    pub added: u32,
    /// Rules loaded now, in total.
    pub count: u32,
}

/// The server's rules. Obtained from `server.scripts`.
#[napi(js_name = "ScriptsControl")]
pub struct ScriptsControl {
    server: Slot,
}

impl ScriptsControl {
    pub(crate) fn new(server: Slot) -> Self {
        Self { server }
    }
}

#[napi]
impl ScriptsControl {
    /// Replaces all rules. Takes an array of rule objects, or a JSON / YAML document as a string.
    /// Hit counts restart unless `keepHits` is true. Returns `{ added, count }`: the rules just
    /// loaded and the number now loaded, as `PUT /_fakellm/scripts` does.
    #[napi]
    pub fn put(&self, input: Value, keep_hits: Option<bool>) -> Result<ScriptsLoaded> {
        let rules = rules(input)?;
        let added = rules.len() as u32;
        with_server(&self.server, |s| {
            if keep_hits.unwrap_or(false) {
                s.scripts().replace_keep_hits(rules)
            } else {
                s.scripts().replace(rules)
            }
            .map(|()| ScriptsLoaded {
                added,
                count: s.scripts().len() as u32,
            })
        })?
        .map_err(control_error)
    }

    /// Appends rules, same input as `put`. Returns `{ added, count }`.
    #[napi]
    pub fn add(&self, input: Value) -> Result<ScriptsLoaded> {
        let rules = rules(input)?;
        let added = rules.len() as u32;
        with_server(&self.server, |s| {
            s.scripts().append(rules).map(|()| ScriptsLoaded {
                added,
                count: s.scripts().len() as u32,
            })
        })?
        .map_err(control_error)
    }

    /// The rules with `index`, `hits`, `reached` and `exhausted`, as `GET /_fakellm/scripts` shows them.
    #[napi]
    pub fn list(&self) -> Result<Value> {
        with_server(&self.server, |s| serde_json::to_value(s.scripts().list()).map_err(control_error))?
    }

    /// Removes all rules.
    #[napi]
    pub fn clear(&self) -> Result<()> {
        with_server(&self.server, |s| s.scripts().clear())
    }
}

/// Trajectories as an array of objects, or as a JSON / YAML / JSONL string.
pub(crate) fn trajectories(input: Value) -> Result<Vec<fakellm::Trajectory>> {
    let (text, format) = match input {
        Value::String(s) => (s, fakellm::TrajectoryFormat::Auto),
        v => (v.to_string(), fakellm::TrajectoryFormat::Json),
    };
    fakellm::Trajectory::parse_many(&text, format).map_err(control_error)
}

/// Policies as an array of `{ name, source, runtime? }`, a single one, or a JSON / YAML string.
pub(crate) fn policies(input: Value) -> Result<Vec<fakellm::PolicySpec>> {
    let (text, format) = match input {
        Value::String(s) => (s, fakellm::ScriptFormat::Auto),
        v => (v.to_string(), fakellm::ScriptFormat::Json),
    };
    fakellm::PolicySpec::parse_many(&text, format).map_err(control_error)
}

/// The registered trajectories. Obtained from `server.trajectories`.
#[napi(js_name = "TrajectoriesControl")]
pub struct TrajectoriesControl {
    server: Slot,
}

impl TrajectoriesControl {
    pub(crate) fn new(server: Slot) -> Self {
        Self { server }
    }
}

#[napi]
impl TrajectoriesControl {
    /// Replaces all trajectories (hit counters restart). Takes an array of trajectory objects, or a
    /// JSON / YAML / JSONL document as a string. Returns `{ added, count }`.
    #[napi]
    pub fn put(&self, input: Value) -> Result<ScriptsLoaded> {
        let items = trajectories(input)?;
        let added = items.len() as u32;
        with_server(&self.server, |s| {
            s.trajectories().replace(items).map(|()| ScriptsLoaded {
                added,
                count: s.trajectories().len() as u32,
            })
        })?
        .map_err(control_error)
    }

    /// Adds trajectories, same input as `put`; one with a registered name replaces it in place.
    #[napi]
    pub fn add(&self, input: Value) -> Result<ScriptsLoaded> {
        let items = trajectories(input)?;
        let added = items.len() as u32;
        with_server(&self.server, |s| {
            s.trajectories().upsert(items).map(|()| ScriptsLoaded {
                added,
                count: s.trajectories().len() as u32,
            })
        })?
        .map_err(control_error)
    }

    /// `{ name, steps, on_divergence, cursor, unknown_tool, hits, last_step }` per trajectory.
    #[napi]
    pub fn list(&self) -> Result<Value> {
        with_server(&self.server, |s| serde_json::to_value(s.trajectories().list()).map_err(control_error))?
    }

    /// Removes all trajectories.
    #[napi]
    pub fn clear(&self) -> Result<()> {
        with_server(&self.server, |s| s.trajectories().clear())
    }
}

/// The registered policy scripts. Obtained from `server.policies`.
#[napi(js_name = "PoliciesControl")]
pub struct PoliciesControl {
    server: Slot,
}

impl PoliciesControl {
    pub(crate) fn new(server: Slot) -> Self {
        Self { server }
    }
}

#[napi]
impl PoliciesControl {
    /// Replaces all policies. Each module is loaded first (a subprocess starts and imports it), so
    /// this blocks the calling thread for that long and throws with the module's own error when one
    /// does not load. Returns `{ added, count }`.
    #[napi]
    pub fn put(&self, input: Value) -> Result<ScriptsLoaded> {
        let specs = policies(input)?;
        let added = specs.len() as u32;
        with_server(&self.server, |s| {
            napi::bindgen_prelude::block_on(s.policies().replace(specs)).map(|()| ScriptsLoaded {
                added,
                count: s.policies().len() as u32,
            })
        })?
        .map_err(control_error)
    }

    /// Adds policies, same input as `put`; one with a registered name replaces it (its state restarts).
    #[napi]
    pub fn add(&self, input: Value) -> Result<ScriptsLoaded> {
        let specs = policies(input)?;
        let added = specs.len() as u32;
        with_server(&self.server, |s| {
            napi::bindgen_prelude::block_on(s.policies().upsert(specs)).map(|()| ScriptsLoaded {
                added,
                count: s.policies().len() as u32,
            })
        })?
        .map_err(control_error)
    }

    /// `{ name, runtime, running, hits, restarts, state, log }` per policy.
    #[napi]
    pub fn list(&self) -> Result<Value> {
        with_server(&self.server, |s| serde_json::to_value(s.policies().list()).map_err(control_error))?
    }

    /// Removes all policies and stops their subprocesses.
    #[napi]
    pub fn clear(&self) -> Result<()> {
        with_server(&self.server, |s| s.policies().clear())
    }
}

/// Which logged requests to return. Unset fields match everything.
#[napi(object)]
pub struct RequestFilter {
    /// Glob over the model name.
    pub model: Option<String>,
    /// Requests that offered a tool with this name.
    pub has_tool: Option<String>,
    pub stream: Option<bool>,
    /// An exact status (`429`), a class (`"4xx"`), or `"error"` for 400 and above.
    pub status: Option<Either<u32, String>>,
    /// `script`, `fallback`, `strict` or `rejected`.
    pub source: Option<String>,
    /// Name of the rule that answered.
    pub rule: Option<String>,
    /// `openai.chat`, `openai.responses`, `openai.embeddings` or `anthropic.messages`.
    pub protocol: Option<String>,
    /// Path prefix.
    pub path: Option<String>,
    /// Only requests at or after this server time.
    pub since: Option<DateTime<Utc>>,
    /// Only requests with a `seq` greater than this.
    pub after: Option<f64>,
    /// Keep only the most recent `limit` matches.
    pub limit: Option<u32>,
    /// Include request headers and the raw body.
    pub full: Option<bool>,
}

fn query(f: Option<RequestFilter>) -> Vec<(String, String)> {
    let Some(f) = f else { return Vec::new() };
    let mut q = Vec::new();
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            q.push((k.to_owned(), v));
        }
    };
    put("model", f.model);
    put("has_tool", f.has_tool);
    put("stream", f.stream.map(|b| b.to_string()));
    put(
        "status",
        f.status.map(|s| match s {
            Either::A(n) => n.to_string(),
            Either::B(s) => s,
        }),
    );
    put("source", f.source);
    put("rule", f.rule);
    put("protocol", f.protocol);
    put("path", f.path);
    put("since", f.since.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)));
    put("after", f.after.map(|n| (n as u64).to_string()));
    put("limit", f.limit.map(|n| n.to_string()));
    put("full", f.full.map(|b| b.to_string()));
    q
}

/// The request log. Obtained from `server.requests`.
#[napi(js_name = "RequestsControl")]
pub struct RequestsControl {
    server: Slot,
}

impl RequestsControl {
    pub(crate) fn new(server: Slot) -> Self {
        Self { server }
    }

    fn fetch(&self, filter: Option<RequestFilter>) -> Result<Value> {
        let q = query(filter);
        with_server(&self.server, |s| s.control().requests_json(&q))?.map_err(control_error)
    }
}

/// Settings of the request log. `enabled` and `capacity` are what can be changed.
#[napi(object)]
pub struct LogConfig {
    /// Whether API requests are being recorded.
    pub enabled: bool,
    /// How many requests the log keeps; the oldest are dropped first.
    pub capacity: u32,
    /// How many requests are in the log now.
    pub stored: u32,
    /// Requests that fell off the end because the log was full, since the last clear.
    pub dropped: u32,
}

/// Fields to change in [`RequestsControl::config`]; unset ones stay as they are.
#[napi(object)]
pub struct LogConfigUpdate {
    pub enabled: Option<bool>,
    /// At least 1. A smaller capacity drops the oldest records at once.
    pub capacity: Option<u32>,
}

#[napi]
impl RequestsControl {
    /// Logged requests, oldest first: the `requests` array of `GET /_fakellm/requests`, with
    /// `seq, time, method, path, status, protocol, model, stream, source, rule, request, reply`.
    #[napi]
    pub fn list(&self, filter: Option<RequestFilter>) -> Result<Value> {
        Ok(self.fetch(filter)?["requests"].take())
    }

    /// How many logged requests match the filter (`limit` is ignored).
    #[napi]
    pub fn count(&self, filter: Option<RequestFilter>) -> Result<u32> {
        let mut filter = filter;
        if let Some(f) = filter.as_mut() {
            f.limit = None;
        }
        Ok(self.fetch(filter)?["total"].as_u64().unwrap_or(0) as u32)
    }

    /// Requests that fell off the end of the log because it was full.
    #[napi]
    pub fn dropped(&self) -> Result<u32> {
        Ok(self.config(None)?.dropped)
    }

    /// The log's settings and occupancy; with `update`, changes `enabled` and/or `capacity` first.
    #[napi]
    pub fn config(&self, update: Option<LogConfigUpdate>) -> Result<LogConfig> {
        let v = with_server(&self.server, |s| match update {
            Some(u) => s.control().set_log_config(u.enabled, u.capacity.map(|c| c as usize)),
            None => Ok(s.control().log_config()),
        })?
        .map_err(control_error)?;
        let n = |k: &str| v[k].as_u64().unwrap_or(0) as u32;
        Ok(LogConfig {
            enabled: v["enabled"].as_bool().unwrap_or(false),
            capacity: n("capacity"),
            stored: n("stored"),
            dropped: n("dropped"),
        })
    }

    /// Empties the log.
    #[napi]
    pub fn clear(&self) -> Result<()> {
        with_server(&self.server, |s| s.control().clear_requests())
    }
}

/// Stored Responses API objects. Obtained from `server.responses`.
#[napi(js_name = "ResponsesControl")]
pub struct ResponsesControl {
    server: Slot,
}

impl ResponsesControl {
    pub(crate) fn new(server: Slot) -> Self {
        Self { server }
    }
}

#[napi]
impl ResponsesControl {
    /// `[{ id, created_at }]` for every stored response.
    #[napi]
    pub fn list(&self) -> Result<Value> {
        Ok(with_server(&self.server, |s| s.control().responses_json())?["responses"].take())
    }

    /// Forgets all stored responses; returns how many there were.
    #[napi]
    pub fn clear(&self) -> Result<u32> {
        with_server(&self.server, |s| s.clear_responses() as u32)
    }
}

/// The server's clock. Every timestamp the server produces reads it, and `ttft` / `delay_ms`
/// waits run on it: while it is frozen they only finish when it is advanced. Obtained from `server.clock`.
#[napi(js_name = "ClockControl")]
pub struct ClockControl {
    server: Slot,
}

impl ClockControl {
    pub(crate) fn new(server: Slot) -> Self {
        Self { server }
    }
}

/// What the clock mutators return, the same fields as the HTTP endpoints' `{now,frozen,shifted}`.
#[napi(object)]
pub struct ClockState {
    pub now: DateTime<Utc>,
    pub frozen: bool,
    pub shifted: bool,
}

fn state_of(s: &fakellm::Server) -> ClockState {
    let c = s.clock();
    ClockState {
        now: c.now().into(),
        frozen: c.is_frozen(),
        shifted: c.is_virtual(),
    }
}

fn clock_error(e: fakellm::OutOfRange) -> Error {
    control_error(e)
}

#[napi]
impl ClockControl {
    /// The current server time.
    #[napi]
    pub fn now(&self) -> Result<DateTime<Utc>> {
        with_server(&self.server, |s| s.clock().now().into())
    }

    /// Whether time is stopped.
    #[napi]
    pub fn is_frozen(&self) -> Result<bool> {
        with_server(&self.server, |s| s.clock().is_frozen())
    }

    /// Whether the clock differs from the host clock (frozen, or running with an offset).
    #[napi]
    pub fn is_shifted(&self) -> Result<bool> {
        with_server(&self.server, |s| s.clock().is_virtual())
    }

    /// The clock in one call: `{ now, frozen, shifted }`, as the HTTP clock endpoints return it.
    #[napi]
    pub fn state(&self) -> Result<ClockState> {
        with_server(&self.server, state_of)
    }

    /// Stops time at `at`, or at the current server time when omitted.
    #[napi]
    pub fn freeze(&self, at: Option<DateTime<Utc>>) -> Result<ClockState> {
        with_server(&self.server, |s| s.clock().freeze(at.map(Into::into)).map(|()| state_of(s)))?.map_err(clock_error)
    }

    /// Lets time run again from the current server time.
    #[napi]
    pub fn unfreeze(&self) -> Result<ClockState> {
        with_server(&self.server, |s| s.clock().unfreeze().map(|()| state_of(s)))?.map_err(clock_error)
    }

    /// Jumps to `at`, keeping the frozen or running state.
    #[napi]
    pub fn set(&self, at: DateTime<Utc>) -> Result<ClockState> {
        with_server(&self.server, |s| s.clock().set(at.into()).map(|()| state_of(s)))?.map_err(clock_error)
    }

    /// Moves time forward, or backward when negative. `by` is milliseconds or a string such as
    /// `"30d"`, `"1h30m"`, `"500ms"`.
    #[napi]
    pub fn advance(&self, by: Either<f64, String>) -> Result<ClockState> {
        let n = nanos(by)?;
        with_server(&self.server, |s| s.clock().advance(n).map(|()| state_of(s)))?.map_err(clock_error)
    }

    /// Puts the clock back on the host clock.
    #[napi]
    pub fn reset(&self) -> Result<ClockState> {
        with_server(&self.server, |s| {
            s.clock().reset();
            state_of(s)
        })
    }
}
