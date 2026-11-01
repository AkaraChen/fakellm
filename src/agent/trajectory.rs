//! Trajectories: one agent run as an ordered list of model turns, replayed one step per request.
//!
//! The step to answer with is `Request::assistant_turns()`, the number of assistant messages the
//! client has replayed. That needs no session ids and survives parallel conversations and
//! retried requests (a retry carries the same history, so it gets the same step). Clients that do
//! not replay history opt into `cursor: by_hits`, where a per-trajectory counter picks the step.
//!
//! Before answering, a step's `expect` is checked against the tool results the client sent back
//! (the results of the *previous* step's calls) and the divergence policy decides what a mismatch
//! means: `strict` fails the request with a 400 listing expected against got, `lenient` carries
//! on, `stop` ends the run with a final text. The same JSON is the import/export and cassette
//! format (`kind: "trajectory"`). See docs/m3-spec.md ("Fake agent").

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::agent::tool_map::ToolMap;
use crate::ir::{ApiError, FinishReason, Request, Role, ToolResult};
use crate::script::{Pattern, ReplySpec, validate_leaf_reply};

/// Document marker; the only value is `"trajectory"`. Cassettes (M4) share the field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Trajectory,
}

/// What a failed `expect` (or a request past the last step) means.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Divergence {
    /// 400 `fakellm_trajectory_diverged` / `fakellm_trajectory_exhausted`.
    #[default]
    Strict,
    /// Ignore the mismatch and carry on; past the end, answer `stop_text`.
    Lenient,
    /// Answer `stop_text` with finish `stop`.
    Stop,
}

/// How the step is chosen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cursor {
    /// `Request::assistant_turns()`.
    #[default]
    ByTurn,
    /// A counter of the requests this trajectory has answered, for clients that send no history.
    ByHits,
}

/// What to do with a step whose tool the client did not offer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownTool {
    /// Answer `stop_text` with finish `stop` instead of the tool call.
    Stop,
    /// 400 `fakellm_unknown_tool` listing the client's tools.
    #[default]
    Error,
    /// Send the call anyway.
    Pass,
}

/// One tool result the client must have sent back. Every set field has to hold on the same result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultExpect {
    /// The tool the result answers, in trajectory terms (the tool map is applied before comparing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The id of the call the result answers, as fakellm sent it (set `id` on the previous step's tool call).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Regex over the result text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<Pattern>,
    /// The result text, exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// What the request that asks for this step must look like. Unset means unchecked.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Each entry needs a matching trailing tool result; results nobody expects are ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResultExpect>,
    /// Regex over the text of any user message, `<system-reminder>` blocks removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub any_user: Option<Pattern>,
}

/// One model turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// What the model says: text, tool calls, reasoning, usage, stream timing, or an `error`.
    /// `trajectory` and `policy` are not allowed here.
    pub reply: ReplySpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expect>,
}

/// A registered trajectory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trajectory {
    #[serde(default)]
    pub kind: Kind,
    /// Unique among registered trajectories; no `/`.
    pub name: String,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub on_divergence: Divergence,
    /// Final text for `stop` and for `lenient` past the last step. Default "Trajectory diverged; stopping.".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_text: Option<String>,
    #[serde(default)]
    pub cursor: Cursor,
    #[serde(default, skip_serializing_if = "ToolMap::is_empty")]
    pub tool_map: ToolMap,
    #[serde(default)]
    pub unknown_tool: UnknownTool,
    /// Also treat a replayed assistant tool call without a matching tool result as a divergence
    /// (what real APIs reject, codex's `validate_request_body_invariants`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check_pairing: bool,
    /// Free-form provenance (source dataset, task id, ...); round-trips, never interpreted.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub meta: Map<String, Value>,
}

pub const DEFAULT_STOP_TEXT: &str = "Trajectory diverged; stopping.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrajectoryFormat {
    Json,
    /// A header line `{"kind":"trajectory","name":...}` without `steps`, then one step per line;
    /// a further header line starts the next trajectory.
    Jsonl,
    Yaml,
    /// JSONL when the text has several non-empty lines that each parse as an object (header-plus-step
    /// lines or whole trajectories with `steps`); else JSON if it starts with `[` or `{`; else YAML.
    Auto,
}

/// A trajectory could not be parsed, compiled or found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrajectoryError(pub String);

impl std::fmt::Display for TrajectoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TrajectoryError {}

