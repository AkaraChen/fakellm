//! Policy scripts: a JS module decides the reply from the request, for agent behavior that does not
//! fit a fixed trajectory ("read the file first, then edit it; on a tool error retry once, then give
//! up"). The module runs in a long-lived subprocess per policy (txiki.js, Node or Bun) that speaks
//! one JSON line per decision over stdio, see [`protocol`]; fakellm keeps the policy's `state`, so a
//! crashed process is replaced without losing it.
//!
//! A script rule hands a request to a policy with `reply: {policy: "name"}`; the policy's reply is
//! a `ReplySpec` that the engine renders like the rule's own. See docs/m3-spec.md ("Policy scripts")
//! and docs/agent.md. Registering a policy runs code, and `runtime` may be any executable: see the
//! security notes in docs/control-plane.md.

pub mod protocol;
pub mod runtime;

pub use runtime::Runtime;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::ir::{ApiError, Request};
use crate::script::{self, ReplySpec, ScriptFormat};

/// Default `Config::policy_timeout_ms`.
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;

/// Lines of subprocess stderr (`context.log`, `console.error`) kept per policy.
pub const LOG_LINES: usize = 200;

/// A policy as registered: `{name, source, runtime?}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySpec {
    /// Unique among registered policies; letters, digits, `.`, `_` and `-`.
    pub name: String,
    /// The ES module source; its default export is `decide(request, context)`.
    pub source: String,
    /// Overrides `Config::policy_runtime` for this policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<Runtime>,
}

impl PolicySpec {
    /// Parses a registration body: one `{name, source, runtime?}` object, an array of them, or an
    /// object with a `policies` array; JSON or YAML (`Auto` looks at the first character).
    pub fn parse_many(text: &str, format: ScriptFormat) -> Result<Vec<PolicySpec>, PolicyError> {
        let json = match format {
            ScriptFormat::Json => true,
            ScriptFormat::Yaml => false,
            ScriptFormat::Auto => text.trim_start().starts_with(['[', '{']),
        };
        let doc: Value = if json {
            serde_json::from_str(text).map_err(|e| PolicyError(format!("invalid JSON: {e}")))?
        } else {
            serde_norway::from_str(text).map_err(|e| PolicyError(format!("invalid YAML: {e}")))?
        };
        let items = match doc {
            Value::Array(a) => a,
            Value::Object(mut o) if o.contains_key("policies") => match o.remove("policies") {
                Some(Value::Array(a)) => a,
                _ => return Err(PolicyError("`policies` must be an array".into())),
            },
            obj @ Value::Object(_) => vec![obj],
            Value::Null => Vec::new(),
            _ => {
                return Err(PolicyError(
                    "expected a policy object, an array of them or {\"policies\": [...]}".into(),
                ));
            }
        };
        items
            .into_iter()
            .enumerate()
            .map(|(i, v)| serde_json::from_value::<PolicySpec>(v).map_err(|e| PolicyError(format!("policy {i}: {e}"))))
            .collect()
    }

    /// Reads a policy module from `path` for `--policy FILE`: the name is the file stem, the runtime the default.
    pub fn from_path(path: &std::path::Path) -> std::io::Result<PolicySpec> {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{}: no file name", path.display())))?
            .to_owned();
        Ok(PolicySpec {
            name,
            source: std::fs::read_to_string(path)?,
            runtime: None,
        })
    }
}

/// A policy could not be registered: bad name, no runtime, module failed to load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyError(pub String);

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PolicyError {}

/// What the registry says about one policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyInfo {
    pub name: String,
    /// The runtime in use after defaults were applied: `tjs`, `node`, `bun` or the path.
    pub runtime: String,
    /// A subprocess is alive right now (they start lazily and after crashes).
    pub running: bool,
    /// Decisions made so far; what `context.hits` showed the latest call, plus one.
    pub hits: u64,
    /// Subprocess starts after the first.
    pub restarts: u64,
    pub state: Value,
    /// The last `LOG_LINES` stderr lines.
    pub log: Vec<String>,
}

/// A policy and what it has learned, as a snapshot keeps it.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyState {
    pub spec: PolicySpec,
    pub state: Value,
    pub hits: u64,
    pub restarts: u64,
}

/// The registered policies and their subprocesses.
pub struct Policies {
    default_runtime: Option<Runtime>,
    timeout: Duration,
    inner: Mutex<Vec<Arc<Entry>>>,
}

