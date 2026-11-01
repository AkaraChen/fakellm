//! The cassette document: recorded request/outcome pairs in IR form, so one recording replays through every
//! protocol. It shares its building blocks with trajectories (`ReplySpec`, the `kind` marker, the JSON / YAML /
//! JSONL import and export rules of docs/m3-spec.md section 3.2), and a cassette converts to a trajectory by
//! taking the assistant replies in order ([`Cassette::to_trajectory`]).
//!
//! Exchanges store the IR [`Request`] and the part of an [`Outcome`] that is data (a reply or an error); how
//! it is delivered (timing, chunking) and where it came from are the replaying server's business.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::agent::trajectory::{Expect, Step, ToolResultExpect, Trajectory};
use crate::cassette::CassetteError;
use crate::cassette::normalize::IgnoreSet;
use crate::cassette::replay::Matcher;
use crate::ir::{ApiError, Delivery, Outcome, Protocol, Reply, Request, Source};
use crate::reqlog::glob_match;
use crate::script::{ErrorSpec, ReplySpec, ToolCallSpec, UsageSpec};

/// The fields matching drops by default, unless the cassette says `!field` (docs/m4-spec.md section 3.2).
/// `tool_choice=auto` drops the field only when it is `auto` or absent. `extra.*` drops all of `Request::extra`.
pub const DEFAULT_IGNORE: &[&str] = &[
    "temperature",
    "top_p",
    "seed",
    "user",
    "metadata",
    "stream",
    "include_usage",
    "store",
    "max_tokens",
    "reasoning_effort",
    "tool_choice=auto",
    "previous_response_id",
    "extra.*",
];

/// Document marker; the only value is `"cassette"`. Trajectories use the same field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Cassette,
}

/// A regex substitution applied to every text of a request before comparing (both sides).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Normalizer {
    /// Rust regex syntax; compiled at registration.
    pub pattern: String,
    /// Replacement; `$1`-style group references work.
    #[serde(default)]
    pub replace: String,
}

/// Facts about the recorded exchange that matching does not use.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExchangeMeta {
    /// The model named by the recorded request (filled from `request.model` when left out).
    #[serde(default)]
    pub model: String,
    /// Wall-clock time from sending to the last byte of the upstream response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The HTTP status the upstream answered with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<u16>,
    /// Anything else (importers put the dataset row id here); round-trips, never interpreted.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// What the recorded call produced. The JSON has the shape of [`Outcome`]'s `reply` and `error` variants
/// without `delivery` and `source`, which a replay supplies; a full `Outcome` JSON is accepted on read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CassetteOutcome {
    Reply { reply: Reply },
    Error { error: ApiError },
}

impl CassetteOutcome {
    /// The data of a reply or error outcome; `None` for a `Hang` (nothing to record).
    pub fn from_outcome(outcome: &Outcome) -> Option<Self> {
        match outcome.inner() {
            Outcome::Reply { reply, .. } => Some(Self::Reply { reply: reply.clone() }),
            Outcome::Error { error, .. } => Some(Self::Error { error: error.clone() }),
            Outcome::Hang { .. } => None,
        }
    }

    /// The outcome to deliver: this data with `delivery` and `source`.
    pub fn into_outcome(self, delivery: Delivery, source: Source) -> Outcome {
        match self {
            Self::Reply { reply } => Outcome::Reply { reply, delivery, source },
            Self::Error { error } => Outcome::Error { error, delivery, source },
        }
    }
}

/// One recorded round trip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exchange {
    /// The request parsed into the IR, exactly as logged.
    pub request: Request,
    pub outcome: CassetteOutcome,
    /// The protocol the exchange was recorded under. Replay is cross-protocol: any request protocol can be
    /// answered from any exchange.
    #[serde(default)]
    pub protocol: Protocol,
    #[serde(default)]
    pub meta: ExchangeMeta,
    /// Fields to ignore for this exchange on top of the cassette's list (same syntax).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

fn any_model() -> String {
    "*".into()
}

fn is_any_model(s: &String) -> bool {
    s == "*"
}