impl Trajectory {
    /// Parses a document holding trajectories: one object, an array of objects, or an object with a
    /// `trajectories` array (JSON or YAML); or JSONL, see [`TrajectoryFormat::Jsonl`].
    pub fn parse_many(text: &str, format: TrajectoryFormat) -> Result<Vec<Trajectory>, TrajectoryError> {
        let format = match format {
            TrajectoryFormat::Auto => detect(text),
            f => f,
        };
        let docs = match format {
            TrajectoryFormat::Jsonl => parse_jsonl(text)?,
            TrajectoryFormat::Json => {
                let doc = serde_json::from_str(text).map_err(|e| TrajectoryError(format!("invalid JSON: {e}")))?;
                split_documents(doc)?
            }
            _ => {
                let doc = serde_norway::from_str(text).map_err(|e| TrajectoryError(format!("invalid YAML: {e}")))?;
                split_documents(doc)?
            }
        };
        docs.into_iter()
            .enumerate()
            .map(|(i, v)| serde_json::from_value::<Trajectory>(v).map_err(|e| TrajectoryError(format!("trajectory {i}: {e}"))))
            .collect()
    }

    /// Checks everything that does not need a request: non-empty `name` without `/`, at least one
    /// step, every step reply valid and free of `trajectory`/`policy`, `expect` regexes compile,
    /// the tool map is valid.
    pub fn validate(&self) -> Result<(), TrajectoryError> {
        let fail = |m: String| TrajectoryError(format!("trajectory {:?}: {m}", self.name));
        if self.name.is_empty() || self.name.contains('/') {
            return Err(fail("name must be non-empty and contain no `/`".into()));
        }
        if self.steps.is_empty() {
            return Err(fail("needs at least one step".into()));
        }
        self.tool_map.validate().map_err(fail)?;
        for (i, step) in self.steps.iter().enumerate() {
            let at = |m: String| fail(format!("step {i}: {m}"));
            validate_leaf_reply(&step.reply).map_err(at)?;
            let Some(expect) = &step.expect else { continue };
            let patterns = expect
                .tool_results
                .iter()
                .filter_map(|r| r.contains.as_ref())
                .chain(expect.any_user.as_ref());
            for p in patterns {
                p.compile().map_err(|e| at(format!("bad regex {:?}: {e}", p.0)))?;
            }
        }
        Ok(())
    }

    /// The single-document JSON form, `kind` first.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("trajectory serializes")
    }

    /// The JSONL form: the header line, then one line per step; ends with a newline.
    pub fn to_jsonl(&self) -> String {
        let mut header = self.to_json();
        if let Some(o) = header.as_object_mut() {
            o.remove("steps");
        }
        let mut out = header.to_string();
        out.push('\n');
        for step in &self.steps {
            out.push_str(&serde_json::to_string(step).expect("step serializes"));
            out.push('\n');
        }
        out
    }

    fn stop_text(&self) -> &str {
        self.stop_text.as_deref().unwrap_or(DEFAULT_STOP_TEXT)
    }

    fn stop_reply(&self) -> ReplySpec {
        ReplySpec {
            text: Some(self.stop_text().to_string()),
            finish_reason: Some(FinishReason::Stop),
            ..Default::default()
        }
    }
}

fn detect(text: &str) -> TrajectoryFormat {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let objects: Vec<Option<Value>> = if lines.len() > 1 {
        lines
            .iter()
            .map(|l| serde_json::from_str::<Value>(l).ok().filter(Value::is_object))
            .collect()
    } else {
        Vec::new()
    };
    if !objects.is_empty() && objects.iter().all(Option::is_some) {
        TrajectoryFormat::Jsonl
    } else if text.trim_start().starts_with(['[', '{']) {
        TrajectoryFormat::Json
    } else {
        TrajectoryFormat::Yaml
    }
}

fn split_documents(doc: Value) -> Result<Vec<Value>, TrajectoryError> {
    const SHAPE: &str = "expected a trajectory object, an array of them, or an object with a `trajectories` array";
    match doc {
        Value::Array(a) => Ok(a),
        Value::Object(mut o) => match o.remove("trajectories") {
            Some(Value::Array(a)) => Ok(a),
            Some(_) => Err(TrajectoryError(SHAPE.into())),
            None => Ok(vec![Value::Object(o)]),
        },
        Value::Null => Ok(Vec::new()),
        _ => Err(TrajectoryError(SHAPE.into())),
    }
}