struct Entry {
    spec: PolicySpec,
    runtime: Runtime,
    /// What `list` and `get` show; written only by whoever holds `live`, so reads never wait on a decision.
    snap: Mutex<Snapshot>,
    /// One decision at a time per policy: state is read-modify-write, and the subprocess is sequential.
    live: tokio::sync::Mutex<Live>,
    /// Shared with the task that forwards the subprocess's stderr; outlives restarts.
    log: Arc<Mutex<VecDeque<String>>>,
    /// Set when the entry leaves the registry, so an in-flight decision kills its subprocess at once.
    closed: tokio::sync::watch::Sender<bool>,
}

/// Marks the policy as not running unless the decision finished; covers a dropped (cancelled) request future.
struct RunGuard<'a> {
    entry: &'a Entry,
    armed: bool,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.entry.snap.lock().running = false;
        }
    }
}

struct Snapshot {
    state: Value,
    hits: u64,
    restarts: u64,
    running: bool,
}

struct Live {
    process: Option<runtime::Process>,
    next_id: u64,
}

/// Started but not yet registered policies, see [`Policies::prepare`].
pub(crate) struct Prepared(Vec<Arc<Entry>>);

impl Default for Policies {
    fn default() -> Self {
        Self::new(None, Duration::from_millis(DEFAULT_TIMEOUT_MS))
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Entry {
    fn logger(&self) -> impl Fn(String) + Send + 'static + use<> {
        let log = self.log.clone();
        let name = self.spec.name.clone();
        move |line| {
            tracing::info!(target: "fakellm::policy", policy = name, "{line}");
            let mut log = log.lock();
            if log.len() == LOG_LINES {
                log.pop_front();
            }
            log.push_back(line);
        }
    }

    fn info(&self) -> PolicyInfo {
        if let Ok(mut live) = self.live.try_lock()
            && let Some(p) = live.process.as_mut()
        {
            self.snap.lock().running = !p.exited();
        }
        let snap = self.snap.lock();
        PolicyInfo {
            name: self.spec.name.clone(),
            runtime: self.runtime.to_string(),
            running: snap.running,
            hits: snap.hits,
            restarts: snap.restarts,
            state: snap.state.clone(),
            log: self.log.lock().iter().cloned().collect(),
        }
    }
}

impl Policies {
    /// `default_runtime` of `None` means "tjs if on PATH, else node, else bun", looked up at registration.
    pub fn new(default_runtime: Option<Runtime>, timeout: Duration) -> Self {
        Self {
            default_runtime,
            timeout,
            inner: Mutex::new(Vec::new()),
        }
    }

    /// Validates `specs` and starts one dry-run subprocess each, without touching the registry: the slow, fallible half
    /// of [`Policies::replace`]. Hot reload and snapshot restore prepare first, then [`Policies::commit`] together with
    /// the other registries, so a failure changes nothing and the swap itself never awaits.
    pub(crate) async fn prepare(&self, specs: Vec<PolicySpec>) -> Result<Prepared, PolicyError> {
        self.start(specs).await.map(Prepared)
    }

    /// Swaps in prepared policies under one lock. The replaced ones are closed: a decision in flight on one of them
    /// ends as `fakellm_policy_crashed` (500) and the request is not retried.
    pub(crate) fn commit(&self, prepared: Prepared) {
        let old = std::mem::replace(&mut *self.inner.lock(), prepared.0);
        close(&old);
    }

    async fn start(&self, specs: Vec<PolicySpec>) -> Result<Vec<Arc<Entry>>, PolicyError> {
        let mut seen = std::collections::HashSet::new();
        for spec in &specs {
            if !valid_name(&spec.name) {
                return Err(PolicyError(format!(
                    "policy name {:?} must be non-empty and use letters, digits, `.`, `_` or `-`",
                    spec.name
                )));
            }
            if !seen.insert(spec.name.as_str()) {
                return Err(PolicyError(format!("policy {:?} is registered twice", spec.name)));
            }
        }
        let mut entries = Vec::with_capacity(specs.len());
        for spec in specs {
            let runtime = spec
                .runtime
                .clone()
                .or_else(|| self.default_runtime.clone())
                .or_else(runtime::detect_runtime)
                .ok_or_else(|| PolicyError("no JavaScript runtime found: install node, bun or txiki.js (tjs), or pass runtime".into()))?;
            let mut entry = Entry {
                spec,
                runtime,
                snap: Mutex::new(Snapshot {
                    state: Value::Object(Default::default()),
                    hits: 0,
                    restarts: 0,
                    running: true,
                }),
                live: tokio::sync::Mutex::new(Live { process: None, next_id: 1 }),
                log: Arc::default(),
                closed: tokio::sync::watch::channel(false).0,
            };
            let process = runtime::spawn(&entry.runtime, &entry.spec.source, self.timeout, entry.logger())
                .await
                .map_err(|e| PolicyError(format!("policy \"{}\": {e}", entry.spec.name)))?;
            entry.live.get_mut().process = Some(process);
            entries.push(Arc::new(entry));
        }
        Ok(entries)
    }