/// A registered cassette. Matching options are fields of the document so a recording carries its own rules.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cassette {
    #[serde(default)]
    pub kind: Kind,
    /// Unique among registered cassettes; no `/`; not `recording` (the in-memory recording owns that name).
    pub name: String,
    /// RFC 3339, when the recording started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<String>,
    /// The `--upstream` URL it was recorded from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Request fields to ignore when matching, added to [`DEFAULT_IGNORE`]; `!field` removes a default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    /// Regex substitutions over every text, added to the default set (ISO timestamps and dates, UUIDs, `req_...` ids).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub normalize: Vec<Normalizer>,
    /// `false` leaves the default normalizers out.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub normalize_defaults: bool,
    /// Compare tool parameter schemas too; by default only the set of tool names.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub match_tool_schemas: bool,
    /// Glob over the *request's* model; the cassette only answers matching requests. Default `*`.
    #[serde(default = "any_model", skip_serializing_if = "is_any_model")]
    pub model_glob: String,
    /// Answer with the nearest exchange when none matches exactly.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fuzzy: bool,
    /// Each exchange answers once, in order of recording among equals; a spent exchange is skipped.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub once: bool,
    /// Free-form provenance; round-trips, never interpreted.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub meta: Map<String, Value>,
    /// May be empty (a freshly opened record file).
    #[serde(default)]
    pub exchanges: Vec<Exchange>,
}

/// How a cassette text is split into documents; same detection rules as [`crate::TrajectoryFormat`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CassetteFormat {
    Json,
    /// A header line `{"kind":"cassette","name":...}` without `exchanges`, then one exchange per line; a further
    /// header line starts the next cassette; a line with `exchanges` is a whole document.
    Jsonl,
    Yaml,
    Auto,
}

/// What [`Cassette::to_trajectory`] keeps and adds.
#[derive(Clone, Debug, Default)]
pub struct ToTrajectory {
    /// Trajectory name; default: the cassette's.
    pub name: Option<String>,
    /// Write `expect.tool_results` (`name` and `id`) for the tool results each kept request carried.
    pub with_expect: bool,
    /// Keep exchanges whose outcome is an error as `error` steps (default: skip them).
    pub keep_errors: bool,
    /// Only exchanges whose request model matches this glob.
    pub model: Option<String>,
}