fn parse_jsonl(text: &str) -> Result<Vec<Value>, TrajectoryError> {
    let mut docs: Vec<Value> = Vec::new();
    for (n, line) in text
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim()))
        .filter(|(_, l)| !l.is_empty())
    {
        let bad = |m: &str| TrajectoryError(format!("line {n}: {m}"));
        let Value::Object(obj) = serde_json::from_str::<Value>(line).map_err(|e| bad(&format!("invalid JSON: {e}")))? else {
            return Err(bad("expected a JSON object"));
        };
        if obj.contains_key("steps") {
            docs.push(Value::Object(obj));
        } else if obj.contains_key("reply") || obj.contains_key("expect") {
            let steps = docs
                .last_mut()
                .and_then(|d| d.get_mut("steps"))
                .and_then(Value::as_array_mut)
                .ok_or_else(|| bad("step line before any trajectory header"))?;
            steps.push(Value::Object(obj));
        } else {
            let mut header = obj;
            if header.get("kind").and_then(Value::as_str) != Some("trajectory") || !header.contains_key("name") {
                return Err(bad("header line needs `kind: \"trajectory\"` and `name`"));
            }
            header.insert("steps".into(), Value::Array(Vec::new()));
            docs.push(Value::Object(header));
        }
    }
    Ok(docs)
}

fn first_chars(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push_str("...");
    }
    out.replace('\n', " ")
}

fn describe_results(results: &[ToolResult<'_>]) -> String {
    if results.is_empty() {
        return "no tool results".into();
    }
    let items: Vec<String> = results
        .iter()
        .map(|r| {
            format!(
                "{}#{}{} {:?}",
                r.name.unwrap_or("?"),
                r.tool_call_id.unwrap_or("?"),
                if r.is_error { " (error)" } else { "" },
                first_chars(&r.content, 80)
            )
        })
        .collect();
    format!("tool results [{}]", items.join(", "))
}

fn describe_expect(e: &ToolResultExpect) -> String {
    let mut parts = Vec::new();
    if let Some(n) = &e.name {
        parts.push(format!("name={n}"));
    }
    if let Some(i) = &e.id {
        parts.push(format!("id={i}"));
    }
    if let Some(c) = &e.contains {
        parts.push(format!("contains={}", c.0));
    }
    if let Some(q) = &e.equals {
        parts.push(format!("equals={q:?}"));
    }
    if let Some(b) = e.is_error {
        parts.push(format!("is_error={b}"));
    }
    format!("a tool result with {}", parts.join(" "))
}

fn result_satisfies(map: &ToolMap, e: &ToolResultExpect, r: &ToolResult<'_>) -> bool {
    e.name
        .as_deref()
        .is_none_or(|n| r.name.is_some_and(|got| got == map.client_name(n) || got == n))
        && e.id.as_deref().is_none_or(|id| r.tool_call_id == Some(id))
        && e.contains
            .as_ref()
            .is_none_or(|p| p.compile().is_ok_and(|re| re.is_match(&r.content)))
        && e.equals.as_deref().is_none_or(|q| r.content == q)
        && e.is_error.is_none_or(|b| r.is_error == b)
}

/// Gives expectation `i` its own result (augmenting-path matching), so one result never satisfies two expectations.
fn assign(i: usize, fits: &[Vec<bool>], owner: &mut [Option<usize>], seen: &mut [bool]) -> bool {
    for j in 0..owner.len() {
        if !fits[i][j] || seen[j] {
            continue;
        }
        seen[j] = true;
        if owner[j].is_none_or(|k| assign(k, fits, owner, seen)) {
            owner[j] = Some(i);
            return true;
        }
    }
    false
}

/// Every unmet expectation as `expected X, got Y`.
fn check_expect(t: &Trajectory, expect: &Expect, req: &Request) -> Vec<String> {
    let mut failed = Vec::new();
    let results = req.trailing_tool_results();
    let fits: Vec<Vec<bool>> = expect
        .tool_results
        .iter()
        .map(|e| results.iter().map(|r| result_satisfies(&t.tool_map, e, r)).collect())
        .collect();
    let mut owner: Vec<Option<usize>> = vec![None; results.len()];
    for (i, e) in expect.tool_results.iter().enumerate() {
        if !assign(i, &fits, &mut owner, &mut vec![false; results.len()]) {
            failed.push(format!("expected {}, got {}", describe_expect(e), describe_results(&results)));
        }
    }
    if let Some(p) = &expect.any_user {
        let texts: Vec<String> = req
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| crate::ir::strip_system_reminders(&m.text_content()))
            .collect();
        if !p.compile().is_ok_and(|re| texts.iter().any(|x| re.is_match(x))) {
            let last = texts.last().map_or("", String::as_str);
            failed.push(format!(
                "expected a user message matching {}, got {} user messages, last {:?}",
                p.0,
                texts.len(),
                first_chars(last, 80)
            ));
        }
    }
    failed
}