    /// Registers `specs` after a dry run each (start the subprocess, import the module, call nothing),
    /// replacing every registered policy. Fails without changing anything when a name is invalid or
    /// duplicated, no runtime is available, or a module does not load (the message carries the
    /// module's own error). The dry-run subprocess is kept as the live one.
    pub async fn replace(&self, specs: Vec<PolicySpec>) -> Result<(), PolicyError> {
        let prepared = self.prepare(specs).await?;
        self.commit(prepared);
        Ok(())
    }

    /// Like [`Policies::replace`] but adds, replacing a registered policy of the same name (its state restarts).
    pub async fn upsert(&self, specs: Vec<PolicySpec>) -> Result<(), PolicyError> {
        let entries = self.start(specs).await?;
        let mut inner = self.inner.lock();
        for entry in entries {
            match inner.iter().position(|e| e.spec.name == entry.spec.name) {
                Some(i) => {
                    inner[i].closed.send_replace(true);
                    inner[i] = entry;
                }
                None => inner.push(entry),
            }
        }
        Ok(())
    }

    /// For snapshots: every policy's spec, `state`, `hits` and `restarts`.
    pub fn export_state(&self) -> Vec<PolicyState> {
        let entries = self.inner.lock().clone();
        entries
            .iter()
            .map(|e| {
                let snap = e.snap.lock();
                PolicyState {
                    spec: e.spec.clone(),
                    state: snap.state.clone(),
                    hits: snap.hits,
                    restarts: snap.restarts,
                }
            })
            .collect()
    }

    /// For snapshots: replaces all policies like [`Policies::replace`] (fresh subprocesses, dry run) and then sets each
    /// one's `state`, `hits` and `restarts` from `state`. Fails like `replace`, without changing anything.
    pub async fn import_state(&self, state: Vec<PolicyState>) -> Result<(), PolicyError> {
        let specs = state.iter().map(|s| s.spec.clone()).collect();
        let prepared = self.prepare(specs).await?;
        for (entry, saved) in prepared.0.iter().zip(&state) {
            let mut snap = entry.snap.lock();
            snap.state = saved.state.clone();
            snap.hits = saved.hits;
            snap.restarts = saved.restarts;
        }
        self.commit(prepared);
        Ok(())
    }

    /// Removes one policy and kills its subprocess (also one that is mid-decision, which fails as crashed); `false` when there was none by that name.
    pub fn remove(&self, name: &str) -> bool {
        let mut inner = self.inner.lock();
        let Some(i) = inner.iter().position(|e| e.spec.name == name) else {
            return false;
        };
        inner.remove(i).closed.send_replace(true);
        true
    }