impl Cassette {
    /// An empty cassette with default options.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            kind: Kind::Cassette,
            name: name.into(),
            recorded_at: None,
            upstream: None,
            ignore: Vec::new(),
            normalize: Vec::new(),
            normalize_defaults: true,
            match_tool_schemas: false,
            model_glob: "*".into(),
            fuzzy: false,
            once: false,
            meta: Map::new(),
            exchanges: Vec::new(),
        }
    }

    /// Parses a text holding cassettes: one object, an array, `{"cassettes": [...]}` (JSON or YAML), or JSONL.
    /// Fills `meta.model` from the request where it is empty, and defaults `exchange.protocol` to the request's
    /// when the document gave none. Does not validate; registration does ([`Cassette::validate`]).
    pub fn parse_many(text: &str, format: CassetteFormat) -> Result<Vec<Cassette>, CassetteError> {
        let format = match format {
            CassetteFormat::Auto => detect(text),
            f => f,
        };
        let docs = match format {
            CassetteFormat::Jsonl => parse_jsonl(text)?,
            CassetteFormat::Json => {
                let doc = serde_json::from_str(text).map_err(|e| CassetteError(format!("invalid JSON: {e}")))?;
                split_documents(doc)?
            }
            _ => {
                let doc = serde_norway::from_str(text).map_err(|e| CassetteError(format!("invalid YAML: {e}")))?;
                split_documents(doc)?
            }
        };
        docs.into_iter()
            .enumerate()
            .map(|(i, mut doc)| {
                check_not_trajectory(&doc)?;
                fill_protocols(&mut doc);
                let mut c = serde_json::from_value::<Cassette>(doc).map_err(|e| CassetteError(format!("cassette {i}: {e}")))?;
                for e in &mut c.exchanges {
                    if e.meta.model.is_empty() {
                        e.meta.model = e.request.model.clone();
                    }
                }
                Ok(c)
            })
            .collect()
    }

    /// Everything that needs no request: `name` non-empty without `/` and not `recording`; `ignore` entries known
    /// (docs/m4-spec.md section 3.2); `normalize` patterns and `model_glob` usable; every exchange has messages.
    /// Errors name the cassette and the exchange index.
    pub fn validate(&self) -> Result<(), CassetteError> {
        let fail = |m: String| CassetteError(format!("cassette {:?}: {m}", self.name));
        if self.name.is_empty() || self.name.contains('/') || self.name == crate::cassette::RECORDING {
            return Err(fail("name must be non-empty, contain no `/` and not be `recording`".into()));
        }
        Matcher::compile(self).map_err(|e| fail(e.0))?;
        for (i, e) in self.exchanges.iter().enumerate() {
            let at = |m: String| fail(format!("exchange {i}: {m}"));
            if e.request.messages.is_empty() {
                return Err(at("request has no messages".into()));
            }
            IgnoreSet::compile(&[&self.ignore, &e.ignore]).map_err(|err| at(err.0))?;
        }
        Ok(())
    }

    /// The single-document JSON form, `kind` first.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("cassette serializes")
    }

    /// The JSONL form: the header line (the document without `exchanges`), then one line per exchange; ends with `\n`.
    pub fn to_jsonl(&self) -> String {
        let mut header = self.to_json();
        if let Some(o) = header.as_object_mut() {
            o.remove("exchanges");
        }
        let mut out = header.to_string();
        out.push('\n');
        for exchange in &self.exchanges {
            out.push_str(&serde_json::to_string(exchange).expect("exchange serializes"));
            out.push('\n');
        }
        out
    }

    /// The assistant replies in order, as a trajectory (docs/m4-spec.md section 3.5): an exchange becomes the next
    /// step when its request carries exactly as many assistant messages as steps so far, so side requests (titles,
    /// other conversations) and retries drop out and the result replays with `cursor: by_turn`.
    /// Fails when no exchange qualifies.
    pub fn to_trajectory(&self, opts: &ToTrajectory) -> Result<Trajectory, CassetteError> {
        let mut steps: Vec<Step> = Vec::new();
        for exchange in &self.exchanges {
            if opts.model.as_deref().is_some_and(|g| !glob_match(g, &exchange.request.model)) {
                continue;
            }
            if exchange.request.assistant_turns() != steps.len() {
                continue;
            }
            let reply = match &exchange.outcome {
                CassetteOutcome::Reply { reply } => reply_spec(reply),
                CassetteOutcome::Error { error } if opts.keep_errors => ReplySpec {
                    error: Some(error_spec(error)),
                    ..Default::default()
                },
                CassetteOutcome::Error { .. } => continue,
            };
            let expect = if opts.with_expect {
                let tool_results: Vec<ToolResultExpect> = exchange
                    .request
                    .trailing_tool_results()
                    .iter()
                    .map(|r| ToolResultExpect {
                        name: r.name.map(str::to_string),
                        id: r.tool_call_id.map(str::to_string),
                        ..Default::default()
                    })
                    .collect();
                (!tool_results.is_empty()).then_some(Expect {
                    tool_results,
                    any_user: None,
                })
            } else {
                None
            };
            steps.push(Step { reply, expect });
        }
        if steps.is_empty() {
            return Err(CassetteError(format!(
                "no exchange of cassette {:?} starts a conversation",
                self.name
            )));
        }
        let mut meta = Map::new();
        meta.insert("source".into(), "cassette".into());
        meta.insert("cassette".into(), self.name.clone().into());
        if let Some(u) = &self.upstream {
            meta.insert("upstream".into(), u.clone().into());
        }
        let mut trajectory: Trajectory = serde_json::from_value(serde_json::json!({
            "name": opts.name.clone().unwrap_or_else(|| self.name.clone()),
            "steps": [],
            "meta": meta,
        }))
        .map_err(|e| CassetteError(e.to_string()))?;
        trajectory.steps = steps;
        Ok(trajectory)
    }
}

fn error_spec(error: &ApiError) -> ErrorSpec {
    ErrorSpec {
        status: error.status,
        kind: Some(error.kind.clone()),
        code: error.code.clone(),
        message: Some(error.message.clone()),
        param: error.param.clone(),
        retry_after: error.retry_after.map(|d| d.as_secs_f64()),
        in_stream: error.in_stream.then_some(true),
        headers: (!error.headers.is_empty()).then(|| error.headers.iter().cloned().collect()),
    }
}