/// Assistant tool calls in the history with no tool result after them.
fn unpaired_calls(req: &Request) -> Vec<String> {
    let mut missing = Vec::new();
    for (i, m) in req.messages.iter().enumerate() {
        for call in &m.tool_calls {
            let answered = req.messages[i + 1..]
                .iter()
                .any(|r| r.role == Role::Tool && r.tool_call_id.as_deref() == Some(call.id.as_str()));
            if !answered {
                missing.push(format!("tool call {} ({}) has no result", call.id, call.name));
            }
        }
    }
    missing
}

/// What the registry says about one trajectory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrajectoryInfo {
    pub name: String,
    pub steps: usize,
    pub on_divergence: Divergence,
    pub cursor: Cursor,
    pub unknown_tool: UnknownTool,
    /// Requests answered from this trajectory, errors and stops included.
    pub hits: u64,
    /// The step the latest request was answered with, if any.
    pub last_step: Option<usize>,
}

/// The step chosen for a request.
#[derive(Clone, Debug, PartialEq)]
pub struct Stepped {
    /// Index of the step that answered; `steps.len()` when a `stop`/`lenient` trajectory ran past its end.
    pub step: usize,
    /// The step's reply with the tool map applied; carries the final `stop_text` for stops.
    pub reply: ReplySpec,
}

/// The registered trajectories, in registration order.
#[derive(Default)]
pub struct Trajectories {
    inner: Mutex<Vec<Compiled>>,
}

struct Compiled {
    trajectory: Trajectory,
    hits: u64,
    last_step: Option<usize>,
}

impl Trajectories {
    /// Replaces all trajectories (hit counters restart). Fails without changing anything if one is invalid
    /// or two share a name.
    pub fn replace(&self, items: Vec<Trajectory>) -> Result<(), TrajectoryError> {
        let compiled = Self::compile(items)?;
        *self.inner.lock() = compiled;
        Ok(())
    }

    /// Adds trajectories, replacing a registered one of the same name in place (its counters restart).
    pub fn upsert(&self, items: Vec<Trajectory>) -> Result<(), TrajectoryError> {
        let compiled = Self::compile(items)?;
        let mut inner = self.inner.lock();
        for c in compiled {
            match inner.iter_mut().find(|x| x.trajectory.name == c.trajectory.name) {
                Some(slot) => *slot = c,
                None => inner.push(c),
            }
        }
        Ok(())
    }

    fn compile(items: Vec<Trajectory>) -> Result<Vec<Compiled>, TrajectoryError> {
        let mut seen = std::collections::HashSet::new();
        for t in &items {
            t.validate()?;
            if !seen.insert(t.name.as_str()) {
                return Err(TrajectoryError(format!("trajectory {:?}: name registered twice", t.name)));
            }
        }
        Ok(items
            .into_iter()
            .map(|trajectory| Compiled {
                trajectory,
                hits: 0,
                last_step: None,
            })
            .collect())
    }

    /// Removes one trajectory; `false` when there was none by that name.
    pub fn remove(&self, name: &str) -> bool {
        let mut inner = self.inner.lock();
        let before = inner.len();
        inner.retain(|c| c.trajectory.name != name);
        inner.len() != before
    }

    pub fn clear(&self) {
        self.inner.lock().clear();
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, name: &str) -> Option<Trajectory> {
        self.inner
            .lock()
            .iter()
            .find(|c| c.trajectory.name == name)
            .map(|c| c.trajectory.clone())
    }

    pub fn list(&self) -> Vec<TrajectoryInfo> {
        self.inner
            .lock()
            .iter()
            .map(|c| TrajectoryInfo {
                name: c.trajectory.name.clone(),
                steps: c.trajectory.steps.len(),
                on_divergence: c.trajectory.on_divergence,
                cursor: c.trajectory.cursor,
                unknown_tool: c.trajectory.unknown_tool,
                hits: c.hits,
                last_step: c.last_step,
            })
            .collect()
    }