    /// Removes every policy; dropping an entry kills its subprocess and deletes its temp dir.
    /// It does not wait for the children to exit.
    pub fn clear(&self) {
        let old = std::mem::take(&mut *self.inner.lock());
        close(&old);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn find(&self, name: &str) -> Option<Arc<Entry>> {
        self.inner.lock().iter().find(|e| e.spec.name == name).cloned()
    }

    pub fn get(&self, name: &str) -> Option<PolicyInfo> {
        self.find(name).map(|e| e.info())
    }

    pub fn list(&self) -> Vec<PolicyInfo> {
        let entries = self.inner.lock().clone();
        entries.iter().map(|e| e.info()).collect()
    }

    /// Asks policy `name` to decide the reply for `req`:
    ///
    /// 1. Unknown name: `ApiError::policy_not_found`.
    /// 2. Takes the policy's lock (decisions are serialized), starts the subprocess if none is
    ///    alive (counting a restart), writes `{id, request, state, hits}`.
    /// 3. Waits for the matching `{id, ...}` line for at most the timeout on the host clock; on timeout
    ///    kills the subprocess and returns `ApiError::policy_timeout`. EOF or a broken pipe is
    ///    `ApiError::policy_crashed`; an `error` line, a reply that is not a `ReplySpec` or one that fails
    ///    validation (`trajectory` is allowed, `policy` is not) is `ApiError::policy_error`.
    /// 4. On success stores the reply's `state` (when present) and counts the hit; failures do not count.
    pub async fn decide(&self, name: &str, req: &Request) -> Result<ReplySpec, ApiError> {
        let entry = self.find(name).ok_or_else(|| ApiError::policy_not_found(name))?;
        let mut live = entry.live.lock().await;
        if live.process.as_mut().is_some_and(|p| p.exited()) {
            live.process = None;
        }
        if live.process.is_none() {
            let process = runtime::spawn(&entry.runtime, &entry.spec.source, self.timeout, entry.logger())
                .await
                .map_err(|e| ApiError::policy_error(name, &e.0))?;
            live.process = Some(process);
            let mut snap = entry.snap.lock();
            snap.restarts += 1;
            snap.running = true;
        }
        let id = live.next_id;
        live.next_id += 1;
        let (state, hits) = {
            let snap = entry.snap.lock();
            (snap.state.clone(), snap.hits)
        };
        let line = protocol::encode(&protocol::DecideLine {
            id,
            request: req,
            state: &state,
            hits,
        });
        // The process leaves `live` for the exchange: if this future is dropped half-way (client disconnect)
        // it is killed with it, so a half-written request line never reaches the next decision.
        let mut process = live.process.take().expect("started above");
        let mut guard = RunGuard {
            entry: &entry,
            armed: true,
        };
        let mut closed = entry.closed.subscribe();
        let answer = tokio::select! {
            a = tokio::time::timeout(self.timeout, exchange(&mut process, id, &line)) => a,
            _ = closed.wait_for(|c| *c) => Ok(None),
        };
        let (outcome, next_state) = match answer {
            Ok(Some(answer)) => answer,
            failed => {
                return Err(match failed {
                    Ok(_) => ApiError::policy_crashed(name),
                    Err(_) => ApiError::policy_timeout(name, self.timeout.as_millis() as u64),
                });
            }
        };
        guard.armed = false;
        live.process = Some(process);
        let reply = outcome.map_err(|m| ApiError::policy_error(name, &m))?;
        if reply.policy.is_some() {
            return Err(ApiError::policy_error(name, "a policy may not reply with `policy`"));
        }
        script::validate_reply(&reply).map_err(|m| ApiError::policy_error(name, &m))?;
        let mut snap = entry.snap.lock();
        if let Some(next) = next_state {
            snap.state = next;
        }
        snap.hits += 1;
        Ok(reply)
    }
}

fn close(entries: &[Arc<Entry>]) {
    for e in entries {
        e.closed.send_replace(true);
    }
}

type Answer = (Result<ReplySpec, String>, Option<Value>);

/// Writes `line` and reads until the answer with `id`; `None` when the process is gone.
async fn exchange(process: &mut runtime::Process, id: u64, line: &str) -> Option<Answer> {
    process.stdin.write_all(line.as_bytes()).await.ok()?;
    process.stdin.flush().await.ok()?;
    let mut buf = String::new();
    loop {
        buf.clear();
        if runtime::read_line_bounded(&mut process.stdout, &mut buf).await.ok()? == 0 {
            return None;
        }
        match protocol::parse(buf.trim()) {
            Ok(protocol::Incoming::Decided { id: got, outcome, state }) if got == id => return Some((outcome, state)),
            Ok(protocol::Incoming::Decided { .. }) => {}
            Err(e) if buf.trim_start().starts_with("{\"id\"") => return Some((Err(format!("unreadable response: {e}")), None)),
            _ => tracing::warn!(target: "fakellm::policy", "ignoring unexpected stdout line: {}", buf.trim_end()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registration_bodies() {
        let one = r#"{"name":"p","source":"export default () => ({text:'x'})","runtime":"node"}"#;
        let specs = PolicySpec::parse_many(one, ScriptFormat::Auto).unwrap();
        assert_eq!((specs[0].name.as_str(), specs[0].runtime.clone()), ("p", Some(Runtime::Node)));
        let wrapped = r#"{"policies":[{"name":"a","source":"s"},{"name":"b","source":"s","runtime":"/opt/tjs"}]}"#;
        let specs = PolicySpec::parse_many(wrapped, ScriptFormat::Json).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[1].runtime, Some(Runtime::Path("/opt/tjs".into())));
        let yaml = "- name: y\n  source: |\n    export default () => ({ text: 'hi' });\n";
        assert!(
            PolicySpec::parse_many(yaml, ScriptFormat::Auto).unwrap()[0]
                .source
                .contains("export default")
        );
        assert!(PolicySpec::parse_many(r#"{"name":"p"}"#, ScriptFormat::Auto).is_err());
        assert!(PolicySpec::parse_many(r#"[{"name":"p","source":"s","bogus":1}]"#, ScriptFormat::Auto).is_err());
    }

    #[test]
    fn runtimes_pick_their_shims() {
        assert_eq!("tjs".parse::<Runtime>().unwrap(), Runtime::Tjs);
        assert_eq!(Runtime::Path("/usr/local/bin/bun".into()).shim().1, "shim.mjs");
        assert_eq!(Runtime::Path("/opt/tjs".into()).shim().1, "shim.js");
        assert!(Runtime::Node.args().is_empty());
        assert_eq!(Runtime::Bun.args(), vec![std::ffi::OsString::from("run")]);
        assert!(runtime::SHIM_BUN.contains("FAKELLM_POLICY_MODULE") && runtime::SHIM_TJS.contains("UNTESTED"));
    }

    fn node() -> bool {
        let ok = runtime::detect_runtime().is_some() && std::process::Command::new("node").arg("--version").output().is_ok();
        if !ok {
            eprintln!("skipping: node is not installed");
        }
        ok
    }

    fn request(tools: &[&str]) -> Request {
        let tools: Vec<_> = tools.iter().map(|n| serde_json::json!({"name": n})).collect();
        serde_json::from_value(serde_json::json!({
            "protocol": "openai.chat", "model": "m", "messages": [], "stream": false, "tools": tools
        }))
        .unwrap()
    }

    fn spec(name: &str, source: &str, runtime: Runtime) -> PolicySpec {
        PolicySpec {
            name: name.into(),
            source: source.into(),
            runtime: Some(runtime),
        }
    }

    fn runtimes() -> Vec<Runtime> {
        let mut found = vec![Runtime::Node];
        if std::process::Command::new("bun").arg("--version").output().is_ok() {
            found.push(Runtime::Bun);
        }
        found
    }

    const STATEFUL: &str = r#"
        export default function decide(request, { state, hits, log }) {
          log("hit", hits);
          if (hits === 0) {
            const tool = request.tools.find((t) => t.name === "read_file");
            return { tool_calls: [{ name: tool.name, arguments: { path: "a" } }], state: { stage: "read" } };
          }
          return { text: "done after " + state.stage };
        }"#;

    #[tokio::test]
    async fn policy_returns_text_then_tool_call_with_state() {
        if !node() {
            return;
        }
        for rt in runtimes() {
            let policies = Policies::default();
            policies.replace(vec![spec("p", STATEFUL, rt.clone())]).await.unwrap();
            let first = policies.decide("p", &request(&["read_file"])).await.unwrap();
            assert_eq!(first.tool_calls.unwrap()[0].name, "read_file", "{rt}");
            let second = policies.decide("p", &request(&[])).await.unwrap();
            assert_eq!(second.text.as_deref(), Some("done after read"), "{rt}");
            let info = policies.get("p").unwrap();
            assert_eq!((info.hits, info.restarts, info.state), (2, 0, serde_json::json!({"stage": "read"})));
            let mut log = info.log;
            for _ in 0..50 {
                if log.len() == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                log = policies.get("p").unwrap().log;
            }
            assert_eq!(log, ["hit 0", "hit 1"]);
            policies.clear();
        }
    }

    #[tokio::test]
    async fn policy_errors_map_to_codes() {
        if !node() {
            return;
        }
        let src = r#"export default (r) => {
            const t = r.tools[0]?.name;
            if (t === "throw") throw new Error("nope");
            if (t === "empty") return {};
            if (t === "nested") return { policy: "q" };
            if (t === "bad") return { bogus: 1 };
            return { text: "ok" };
        }"#;
        let policies = Policies::default();
        policies.replace(vec![spec("p", src, Runtime::Node)]).await.unwrap();
        for tool in ["throw", "empty", "nested", "bad"] {
            let err = policies.decide("p", &request(&[tool])).await.unwrap_err();
            assert_eq!((err.status, err.code.as_deref()), (400, Some("fakellm_policy_error")), "{tool}");
        }
        assert!(
            policies
                .decide("p", &request(&["throw"]))
                .await
                .unwrap_err()
                .message
                .contains("nope")
        );
        assert_eq!(policies.decide("p", &request(&[])).await.unwrap().text.as_deref(), Some("ok"));
        assert_eq!(policies.get("p").unwrap().hits, 1);
        let err = policies.decide("zzz", &request(&[])).await.unwrap_err();
        assert_eq!(err.code.as_deref(), Some("fakellm_policy_not_found"));
    }

    #[tokio::test]
    async fn timeout_kills_and_the_next_decision_restarts() {
        if !node() {
            return;
        }
        let src = r#"export default async (r) => {
            if (r.tools.length) await new Promise((res) => setTimeout(res, 60000));
            return { text: "fast" };
        }"#;
        let policies = Policies::new(Some(Runtime::Node), Duration::from_millis(400));
        policies.replace(vec![spec("p", src, Runtime::Node)]).await.unwrap();
        let err = policies.decide("p", &request(&["slow"])).await.unwrap_err();
        assert_eq!((err.status, err.code.as_deref()), (500, Some("fakellm_policy_timeout")));
        let info = policies.get("p").unwrap();
        assert_eq!((info.running, info.hits), (false, 0));
        assert_eq!(policies.decide("p", &request(&[])).await.unwrap().text.as_deref(), Some("fast"));
        assert_eq!(policies.get("p").unwrap().restarts, 1);
    }

    #[tokio::test]
    async fn crash_restarts_and_keeps_state() {
        if !node() {
            return;
        }
        let src = r#"export default (r, { hits }) => {
            if (r.tools.length) process.exit(1);
            return { text: "hit " + hits, state: { kept: true } };
        }"#;
        for rt in runtimes() {
            let policies = Policies::default();
            policies.replace(vec![spec("p", src, rt)]).await.unwrap();
            policies.decide("p", &request(&[])).await.unwrap();
            let err = policies.decide("p", &request(&["die"])).await.unwrap_err();
            assert_eq!((err.status, err.code.as_deref()), (500, Some("fakellm_policy_crashed")));
            let again = policies.decide("p", &request(&[])).await.unwrap();
            assert_eq!(again.text.as_deref(), Some("hit 1"));
            let info = policies.get("p").unwrap();
            assert_eq!((info.restarts, info.hits, info.state), (1, 2, serde_json::json!({"kept": true})));
        }
    }

    #[tokio::test]
    async fn registration_checks_names_and_modules() {
        if !node() {
            return;
        }
        let policies = Policies::default();
        policies
            .replace(vec![spec("keep", "export default () => ({ text: 'k' })", Runtime::Node)])
            .await
            .unwrap();
        let bad = |name: &str, src: &str| vec![spec(name, src, Runtime::Node)];
        let e = policies.replace(bad("p", "export default (")).await.unwrap_err();
        assert!(e.0.starts_with("policy \"p\": "), "{e}");
        let e = policies.replace(bad("p", "export const x = 1;")).await.unwrap_err();
        assert!(e.0.contains("default function"), "{e}");
        let e = policies.replace(bad("p", "throw new Error('at load')")).await.unwrap_err();
        assert!(e.0.contains("at load"), "{e}");
        assert!(policies.replace(bad("bad name", "export default () => 1")).await.is_err());
        let ok = "export default () => ({ text: 'x' })";
        assert!(
            policies
                .replace(vec![spec("a", ok, Runtime::Node), spec("a", ok, Runtime::Node)])
                .await
                .is_err()
        );
        let e = policies
            .replace(vec![spec("p", ok, Runtime::Path("/nonexistent/runtime".into()))])
            .await
            .unwrap_err();
        assert!(e.0.contains("cannot start"), "{e}");
        assert_eq!(policies.list().iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["keep"]);
        policies.upsert(bad("other", ok)).await.unwrap();
        assert_eq!(policies.len(), 2);
        assert!(policies.remove("keep") && !policies.remove("keep"));
    }
}