fn detect(text: &str) -> CassetteFormat {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let all_objects = lines.len() > 1 && lines.iter().all(|l| serde_json::from_str::<Value>(l).is_ok_and(|v| v.is_object()));
    if all_objects {
        CassetteFormat::Jsonl
    } else if text.trim_start().starts_with(['[', '{']) {
        CassetteFormat::Json
    } else {
        CassetteFormat::Yaml
    }
}

fn split_documents(doc: Value) -> Result<Vec<Value>, CassetteError> {
    const SHAPE: &str = "expected a cassette object, an array of them, or an object with a `cassettes` array";
    match doc {
        Value::Array(a) => Ok(a),
        Value::Object(mut o) => match o.remove("cassettes") {
            Some(Value::Array(a)) => Ok(a),
            Some(_) => Err(CassetteError(SHAPE.into())),
            None => Ok(vec![Value::Object(o)]),
        },
        Value::Null => Ok(Vec::new()),
        _ => Err(CassetteError(SHAPE.into())),
    }
}

fn parse_jsonl(text: &str) -> Result<Vec<Value>, CassetteError> {
    let mut docs: Vec<Value> = Vec::new();
    for (n, line) in text
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim()))
        .filter(|(_, l)| !l.is_empty())
    {
        let bad = |m: &str| CassetteError(format!("line {n}: {m}"));
        let Value::Object(obj) = serde_json::from_str::<Value>(line).map_err(|e| bad(&format!("invalid JSON: {e}")))? else {
            return Err(bad("expected a JSON object"));
        };
        check_not_trajectory(&Value::Object(obj.clone())).map_err(|e| bad(&e.0))?;
        if obj.contains_key("exchanges") {
            docs.push(Value::Object(obj));
        } else if obj.contains_key("request") || obj.contains_key("outcome") {
            let exchanges = docs
                .last_mut()
                .and_then(|d| d.get_mut("exchanges"))
                .and_then(Value::as_array_mut)
                .ok_or_else(|| bad("exchange line before any cassette header"))?;
            exchanges.push(Value::Object(obj));
        } else {
            let mut header = obj;
            if header.get("kind").and_then(Value::as_str) != Some("cassette") || !header.contains_key("name") {
                return Err(bad("header line needs `kind: \"cassette\"` and `name`"));
            }
            header.insert("exchanges".into(), Value::Array(Vec::new()));
            docs.push(Value::Object(header));
        }
    }
    Ok(docs)
}

fn check_not_trajectory(doc: &Value) -> Result<(), CassetteError> {
    let is_trajectory = doc.get("kind").and_then(Value::as_str) == Some("trajectory") || doc.get("steps").is_some();
    if is_trajectory {
        return Err(CassetteError("this is a trajectory; use --trajectory".into()));
    }
    Ok(())
}

/// Exchanges that leave out `protocol` get the protocol of their request (which itself defaults to Chat).
fn fill_protocols(doc: &mut Value) {
    let Some(exchanges) = doc.get_mut("exchanges").and_then(Value::as_array_mut) else {
        return;
    };
    for e in exchanges {
        let Some(obj) = e.as_object_mut() else { continue };
        if obj.contains_key("protocol") {
            continue;
        }
        if let Some(p) = obj.get("request").and_then(|r| r.get("protocol")).cloned() {
            obj.insert("protocol".into(), p);
        }
    }
}