    /// Chooses and prepares the reply for `req` from trajectory `name`:
    ///
    /// 1. Unknown name: `ApiError::trajectory_not_found`.
    /// 2. The step index: `req.assistant_turns()` for `by_turn`; for `by_hits` the number of earlier
    ///    calls (the counter then increments). Every call counts in `hits`.
    /// 3. `check_pairing` and the step's `expect` are checked; a mismatch is handled per
    ///    `on_divergence` (strict: `ApiError::trajectory_diverged` with expected against got).
    /// 4. Past the last step: strict gives `ApiError::trajectory_exhausted`, the others a reply
    ///    with only `stop_text`.
    /// 5. The step's tool calls go through the tool map; a call whose client name is not among
    ///    `req.tools` is handled per `unknown_tool` (`ApiError::unknown_tool` for `error`).
    pub fn step(&self, name: &str, req: &Request) -> Result<Stepped, (ApiError, usize)> {
        let mut inner = self.inner.lock();
        let c = inner
            .iter_mut()
            .find(|c| c.trajectory.name == name)
            .ok_or_else(|| (ApiError::trajectory_not_found(name), req.assistant_turns()))?;
        let index = match c.trajectory.cursor {
            Cursor::ByTurn => req.assistant_turns(),
            Cursor::ByHits => c.hits as usize,
        };
        c.hits += 1;
        let result = select(&c.trajectory, index, req).map_err(|e| (e, index));
        c.last_step = result.as_ref().ok().map(|s| s.step).or(Some(index.min(c.trajectory.steps.len())));
        result
    }
}