/// The reply as a trajectory step reply: text, reasoning, tool calls (ids kept, arguments parsed as JSON when
/// they are valid JSON, else kept as the string), `finish_reason`, and the output side of `usage`. Prompt-side counts
/// are left out: they depend on the replayed request (and mean different things per protocol), so the engine counts them.
pub fn reply_spec(reply: &Reply) -> ReplySpec {
    ReplySpec {
        text: reply.text.clone(),
        tool_calls: (!reply.tool_calls.is_empty()).then(|| {
            reply
                .tool_calls
                .iter()
                .map(|c| ToolCallSpec {
                    name: c.name.clone(),
                    arguments: serde_json::from_str(&c.arguments).unwrap_or_else(|_| Value::String(c.arguments.clone())),
                    id: Some(c.id.clone()),
                })
                .collect()
        }),
        reasoning: reply.reasoning.clone(),
        reasoning_opaque: reply.reasoning_opaque.clone(),
        finish_reason: Some(reply.finish),
        usage: Some(UsageSpec {
            completion_tokens: Some(reply.usage.completion_tokens),
            reasoning_tokens: Some(reply.usage.reasoning_tokens),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ir::{FinishReason, Message, Role, ToolCall};

    fn exchange() -> Exchange {
        let mut request = Request::new("gpt-4o", vec![Message::text(Role::User, "hi")]);
        request.protocol = Protocol::OpenaiChat;
        Exchange {
            request,
            outcome: CassetteOutcome::Reply {
                reply: Reply {
                    text: Some("yo".into()),
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "ls".into(),
                        arguments: r#"{"path":"."}"#.into(),
                    }],
                    finish: FinishReason::ToolCalls,
                    ..Reply::default()
                },
            },
            protocol: Protocol::OpenaiChat,
            meta: ExchangeMeta {
                model: "gpt-4o".into(),
                duration_ms: Some(12),
                upstream_status: Some(200),
                extra: Map::new(),
            },
            ignore: Vec::new(),
        }
    }

    #[test]
    fn document_shape_and_jsonl() {
        let mut c = Cassette::new("weather");
        c.exchanges.push(exchange());
        let doc = c.to_json();
        assert_eq!(doc["kind"], "cassette");
        assert_eq!(doc["exchanges"][0]["outcome"]["outcome"], "reply");
        assert!(doc.get("fuzzy").is_none() && doc.get("model_glob").is_none() && doc.get("normalize_defaults").is_none());
        assert_eq!(serde_json::from_value::<Cassette>(doc).unwrap(), c);
        let jsonl = c.to_jsonl();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 2);
        let header: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            (header["kind"].as_str(), header["name"].as_str()),
            (Some("cassette"), Some("weather"))
        );
        assert!(header.get("exchanges").is_none());
        assert_eq!(serde_json::from_str::<Exchange>(lines[1]).unwrap(), c.exchanges[0]);
    }

    #[test]
    fn hand_written_exchanges_may_omit_defaults() {
        let e: Exchange = serde_json::from_value(json!({
            "request": {"model": "m", "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]},
            "outcome": {"outcome": "reply", "reply": {"text": "yo", "finish": "stop", "usage": {"prompt_tokens": 1, "completion_tokens": 1}}}
        }))
        .unwrap();
        assert_eq!(e.protocol, Protocol::OpenaiChat);
        assert!(!e.request.stream && e.meta.model.is_empty());
        let full: CassetteOutcome = serde_json::from_value(json!({
            "outcome": "error",
            "error": {"status": 429, "kind": "requests", "message": "slow"},
            "delivery": {"ttft": {"secs": 0, "nanos": 0}, "token_interval": {"secs": 0, "nanos": 0}, "chunk_tokens": 1, "args_chunk_bytes": 16},
            "source": {"kind": "fallback"}
        }))
        .unwrap();
        assert!(matches!(full, CassetteOutcome::Error { error } if error.status == 429));
    }

    #[test]
    fn outcome_conversion_and_reply_spec() {
        let CassetteOutcome::Reply { reply } = exchange().outcome else {
            unreachable!()
        };
        let outcome = CassetteOutcome::Reply { reply: reply.clone() }.into_outcome(Delivery::default(), Source::Upstream);
        assert_eq!(
            CassetteOutcome::from_outcome(&outcome),
            Some(CassetteOutcome::Reply { reply: reply.clone() })
        );
        let hang = Outcome::Hang {
            until: None,
            then: Box::new(Outcome::rejected(ApiError::server_error())),
        };
        assert!(matches!(CassetteOutcome::from_outcome(&hang), Some(CassetteOutcome::Error { .. })));
        let spec = reply_spec(&reply);
        assert_eq!(spec.text.as_deref(), Some("yo"));
        let call = &spec.tool_calls.unwrap()[0];
        assert_eq!(
            (call.name.as_str(), call.id.as_deref(), &call.arguments),
            ("ls", Some("call_1"), &json!({"path": "."}))
        );
        assert_eq!(spec.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(spec.usage.unwrap().prompt_tokens, None);
    }
}