fn select(t: &Trajectory, index: usize, req: &Request) -> Result<Stepped, ApiError> {
    let stop = |step: usize| Stepped {
        step,
        reply: t.stop_reply(),
    };
    let at = match t.cursor {
        Cursor::ByTurn => format!("{index} (by_turn: {} assistant messages in the request)", req.assistant_turns()),
        Cursor::ByHits => format!("{index} (by_hits: request number {})", index + 1),
    };
    let mut failed = if t.check_pairing { unpaired_calls(req) } else { Vec::new() };
    if index >= t.steps.len() {
        return match (failed.is_empty(), t.on_divergence) {
            (false, Divergence::Strict) => Err(ApiError::trajectory_diverged(&t.name, &at, &failed.join("; "))),
            (_, Divergence::Strict) => Err(ApiError::trajectory_exhausted(&t.name, t.steps.len(), &format!("step {at}"))),
            _ => Ok(stop(t.steps.len())),
        };
    }
    let step = &t.steps[index];
    if let Some(expect) = &step.expect {
        failed.extend(check_expect(t, expect, req));
    }
    if !failed.is_empty() {
        match t.on_divergence {
            Divergence::Strict => return Err(ApiError::trajectory_diverged(&t.name, &at, &failed.join("; "))),
            Divergence::Stop => return Ok(stop(index)),
            Divergence::Lenient => {}
        }
    }
    let mut reply = step.reply.clone();
    if let Some(calls) = reply.tool_calls.as_mut() {
        for call in calls.iter_mut() {
            *call = t.tool_map.apply(call);
        }
        if t.unknown_tool != UnknownTool::Pass
            && let Some(missing) = calls.iter().find(|c| !req.has_tool(&c.name))
        {
            match t.unknown_tool {
                UnknownTool::Stop => return Ok(stop(index)),
                _ => {
                    let offered: Vec<&str> = req.tool_names().collect();
                    return Err(ApiError::unknown_tool(&t.name, index, &missing.name, &offered));
                }
            }
        }
    }
    Ok(Stepped { step: index, reply })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn document_shape_round_trips() {
        let doc = json!({
            "kind": "trajectory",
            "name": "fix-bug",
            "on_divergence": "lenient",
            "stop_text": "done",
            "cursor": "by_hits",
            "unknown_tool": "pass",
            "tool_map": {"bash": {"name": "run_shell", "args": {"cmd": "$command"}}},
            "steps": [
                {"reply": {"tool_calls": [{"name": "bash", "arguments": {"command": "ls"}, "id": "call_1"}]}},
                {"reply": {"text": "ok"}, "expect": {"tool_results": [{"name": "bash", "contains": "Cargo"}], "any_user": "fix"}}
            ]
        });
        let t: Trajectory = serde_json::from_value(doc.clone()).unwrap();
        assert_eq!(
            (t.on_divergence, t.cursor, t.unknown_tool),
            (Divergence::Lenient, Cursor::ByHits, UnknownTool::Pass)
        );
        assert_eq!(t.tool_map.client_name("bash"), "run_shell");
        assert_eq!(t.tool_map.client_name("read"), "read");
        assert_eq!(serde_json::to_value(&t).unwrap(), doc);
        let minimal: Trajectory = serde_json::from_value(json!({"name": "m", "steps": []})).unwrap();
        assert_eq!(
            (minimal.kind, minimal.on_divergence, minimal.cursor),
            (Kind::Trajectory, Divergence::Strict, Cursor::ByTurn)
        );
        assert!(serde_json::from_value::<Trajectory>(json!({"name": "m", "steps": [], "oops": 1})).is_err());
    }

    #[test]
    fn registry_counts_without_stubs() {
        let t = Trajectories::default();
        assert!(t.is_empty());
        t.clear();
        assert_eq!(t.len(), 0);
    }

    use crate::ir::{Message, ToolCall, ToolDef};

    fn traj(v: Value) -> Trajectory {
        serde_json::from_value(v).unwrap()
    }

    fn base(extra: Value) -> Trajectory {
        let mut doc = json!({
            "name": "t",
            "tool_map": {"bash": {"name": "run_shell", "args": {"cmd": "$command"}}},
            "steps": [
                {"reply": {"tool_calls": [{"name": "bash", "arguments": {"command": "ls"}, "id": "call_1"}]}},
                {"reply": {"text": "done"},
                 "expect": {"tool_results": [{"name": "bash", "id": "call_1", "contains": "Cargo"}]}}
            ]
        });
        for (k, v) in extra.as_object().unwrap() {
            doc[k] = v.clone();
        }
        traj(doc)
    }

    fn tool_def(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: None,
            parameters: None,
            strict: None,
        }
    }

    fn first(tools: &[&str]) -> Request {
        let mut r = Request::new("m", vec![Message::text(Role::User, "fix it")]);
        r.tools = tools.iter().map(|t| tool_def(t)).collect();
        r
    }

    fn second(tools: &[&str], result: &str) -> Request {
        let mut r = first(tools);
        let mut a = Message::text(Role::Assistant, "");
        a.tool_calls.push(ToolCall {
            id: "call_1".into(),
            name: "run_shell".into(),
            arguments: "{}".into(),
        });
        let mut t = Message::text(Role::Tool, result);
        t.tool_call_id = Some("call_1".into());
        r.messages.push(a);
        r.messages.push(t);
        r
    }

    fn registry(t: Trajectory) -> Trajectories {
        let r = Trajectories::default();
        r.replace(vec![t]).unwrap();
        r
    }

    #[test]
    fn selects_step_by_turn_and_maps_tools() {
        let r = registry(base(json!({})));
        let s0 = r.step("t", &first(&["run_shell"])).unwrap();
        assert_eq!(s0.step, 0);
        let call = &s0.reply.tool_calls.unwrap()[0];
        assert_eq!(
            (call.name.as_str(), &call.arguments, call.id.as_deref()),
            ("run_shell", &json!({"cmd": "ls"}), Some("call_1"))
        );
        let again = r.step("t", &first(&["run_shell"])).unwrap();
        assert_eq!(again.step, 0, "a retry gets the same step");
        let s1 = r.step("t", &second(&["run_shell"], "Cargo.toml")).unwrap();
        assert_eq!((s1.step, s1.reply.text.as_deref()), (1, Some("done")));
        let info = &r.list()[0];
        assert_eq!((info.hits, info.last_step), (3, Some(1)));
    }

    #[test]
    fn by_hits_counts_calls() {
        let r = registry(base(json!({"cursor": "by_hits", "on_divergence": "lenient"})));
        let req = first(&["run_shell"]);
        assert_eq!(r.step("t", &req).unwrap().step, 0);
        assert_eq!(r.step("t", &req).unwrap().step, 1);
        let past = r.step("t", &req).unwrap();
        assert_eq!((past.step, past.reply.text.as_deref()), (2, Some(DEFAULT_STOP_TEXT)));
        r.upsert(vec![base(json!({"cursor": "by_hits"}))]).unwrap();
        assert_eq!(r.list()[0].hits, 0);
    }

    #[test]
    fn strict_divergence_and_exhaustion() {
        let r = registry(base(json!({})));
        let err = r.step("t", &second(&["run_shell"], "nothing")).unwrap_err().0;
        assert_eq!((err.status, err.code.as_deref()), (400, Some("fakellm_trajectory_diverged")));
        assert!(
            err.message.contains("contains=Cargo") && err.message.contains("run_shell#call_1"),
            "{}",
            err.message
        );
        let mut past = second(&["run_shell"], "Cargo");
        past.messages.push(Message::text(Role::Assistant, "done"));
        let err = r.step("t", &past).unwrap_err().0;
        assert_eq!(err.code.as_deref(), Some("fakellm_trajectory_exhausted"));
        assert_eq!(r.list()[0].hits, 2);
        assert!(matches!(
            r.step("nope", &past).unwrap_err().0.code.as_deref(),
            Some("fakellm_trajectory_not_found")
        ));
    }

    #[test]
    fn lenient_and_stop_divergence() {
        let bad = second(&["run_shell"], "nothing");
        let lenient = registry(base(json!({"on_divergence": "lenient"})));
        assert_eq!(lenient.step("t", &bad).unwrap().reply.text.as_deref(), Some("done"));
        let stop = registry(base(json!({"on_divergence": "stop", "stop_text": "halt"})));
        let s = stop.step("t", &bad).unwrap();
        assert_eq!(
            (s.step, s.reply.text.as_deref(), s.reply.finish_reason),
            (1, Some("halt"), Some(FinishReason::Stop))
        );
        assert!(s.reply.tool_calls.is_none());
    }

    #[test]
    fn expect_fields() {
        let t = traj(
            json!({"name": "e", "on_divergence": "lenient", "tool_map": {"bash": {"name": "run_shell"}}, "steps": [{"reply": {"text": "x"}}]}),
        );
        let req = second(&[], "Cargo.toml\nsrc");
        let check = |e: Value| check_expect(&t, &serde_json::from_value(e).unwrap(), &req).is_empty();
        assert!(check(json!({"tool_results": [{"name": "run_shell"}]})));
        assert!(check(
            json!({"tool_results": [{"name": "bash", "id": "call_1", "equals": "Cargo.toml\nsrc", "is_error": false}]})
        ));
        assert!(check(json!({"tool_results": [{"contains": "/cargo/i"}]})));
        assert!(
            !check(json!({"tool_results": [{"contains": "/cargo/i"}, {"contains": "src"}]})),
            "one result serves one expectation"
        );
        assert!(!check(json!({"tool_results": [{"name": "other"}]})));
        assert!(!check(json!({"tool_results": [{"equals": "Cargo"}]})));
        assert!(!check(json!({"tool_results": [{"is_error": true}]})));
        assert!(!check(json!({"tool_results": [{"id": "call_9"}]})));
        assert!(check(json!({"any_user": "fix"})));
        assert!(!check(json!({"any_user": "deploy"})));
        let mut reminded = req.clone();
        reminded
            .messages
            .push(Message::text(Role::User, "<system-reminder>deploy</system-reminder>"));
        let e: Expect = serde_json::from_value(json!({"any_user": "deploy"})).unwrap();
        assert!(!check_expect(&t, &e, &reminded).is_empty());
    }

    #[test]
    fn check_pairing_flags_unanswered_calls() {
        let r = registry(base(json!({"check_pairing": true})));
        let mut req = second(&["run_shell"], "Cargo");
        req.messages.pop();
        let err = r.step("t", &req).unwrap_err().0;
        assert!(
            err.message.contains("tool call call_1 (run_shell) has no result"),
            "{}",
            err.message
        );
        assert!(r.step("t", &second(&["run_shell"], "Cargo")).is_ok());
    }

    #[test]
    fn unknown_tool_modes() {
        let offered = first(&["read_file"]);
        let err = registry(base(json!({}))).step("t", &offered).unwrap_err().0;
        assert_eq!(err.code.as_deref(), Some("fakellm_unknown_tool"));
        assert!(
            err.message.contains("read_file") && err.message.contains("run_shell"),
            "{}",
            err.message
        );
        let stop = registry(base(json!({"unknown_tool": "stop"}))).step("t", &first(&[])).unwrap();
        assert_eq!(stop.reply.text.as_deref(), Some(DEFAULT_STOP_TEXT));
        let pass = registry(base(json!({"unknown_tool": "pass"}))).step("t", &first(&[])).unwrap();
        assert_eq!(pass.reply.tool_calls.unwrap()[0].name, "run_shell");
    }

    #[test]
    fn validation_errors_name_the_step() {
        let r = Trajectories::default();
        let bad = |v: Value| r.replace(vec![traj(v)]).unwrap_err().0;
        assert!(bad(json!({"name": "a/b", "steps": [{"reply": {"text": "x"}}]})).contains("name"));
        assert!(bad(json!({"name": "a", "steps": []})).contains("at least one step"));
        let e = bad(json!({"name": "a", "steps": [{"reply": {"text": "x"}}, {"reply": {"text": "y"}, "expect": {"any_user": "("}}]}));
        assert!(e.contains("step 1") && e.contains("regex"), "{e}");
        assert!(bad(json!({"name": "a", "steps": [{"reply": {"trajectory": "b"}}]})).contains("step 0"));
        assert!(bad(json!({"name": "a", "steps": [{"reply": {"text": "x"}}], "tool_map": {"t": {"name": ""}}})).contains("tool_map"));
        let dup = base(json!({}));
        assert!(r.replace(vec![dup.clone(), dup.clone()]).unwrap_err().0.contains("twice"));
        r.replace(vec![dup]).unwrap();
        assert!(r.replace(vec![traj(json!({"name": "a", "steps": []}))]).is_err());
        assert_eq!(r.len(), 1, "a failed replace changes nothing");
        assert!(r.remove("t") && !r.remove("t"));
    }

    #[test]
    fn parses_json_yaml_and_jsonl() {
        let one = r#"{"name":"a","steps":[{"reply":{"text":"x"}}]}"#;
        assert_eq!(Trajectory::parse_many(one, TrajectoryFormat::Auto).unwrap().len(), 1);
        let arr = format!("[{one},{one}]");
        assert_eq!(Trajectory::parse_many(&arr, TrajectoryFormat::Json).unwrap().len(), 2);
        let wrapped = format!("{{\"trajectories\":[{one}]}}");
        assert_eq!(Trajectory::parse_many(&wrapped, TrajectoryFormat::Auto).unwrap().len(), 1);
        let yaml = "name: y\nsteps:\n  - reply: { text: hi }\n  - reply: { text: bye }\n";
        let y = Trajectory::parse_many(yaml, TrajectoryFormat::Auto).unwrap();
        assert_eq!((y[0].name.as_str(), y[0].steps.len()), ("y", 2));
        let err = Trajectory::parse_many(r#"[{"name":"a","steps":[],"oops":1}]"#, TrajectoryFormat::Auto).unwrap_err();
        assert!(err.0.contains("trajectory 0"), "{err}");

        let jsonl = "{\"kind\":\"trajectory\",\"name\":\"a\"}\n{\"reply\":{\"text\":\"1\"}}\n\n{\"reply\":{\"text\":\"2\"}}\n{\"kind\":\"trajectory\",\"name\":\"b\"}\n{\"reply\":{\"text\":\"3\"}}\n";
        let j = Trajectory::parse_many(jsonl, TrajectoryFormat::Auto).unwrap();
        assert_eq!(
            j.iter().map(|t| (t.name.as_str(), t.steps.len())).collect::<Vec<_>>(),
            [("a", 2), ("b", 1)]
        );
        let whole = format!("{one}\n{one}\n");
        assert_eq!(Trajectory::parse_many(&whole, TrajectoryFormat::Jsonl).unwrap().len(), 2);
        let orphan = Trajectory::parse_many(
            "{\"reply\":{\"text\":\"1\"}}\n{\"reply\":{\"text\":\"2\"}}",
            TrajectoryFormat::Jsonl,
        )
        .unwrap_err();
        assert!(orphan.0.contains("line 1"), "{orphan}");
        assert!(Trajectory::parse_many("{\"name\":\"a\"}\n{\"reply\":{\"text\":\"1\"}}", TrajectoryFormat::Jsonl).is_err());
    }

    #[test]
    fn export_round_trips() {
        let t = base(json!({"meta": {"src": "x"}, "check_pairing": true, "stop_text": "s"}));
        let again = Trajectory::parse_many(&t.to_json().to_string(), TrajectoryFormat::Auto).unwrap();
        assert_eq!(again, vec![t.clone()]);
        let jsonl = t.to_jsonl();
        assert!(jsonl.ends_with('\n') && jsonl.starts_with("{\"kind\":\"trajectory\""));
        assert_eq!(jsonl.lines().count(), 1 + t.steps.len());
        assert_eq!(Trajectory::parse_many(&jsonl, TrajectoryFormat::Auto).unwrap(), vec![t]);
    }
}
