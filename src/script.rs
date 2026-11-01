//! Scripts: ordered rules that match a parsed request and say what to answer. The first rule
//! whose conditions all hold, and whose `times` budget is not spent, wins. Rules are registered
//! through the control plane (`PUT /_fakellm/scripts`, JSON or YAML) or `Config::scripts`.
//!
//! Matching is stateful on purpose: `has_tool_result` and `last_tool_result` let one rule answer
//! the first turn with a tool call and another answer the turn after the tool ran, and `times`
//! stops a rule from re-matching forever, which is how fake models send agent loops into
//! thousands of iterations.
//!
//! The [`Engine`] is the whole decision: scripts first, then the fallback, or an error in strict
//! mode. It also fills in `usage` and applies `max_tokens`, so frontends only render.

use std::collections::BTreeMap;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use regex::Regex;

use crate::agent::trajectory::Trajectories;
use crate::cassette::Cassettes;
use crate::fallback::{self, FallbackConfig};
use crate::ir::{ApiError, Delivery, FinishReason, Outcome, Protocol, Reply, Request, Source, ToolCall, Usage};
use crate::policy::Policies;
use crate::reqlog::glob_match;
use crate::tokenizer;

/// A regular expression, written as `"pattern"`, `"/pattern/"` or `"/pattern/i"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Pattern(pub String);

/// Conditions a request must meet. Unset fields match everything; set fields all have to hold.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Match {
    /// Glob over the model name: `gpt-*`, `*-mini`, `claude-?-*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Regex over the most recent user message with `<system-reminder>` blocks removed
    /// (`Request::last_user_text`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user: Option<Pattern>,
    /// Regex over the most recent user message exactly as sent (`Request::last_user_text_raw`),
    /// reminders included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_raw: Option<Pattern>,
    /// Regex over the system/developer text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<Pattern>,
    /// Regex over the text of any message, any role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub any_message: Option<Pattern>,
    /// Regex over the raw JSON body text of the request, as the client sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Pattern>,
    /// A tool with this name is offered in `tools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_tool: Option<String>,
    /// `true`: no tools offered. `false`: at least one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_tools: Option<bool>,
    /// `true`: the conversation ends with tool results. `false`: it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_tool_result: Option<bool>,
    /// The trailing tool results include one for a function with this name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_result: Option<String>,
    /// Regex over the text of the trailing tool results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_contains: Option<Pattern>,
    /// The trailing tool results include one answering the tool call with exactly this id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_call_id: Option<String>,
    /// Streaming requests only (`true`) or sync only (`false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Number of assistant turns so far (0 on the first request).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<usize>,
    /// Explicit catch-all, for readability; `{}` matches everything too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub any: Option<bool>,
    /// Only requests that came in on this API (`openai.chat`, `openai.responses`, `anthropic.messages`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    /// Match a request with this probability (0..=1) once every other condition holds. Decided
    /// by a deterministic xorshift seeded from `fallback::request_seed` and the rule's number
    /// of earlier matches, so a run is reproducible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probability: Option<f64>,
    /// Match the nth, 2nth, 3nth ... request that reached this rule (every other condition
    /// held), counting from 1; `every_nth: 3` matches the 3rd, 6th, 9th.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_nth: Option<u32>,
    /// Glob over the request path (`/v1/systemone`, `/v1/*`), after the gateway prefix is removed.
    /// A rule with `path` is a custom endpoint rule: it applies to any path under the API, known
    /// or not, needs `reply.body`, and only ever sees requests the standard routes do not serve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Sequence matcher: only once the rule with this `name` has answered at least `after_count` times
    /// (default 1) in this server. Lets a later rule take over from an earlier one without relying on rule
    /// order and `times`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// Minimum hits of the `after` rule, at least 1. Needs `after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_count: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallSpec {
    pub name: String,
    /// An object (serialized compactly) or a string sent verbatim, valid JSON or not.
    #[serde(default)]
    pub arguments: Value,
    /// Generated from the request seed when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorSpec {
    pub status: u16,
    /// Defaults to the real API's `type` for this status (`ApiError::for_status`).
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "type")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    /// Seconds, sent as `retry-after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<f64>,
    /// Deliver the error inside the event stream of a streaming request, see [`ApiError::in_stream`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_stream: Option<bool>,
    /// Extra response headers, e.g. `retry-after-ms`, `x-should-retry`, `x-codex-active-limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamSpec {
    /// Time to first token, server-clock milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_tokens: Option<usize>,
    /// Drop the connection after this many content chunks, without finishing the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_after_chunks: Option<usize>,
    /// Like `cut_after_chunks`, but the TCP connection dies without the terminating chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_after_chunks: Option<usize>,
    /// Bytes of tool call arguments per streamed fragment for this rule (aimock's `chunkSize`);
    /// overrides the server-wide `--args-chunk-bytes`. At least 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_chunk_bytes: Option<usize>,
}

/// Token counts that replace the tokenizer's for the fields given; the others are still computed.
/// Both providers' spellings are accepted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSpec {
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "input_tokens")]
    pub prompt_tokens: Option<u32>,
    /// Total output, reasoning included.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "output_tokens")]
    pub completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "cache_read_tokens")]
    pub cached_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "cache_creation_input_tokens")]
    pub cache_creation_tokens: Option<u32>,
    /// Anthropic streams: which event carries `input_tokens` (default `start`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens_in: Option<InputTokensIn>,
}

/// Where an Anthropic stream reports `input_tokens`: `message_start` or `message_delta`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTokensIn {
    Start,
    Delta,
}

/// What to answer. Exactly one of `text`/`tool_calls` (together is fine), `error`, `trajectory` or
/// `policy` must be set (`hang` alone is also a reply).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplySpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallSpec>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorSpec>,
    /// Overrides the inferred finish reason (`tool_calls` when there are tool calls, else `stop`,
    /// or `length` when `max_tokens` truncated the text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamSpec>,
    /// Delay before a sync response or an error, server-clock milliseconds. For streams this is
    /// the same as `stream.ttft_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    /// Never answer: hold the request until the client gives up (`Outcome::Hang`). With
    /// `hang_ms`, answer with the rest of this spec after that long on the server clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hang: Option<bool>,
    /// Hold the request this many server-clock milliseconds, then answer. Implies `hang`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hang_ms: Option<u64>,
    /// The thinking `signature` / reasoning `encrypted_content` to send with `reasoning`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_opaque: Option<String>,
    /// Token counts reported instead of the tokenizer's, for the fields given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageSpec>,
    /// Sync responses: headers, half the body, then the connection dies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<bool>,
    /// Answer with the next step of this registered trajectory (`/_fakellm/trajectories`, see
    /// `crate::agent`). Excludes `text`, `tool_calls` and `error`; `stream`, `delay_ms`, `hang*`,
    /// `reset` and `usage` here are defaults for steps that do not set their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trajectory: Option<String>,
    /// Let this registered policy script decide the reply (`/_fakellm/policies`, see
    /// `crate::policy`). Same exclusions and defaults as `trajectory`; a policy may itself return
    /// `trajectory`, never `policy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    /// Custom endpoint rules (`match.path`): the JSON body to answer with, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    /// Custom endpoint rules: HTTP status of the answer, 200 when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Custom endpoint rules: extra response headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
}

impl ReplySpec {
    /// Fills every delivery field this spec leaves unset from `base` (the rule's own spec, when
    /// the content came from a trajectory step or a policy): `stream`, `delay_ms`, `hang`,
    /// `hang_ms`, `reset`, `usage`. Fields set here win; `stream` is replaced as a whole.
    pub fn with_defaults_from(mut self, base: &ReplySpec) -> ReplySpec {
        macro_rules! fill {
            ($($f:ident),*) => { $(if self.$f.is_none() { self.$f = base.$f.clone(); })* };
        }
        fill!(stream, delay_ms, hang, hang_ms, reset, usage);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, rename = "match")]
    pub matcher: Match,
    pub reply: ReplySpec,
    /// How many requests this rule may answer; unlimited when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<u32>,
}

/// A rule as listed by the control plane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuleInfo {
    pub index: usize,
    #[serde(flatten)]
    pub rule: Rule,
    /// Requests answered since the rule was registered.
    pub hits: u64,
    /// Requests that got past every other condition of the rule, whether or not `probability`
    /// or `every_nth` then let them through.
    #[serde(default)]
    pub reached: u64,
    /// `times` is spent.
    pub exhausted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptFormat {
    Json,
    Yaml,
    /// JSON if the text starts with `[` or `{`, YAML otherwise.
    Auto,
}

/// A rule could not be compiled: bad regex or glob, a reply with neither text nor error, ...
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptError(pub String);

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScriptError {}

impl Rule {
    /// Parses a document holding rules: a JSON/YAML array, or an object with a `rules` array.
    pub fn parse_many(text: &str, format: ScriptFormat) -> Result<Vec<Rule>, ScriptError> {
        let json = match format {
            ScriptFormat::Json => true,
            ScriptFormat::Yaml => false,
            ScriptFormat::Auto => text.trim_start().starts_with(['[', '{']),
        };
        let doc: Value = if json {
            serde_json::from_str(text).map_err(|e| ScriptError(format!("invalid JSON: {e}")))?
        } else {
            serde_norway::from_str(text).map_err(|e| ScriptError(format!("invalid YAML: {e}")))?
        };
        let items = match doc {
            Value::Array(a) => a,
            Value::Object(mut o) => match o.remove("rules") {
                Some(Value::Array(a)) => a,
                _ => return Err(ScriptError("expected an array of rules or an object with a `rules` array".into())),
            },
            Value::Null => Vec::new(),
            _ => return Err(ScriptError("expected an array of rules or an object with a `rules` array".into())),
        };
        items
            .into_iter()
            .enumerate()
            .map(|(i, v)| serde_json::from_value::<Rule>(v).map_err(|e| ScriptError(format!("rule {i}: {e}"))))
            .collect()
    }
}

/// A compiled [`Pattern`]: the fast `regex` engine when it can take the pattern, `fancy-regex`
/// (lookahead, lookbehind, backreferences) when only that can.
#[derive(Clone, Debug)]
pub(crate) enum Re {
    Plain(Regex),
    Fancy(Box<fancy_regex::Regex>),
}

/// Why a pattern did not compile.
#[derive(Clone, Debug)]
pub(crate) struct ReError(String);

impl std::fmt::Display for ReError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Re {
    pub(crate) fn new(src: &str) -> Result<Self, ReError> {
        match Regex::new(src) {
            Ok(re) => Ok(Self::Plain(re)),
            Err(plain) => match fancy_regex::Regex::new(src) {
                Ok(re) => Ok(Self::Fancy(Box::new(re))),
                // Report the engine that knows the most syntax.
                Err(fancy) => Err(ReError(if matches!(plain, regex::Error::Syntax(_)) {
                    fancy.to_string()
                } else {
                    plain.to_string()
                })),
            },
        }
    }

    /// A match error at run time (fancy-regex's backtrack limit) counts as no match.
    pub(crate) fn is_match(&self, text: &str) -> bool {
        match self {
            Self::Plain(re) => re.is_match(text),
            Self::Fancy(re) => re.is_match(text).unwrap_or(false),
        }
    }

    /// Every match replaced by `replacement` (literal text, no `$` groups); `None` when the engine
    /// failed at run time (fancy-regex's backtrack limit), so the caller can fail closed.
    pub(crate) fn try_replace_all_literal(&self, text: &str, replacement: &str) -> Option<String> {
        match self {
            Self::Plain(re) => Some(re.replace_all(text, regex::NoExpand(replacement)).into_owned()),
            Self::Fancy(re) => {
                let mut out = String::with_capacity(text.len());
                let mut last = 0;
                for m in re.find_iter(text) {
                    let m = m.ok()?;
                    out.push_str(&text[last..m.start()]);
                    out.push_str(replacement);
                    last = m.end();
                }
                out.push_str(&text[last..]);
                Some(out)
            }
        }
    }
}

impl Pattern {
    pub(crate) fn compile(&self) -> Result<Re, ReError> {
        let s = self.0.as_str();
        let (body, ci) = if s.len() >= 2 && s.starts_with('/') && s.ends_with('/') {
            (&s[1..s.len() - 1], false)
        } else if s.len() >= 3 && s.starts_with('/') && s.ends_with("/i") {
            (&s[1..s.len() - 2], true)
        } else {
            (s, false)
        };
        if ci { Re::new(&format!("(?i){body}")) } else { Re::new(body) }
    }
}

#[derive(Default)]
struct Matchers {
    last_user: Option<Re>,
    last_user_raw: Option<Re>,
    system: Option<Re>,
    any_message: Option<Re>,
    body: Option<Re>,
    tool_result_contains: Option<Re>,
}

impl Matchers {
    fn compile(m: &Match) -> Result<Self, String> {
        let one = |name: &str, p: &Option<Pattern>| -> Result<Option<Re>, String> {
            p.as_ref()
                .map(|p| p.compile().map_err(|e| format!("match.{name}: invalid regex {:?}: {e}", p.0)))
                .transpose()
        };
        Ok(Self {
            last_user: one("last_user", &m.last_user)?,
            last_user_raw: one("last_user_raw", &m.last_user_raw)?,
            system: one("system", &m.system)?,
            any_message: one("any_message", &m.any_message)?,
            body: one("body", &m.body)?,
            tool_result_contains: one("tool_result_contains", &m.tool_result_contains)?,
        })
    }
}

fn validate(rule: &Rule) -> Result<(), String> {
    if let Some(p) = rule.matcher.probability
        && !(0.0..=1.0).contains(&p)
    {
        return Err("match.probability must be between 0 and 1".into());
    }
    if rule.matcher.after_count.is_some() && rule.matcher.after.is_none() {
        return Err("match.after_count needs match.after".into());
    }
    if rule.matcher.after_count == Some(0) {
        return Err("match.after_count must be at least 1".into());
    }
    if rule.matcher.after.is_some() && rule.matcher.after == rule.name {
        return Err("match.after names the rule itself, which could never start".into());
    }
    if rule.matcher.every_nth == Some(0) {
        return Err("match.every_nth must be at least 1".into());
    }
    let custom = rule.matcher.path.is_some();
    let r = &rule.reply;
    if custom != r.body.is_some() {
        return Err("`match.path` and `reply.body` go together".into());
    }
    if !custom && (r.status.is_some() || r.headers.is_some()) {
        return Err("`reply.status` and `reply.headers` need `match.path`".into());
    }
    if custom {
        if r.text.is_some()
            || r.tool_calls.is_some()
            || r.error.is_some()
            || r.trajectory.is_some()
            || r.policy.is_some()
            || r.reasoning.is_some()
        {
            return Err("a custom endpoint reply has only `body`, `status` and `headers`".into());
        }
        if let Some(s) = r.status
            && !(100..=599).contains(&s)
        {
            return Err(format!("reply.status {s} is not an HTTP status"));
        }
        for (k, v) in r.headers.iter().flatten() {
            if http::HeaderName::from_bytes(k.as_bytes()).is_err() || http::HeaderValue::from_str(v).is_err() {
                return Err(format!("reply.headers: invalid header {k:?}"));
            }
        }
        return Ok(());
    }
    validate_reply(&rule.reply)
}

/// The checks every reply spec must pass, whether it sits in a rule, a trajectory step or came
/// back from a policy. Steps and policy replies additionally must not delegate again, see
/// [`validate_leaf_reply`].
pub(crate) fn validate_reply(r: &ReplySpec) -> Result<(), String> {
    let has_content = r.text.is_some() || r.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
    let delegated = r.trajectory.is_some() || r.policy.is_some();
    let hangs = r.hang == Some(true) || r.hang_ms.is_some();
    if r.hang_ms.is_some() && (r.delay_ms.is_some() || r.stream.as_ref().is_some_and(|s| s.ttft_ms.is_some())) {
        return Err(
            "reply `hang_ms` and `delay_ms` / `stream.ttft_ms` do not combine: `hang_ms` sends no response headers for that long, \
             `delay_ms` / `ttft_ms` delay the first byte after the headers; use one of them"
                .into(),
        );
    }
    if r.trajectory.is_some() && r.policy.is_some() {
        return Err("reply `trajectory` and `policy` exclude each other".into());
    }
    if delegated && (has_content || r.error.is_some() || r.reasoning.is_some() || r.finish_reason.is_some()) {
        return Err("reply `trajectory` / `policy` exclude `text`, `tool_calls`, `reasoning`, `finish_reason` and `error`".into());
    }
    match (&r.error, has_content) {
        (None, false) if !hangs && !delegated => {
            return Err("reply needs `text`, `tool_calls`, `error`, `trajectory` or `policy`".into());
        }
        (Some(_), true) => return Err("reply `error` excludes `text` and `tool_calls`".into()),
        _ => {}
    }
    if let Some(tps) = r.stream.as_ref().and_then(|s| s.tokens_per_sec)
        && !(tps > 0.0 && tps.is_finite() && Duration::try_from_secs_f64(1.0 / tps).is_ok())
    {
        return Err("reply.stream.tokens_per_sec must be greater than 0 and not vanishingly small".into());
    }
    if r.stream.as_ref().and_then(|s| s.args_chunk_bytes) == Some(0) {
        return Err("reply.stream.args_chunk_bytes must be at least 1".into());
    }
    if let Some(e) = &r.error
        && !(100..=599).contains(&e.status)
    {
        return Err(format!("reply.error.status {} is not an HTTP status", e.status));
    }
    if let Some(ra) = r.error.as_ref().and_then(|e| e.retry_after)
        && Duration::try_from_secs_f64(ra).is_err()
    {
        return Err("reply.error.retry_after must be a non-negative number".into());
    }
    Ok(())
}

/// [`validate_reply`] plus: no `trajectory` or `policy` (trajectory steps and policy replies are
/// leaves; a policy may name a trajectory, which the engine handles before it validates).
pub(crate) fn validate_leaf_reply(r: &ReplySpec) -> Result<(), String> {
    if r.trajectory.is_some() || r.policy.is_some() {
        return Err("`trajectory` and `policy` are not allowed here".into());
    }
    validate_reply(r)
}

/// A rule and its counters, as a snapshot keeps them.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptState {
    pub rule: Rule,
    pub hits: u64,
    pub reached: u64,
}

/// The registered rules with their compiled matchers and hit counters.
#[derive(Default)]
pub struct Scripts {
    inner: Mutex<Vec<Compiled>>,
}

struct Compiled {
    rule: Rule,
    hits: u64,
    reached: u64,
    re: Matchers,
}

impl Compiled {
    fn new(rule: Rule, index: usize) -> Result<Self, ScriptError> {
        validate(&rule).map_err(|e| ScriptError(format!("rule {index}: {e}")))?;
        let re = Matchers::compile(&rule.matcher).map_err(|e| ScriptError(format!("rule {index}: {e}")))?;
        Ok(Self {
            rule,
            hits: 0,
            reached: 0,
            re,
        })
    }

    fn exhausted(&self) -> bool {
        self.rule.times.is_some_and(|t| self.hits >= u64::from(t))
    }

    /// Whether `req` matches; counts the request as reached when every plain condition holds.
    fn matches(&mut self, req: &Request) -> bool {
        if !self.conditions_hold(req) {
            return false;
        }
        self.reached += 1;
        let m = &self.rule.matcher;
        if let Some(n) = m.every_nth
            && !self.reached.is_multiple_of(u64::from(n))
        {
            return false;
        }
        if let Some(p) = m.probability
            && draw(fallback::request_seed(req), self.reached) >= p
        {
            return false;
        }
        true
    }

    fn conditions_hold(&self, req: &Request) -> bool {
        let m = &self.rule.matcher;
        match (&m.path, &req.path) {
            (Some(g), Some(p)) if glob_match(g, p) => {}
            (None, None) => {}
            _ => return false,
        }
        if let Some(p) = m.protocol
            && p != req.protocol
        {
            return false;
        }
        if let Some(g) = &m.model
            && !glob_match(g, &req.model)
        {
            return false;
        }
        if let Some(re) = &self.re.last_user
            && !req.last_user_text().is_some_and(|t| re.is_match(&t))
        {
            return false;
        }
        if let Some(re) = &self.re.last_user_raw
            && !req.last_user_text_raw().is_some_and(|t| re.is_match(&t))
        {
            return false;
        }
        if let Some(re) = &self.re.system
            && !re.is_match(&req.system_text())
        {
            return false;
        }
        if let Some(re) = &self.re.any_message
            && !req.messages.iter().any(|msg| re.is_match(&msg.text_content()))
        {
            return false;
        }
        if let Some(re) = &self.re.body {
            let hit = match &req.raw_body {
                Some(raw) => re.is_match(raw),
                None => re.is_match(&serde_json::to_string(req).unwrap_or_default()),
            };
            if !hit {
                return false;
            }
        }
        if let Some(name) = &m.has_tool
            && !req.has_tool(name)
        {
            return false;
        }
        if let Some(no) = m.no_tools
            && no != req.tools.is_empty()
        {
            return false;
        }
        if let Some(b) = m.has_tool_result
            && b != req.ends_with_tool_result()
        {
            return false;
        }
        if let Some(name) = &m.last_tool_result
            && !req.trailing_tool_results().iter().any(|r| r.name == Some(name.as_str()))
        {
            return false;
        }
        if let Some(re) = &self.re.tool_result_contains
            && !req.trailing_tool_results().iter().any(|r| re.is_match(&r.content))
        {
            return false;
        }
        if let Some(id) = &m.last_tool_call_id
            && !req.trailing_tool_results().iter().any(|r| r.tool_call_id == Some(id.as_str()))
        {
            return false;
        }
        if let Some(b) = m.stream
            && b != req.stream
        {
            return false;
        }
        if let Some(n) = m.turn
            && n != req.assistant_turns()
        {
            return false;
        }
        true
    }
}

/// A uniform number in `[0, 1)` from the request seed and how many requests reached the rule
/// before, so a run replays the same decisions. The counter is mixed in because an identical
/// retry has the same seed and would otherwise draw the same number forever.
fn draw(seed: u64, reached: u64) -> f64 {
    let mut z = (seed ^ reached.wrapping_mul(0x9e37_79b9_7f4a_7c15)).wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64
}

/// Every `match.after` must name a rule of the set, and the names must not form a cycle (none of the
/// rules in it could ever start).
fn check_after<'a>(rules: impl Iterator<Item = &'a Rule>) -> Result<(), ScriptError> {
    use std::collections::{HashMap, HashSet};
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut gated = Vec::new();
    for r in rules {
        if let Some(n) = &r.name {
            edges.entry(n).or_default();
        }
        if let Some(a) = &r.matcher.after {
            gated.push((r.name.as_deref(), a.as_str()));
        }
    }
    for (name, after) in &gated {
        if !edges.contains_key(after) {
            return Err(ScriptError(format!("match.after names no rule: {after:?}")));
        }
        if let Some(n) = name {
            edges.entry(n).or_default().push(after);
        }
    }
    // Depth-first search for a cycle among named rules.
    fn visit<'a>(
        n: &'a str,
        edges: &HashMap<&'a str, Vec<&'a str>>,
        done: &mut HashSet<&'a str>,
        path: &mut Vec<&'a str>,
    ) -> Option<&'a str> {
        if path.contains(&n) {
            return Some(n);
        }
        if !done.insert(n) {
            return None;
        }
        path.push(n);
        for next in edges.get(n).into_iter().flatten() {
            if let Some(c) = visit(next, edges, done, path) {
                return Some(c);
            }
        }
        path.pop();
        None
    }
    let mut done = HashSet::new();
    let names: Vec<&str> = edges.keys().copied().collect();
    for n in names {
        if let Some(c) = visit(n, &edges, &mut done, &mut Vec::new()) {
            return Err(ScriptError(format!(
                "match.after forms a cycle through rule {c:?}, none of them could ever start"
            )));
        }
    }
    Ok(())
}

fn compile_all(rules: Vec<Rule>, offset: usize) -> Result<Vec<Compiled>, ScriptError> {
    rules.into_iter().enumerate().map(|(i, r)| Compiled::new(r, offset + i)).collect()
}

impl Scripts {
    /// Replaces all rules. Fails without changing anything if any rule is invalid.
    pub fn replace(&self, rules: Vec<Rule>) -> Result<(), ScriptError> {
        check_after(rules.iter())?;
        let compiled = compile_all(rules, 0)?;
        *self.inner.lock() = compiled;
        Ok(())
    }

    /// Like [`Scripts::replace`], but a rule keeps the counters of the old rule with the same
    /// name (`PUT /_fakellm/scripts?keep_hits=true`); an unnamed rule keeps those of the old
    /// unnamed rule at the same index. Others start at zero.
    pub fn replace_keep_hits(&self, rules: Vec<Rule>) -> Result<(), ScriptError> {
        check_after(rules.iter())?;
        let mut compiled = compile_all(rules, 0)?;
        let mut inner = self.inner.lock();
        for (i, new) in compiled.iter_mut().enumerate() {
            let old = match &new.rule.name {
                Some(n) => inner.iter().find(|o| o.rule.name.as_ref() == Some(n)),
                None => inner.get(i).filter(|o| o.rule.name.is_none()),
            };
            if let Some(old) = old {
                new.hits = old.hits;
                new.reached = old.reached;
            }
        }
        *inner = compiled;
        Ok(())
    }

    /// Appends rules after the existing ones.
    pub fn append(&self, rules: Vec<Rule>) -> Result<(), ScriptError> {
        let mut inner = self.inner.lock();
        check_after(inner.iter().map(|c| &c.rule).chain(rules.iter()))?;
        let compiled = compile_all(rules, inner.len())?;
        inner.extend(compiled);
        Ok(())
    }

    pub fn list(&self) -> Vec<RuleInfo> {
        self.inner
            .lock()
            .iter()
            .enumerate()
            .map(|(index, c)| RuleInfo {
                index,
                rule: c.rule.clone(),
                hits: c.hits,
                reached: c.reached,
                exhausted: c.exhausted(),
            })
            .collect()
    }

    pub fn clear(&self) {
        self.inner.lock().clear();
    }

    /// For snapshots: every rule with its counters.
    pub fn export_state(&self) -> Vec<ScriptState> {
        self.inner
            .lock()
            .iter()
            .map(|c| ScriptState {
                rule: c.rule.clone(),
                hits: c.hits,
                reached: c.reached,
            })
            .collect()
    }

    /// For snapshots: replaces all rules and counters with `state`. Fails without changing anything if a rule is invalid.
    pub fn import_state(&self, state: Vec<ScriptState>) -> Result<(), ScriptError> {
        check_after(state.iter().map(|s| &s.rule))?;
        let mut compiled = Vec::with_capacity(state.len());
        for (i, s) in state.into_iter().enumerate() {
            let mut c = Compiled::new(s.rule, i)?;
            c.hits = s.hits;
            c.reached = s.reached;
            compiled.push(c);
        }
        *self.inner.lock() = compiled;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The first unexhausted rule matching `req`, counting the hit. Returns the rule's index and a
    /// clone of its reply spec.
    pub fn resolve(&self, req: &Request) -> Option<(usize, Option<String>, ReplySpec)> {
        self.resolve_from(req, 0)
    }

    /// [`Scripts::resolve`] looking only at rules from index `start` on (a policy declined the
    /// rules before).
    pub fn resolve_from(&self, req: &Request, start: usize) -> Option<(usize, Option<String>, ReplySpec)> {
        self.resolve_where_from(req, start, |_| true)
    }

    /// Takes back the hit of rule `index`, which a policy declined.
    pub(crate) fn uncount_hit(&self, index: usize) {
        if let Some(c) = self.inner.lock().get_mut(index) {
            c.hits = c.hits.saturating_sub(1);
        }
    }

    /// Like [`Scripts::resolve`], but rules whose reply fails `accept` are skipped entirely: they
    /// are not reached and keep their hit budget.
    pub fn resolve_where(&self, req: &Request, accept: impl Fn(&ReplySpec) -> bool) -> Option<(usize, Option<String>, ReplySpec)> {
        self.resolve_where_from(req, 0, accept)
    }

    /// Like [`Scripts::resolve_where`], but only looks: no hit and no reached counter changes, so a
    /// `times` / `every_nth` / `probability` budget stays for the request meant to use it.
    pub fn peek_where(&self, req: &Request, accept: impl Fn(&ReplySpec) -> bool) -> Option<(usize, Option<String>, ReplySpec)> {
        self.resolve_inner(req, 0, accept, false)
    }

    fn resolve_where_from(
        &self,
        req: &Request,
        start: usize,
        accept: impl Fn(&ReplySpec) -> bool,
    ) -> Option<(usize, Option<String>, ReplySpec)> {
        self.resolve_inner(req, start, accept, true)
    }

    fn resolve_inner(
        &self,
        req: &Request,
        start: usize,
        accept: impl Fn(&ReplySpec) -> bool,
        count: bool,
    ) -> Option<(usize, Option<String>, ReplySpec)> {
        let mut inner = self.inner.lock();
        // Hits per rule name, only when some rule is gated on another (`match.after`).
        let named_hits: std::collections::HashMap<String, u64> = if inner.iter().any(|c| c.rule.matcher.after.is_some()) {
            let mut map = std::collections::HashMap::new();
            for c in inner.iter() {
                if let Some(n) = &c.rule.name {
                    *map.entry(n.clone()).or_insert(0) += c.hits;
                }
            }
            map
        } else {
            Default::default()
        };
        let mut found = None;
        for (index, c) in inner.iter_mut().enumerate().skip(start) {
            if let Some(after) = &c.rule.matcher.after
                && named_hits.get(after).copied().unwrap_or(0) < u64::from(c.rule.matcher.after_count.unwrap_or(1))
            {
                continue;
            }
            if c.exhausted() || !accept(&c.rule.reply) {
                continue;
            }
            let reached = c.reached;
            let hit = c.matches(req);
            if !count {
                c.reached = reached;
            }
            if hit {
                found = Some((index, c));
                break;
            }
        }
        let (index, c) = found?;
        if count {
            c.hits += 1;
        }
        Some((index, c.rule.name.clone(), c.rule.reply.clone()))
    }
}

/// Decides every request: scripts, then fallback or strict error. Owned by the control plane.
/// A matching rule may delegate its reply to a trajectory step or a policy script, see
/// [`Engine::resolve_async`].
pub struct Engine {
    pub scripts: Scripts,
    pub trajectories: Trajectories,
    pub policies: Policies,
    /// Consulted for requests no rule matches, before the upstream, strict mode and the default reply.
    pub cassettes: Cassettes,
    pub fallback: FallbackConfig,
    pub strict: bool,
    /// Delivery used when the matching rule has no `stream`/`delay_ms`.
    pub default_delivery: Delivery,
}

impl Engine {
    pub fn new(fallback: FallbackConfig, strict: bool, default_delivery: Delivery) -> Self {
        Self {
            scripts: Scripts::default(),
            trajectories: Trajectories::default(),
            policies: Policies::default(),
            cassettes: Cassettes::default(),
            fallback,
            strict,
            default_delivery,
        }
    }

    /// Uses `policies` instead of the default (no runtime override, 5 s timeout).
    pub fn with_policies(mut self, policies: Policies) -> Self {
        self.policies = policies;
        self
    }

    /// Produces the outcome for a parsed request: the matching rule's reply (text truncated to
    /// `max_tokens`, tool call ids filled in, `usage` computed with the tokenizer, delivery from
    /// the rule's `stream`/`delay_ms` over `default_delivery`), or the fallback, or the strict
    /// error. Pure apart from the hit counters.
    ///
    /// A rule with `trajectory` answers with the trajectory's step for this request (see
    /// [`Trajectories::step`]). A rule with `policy` cannot be decided synchronously; this entry
    /// point answers it with a `fakellm_policy_error`. The server uses [`Engine::resolve_async`].
    pub fn resolve(&self, req: &Request) -> Outcome {
        let Some((index, name, spec)) = self.scripts.resolve(req) else {
            return self.unscripted(req);
        };
        if let Some(p) = &spec.policy {
            let source = Source::Policy {
                index,
                name,
                policy: p.clone(),
            };
            let error = ApiError::policy_error(p, "policy rules need Engine::resolve_async");
            return self.error_outcome(error, source);
        }
        self.resolve_matched(req, index, name, spec, None)
    }

    /// [`Engine::resolve`] for the server: a rule with `policy` is decided by that policy
    /// ([`Policies::decide`], awaited; the reply it returns is then treated like the rule's own,
    /// with the rule's delivery fields as defaults).
    pub async fn resolve_async(&self, req: &Request) -> Outcome {
        let mut start = 0;
        loop {
            let Some((index, name, mut spec)) = self.scripts.resolve_from(req, start) else {
                return self.unscripted(req);
            };
            let Some(policy) = spec.policy.take() else {
                return self.resolve_matched(req, index, name, spec, None);
            };
            match self.policies.decide(&policy, req).await {
                Ok(Some(decided)) => {
                    let decided = decided.with_defaults_from(&spec);
                    return self.resolve_matched(req, index, name, decided, Some(policy));
                }
                // Declined: the rule did not match after all, so it takes no hit and the next rule gets a turn.
                Ok(None) => {
                    self.scripts.uncount_hit(index);
                    start = index + 1;
                }
                Err(error) => return self.error_outcome(error, Source::Policy { index, name, policy }),
            }
        }
    }

    /// For a request on a path the standard routes do not serve: the first rule with `match.path`
    /// and `reply.body` that matches, counting its hit.
    pub fn resolve_custom(&self, req: &Request) -> Option<(usize, Option<String>, ReplySpec)> {
        self.scripts.resolve_where(req, |r| r.body.is_some())
    }

    /// For endpoints that generate nothing scriptable (`POST /v1/embeddings`): the first rule that
    /// matches and answers with `error`, as an `Outcome::Error`. Rules without `error` are skipped
    /// entirely, like in [`Engine::count_input_tokens`], so they keep their hit budget.
    pub fn resolve_error(&self, req: &Request) -> Option<Outcome> {
        let (index, name, spec) = self.scripts.resolve_where(req, |r| r.error.is_some())?;
        Some(self.resolve_matched(req, index, name, spec, None))
    }

    fn error_outcome(&self, error: ApiError, source: Source) -> Outcome {
        Outcome::Error {
            error,
            delivery: self.default_delivery.clone(),
            source,
        }
    }

    /// Everything after a rule (and its policy, if any) produced `spec`: trajectory delegation,
    /// delivery, and rendering the spec into an outcome. `policy` names the policy that decided
    /// `spec`, for the log.
    fn resolve_matched(&self, req: &Request, index: usize, name: Option<String>, mut spec: ReplySpec, policy: Option<String>) -> Outcome {
        let mut source = match policy {
            Some(policy) => Source::Policy {
                index,
                name: name.clone(),
                policy,
            },
            None => Source::Script { index, name: name.clone() },
        };
        if let Some(trajectory) = spec.trajectory.take() {
            match self.trajectories.step(&trajectory, req) {
                Ok(stepped) => {
                    source = Source::Trajectory {
                        index,
                        name,
                        trajectory,
                        step: stepped.step,
                    };
                    spec = stepped.reply.with_defaults_from(&spec);
                }
                Err((error, step)) => {
                    return self.error_outcome(
                        error,
                        Source::Trajectory {
                            index,
                            name,
                            trajectory,
                            step,
                        },
                    );
                }
            }
        }
        let mut delivery = self.default_delivery.clone();
        if let Some(s) = &spec.stream {
            if let Some(ms) = s.ttft_ms.or(spec.delay_ms) {
                delivery.ttft = Duration::from_millis(ms);
            }
            if s.tokens_per_sec.is_some() {
                delivery.token_interval = interval_from_tps(s.tokens_per_sec);
            }
            if let Some(n) = s.chunk_tokens {
                delivery.chunk_tokens = n.max(1);
            }
            if s.cut_after_chunks.is_some() {
                delivery.cut_after_chunks = s.cut_after_chunks;
            }
            if s.reset_after_chunks.is_some() {
                delivery.reset_after_chunks = s.reset_after_chunks;
            }
            if let Some(n) = s.args_chunk_bytes {
                delivery.args_chunk_bytes = n.max(1);
            }
        } else if let Some(ms) = spec.delay_ms {
            delivery.ttft = Duration::from_millis(ms);
        }

        let hang = match (spec.hang, spec.hang_ms) {
            (_, Some(ms)) => Some(Some(Duration::from_millis(ms))),
            (Some(true), None) => Some(None),
            _ => None,
        };
        delivery.reset = spec.reset.unwrap_or(false);
        let outcome = Self::answer(req, spec, delivery, source);
        match hang {
            Some(until) => Outcome::Hang {
                until,
                then: Box::new(outcome),
            },
            None => outcome,
        }
    }

    /// The `input_tokens` of `POST /v1/messages/count_tokens`: the first rule that matches and
    /// sets `usage.prompt_tokens` (without counting a hit or a reach), else the tokenizer's count.
    /// Rules without that field are skipped and the chosen rule keeps its budget, so a count call
    /// never eats what the real request was meant to use.
    ///
    /// Real Anthropic semantics: `usage.input_tokens` of a message excludes cache reads and writes
    /// (`total = input + cache_read + cache_creation`) while `count_tokens` counts the whole input.
    /// Here a rule's `input_tokens` is literal in both places (see `Engine::answer`); without a rule
    /// `count_tokens` is the tokenizer total and a message derives `input_tokens` from the same total.
    pub fn count_input_tokens(&self, req: &Request) -> u32 {
        self.scripts
            .peek_where(req, |r| r.usage.as_ref().is_some_and(|u| u.prompt_tokens.is_some()))
            .and_then(|(_, _, spec)| spec.usage?.prompt_tokens)
            .unwrap_or_else(|| tokenizer::count_request(req))
    }

    fn answer(req: &Request, spec: ReplySpec, mut delivery: Delivery, source: Source) -> Outcome {
        if let Some(e) = spec.error {
            let mut error = ApiError::for_status_in(req.protocol, e.status);
            if e.status == 404 {
                error = match req.protocol {
                    Protocol::AnthropicMessages => error.with_message(format!("model: {}", req.model)),
                    _ => ApiError::model_not_found(&req.model),
                };
            }
            if let Some(k) = e.kind {
                error.kind = k;
            }
            if e.code.is_some() {
                error.code = e.code;
            }
            if let Some(m) = e.message {
                error.message = m;
            }
            if e.param.is_some() {
                error.param = e.param;
            }
            if let Some(s) = e.retry_after {
                error.retry_after = Some(Duration::try_from_secs_f64(s).unwrap_or(Duration::MAX));
            }
            error.in_stream = e.in_stream.unwrap_or(false);
            for (name, value) in e.headers.into_iter().flatten() {
                error.headers.push((name.to_ascii_lowercase(), value));
            }
            if let Some(ms) = spec.delay_ms {
                delivery.ttft = Duration::from_millis(ms);
            }
            return Outcome::Error { error, delivery, source };
        }

        let text = spec.text.or_else(|| spec.tool_calls.is_none().then(String::new));
        let calls = spec.tool_calls.unwrap_or_default();
        let seed = if calls.iter().any(|c| c.id.is_none()) {
            fallback::request_seed(req)
        } else {
            0
        };
        let tool_calls: Vec<ToolCall> = calls
            .into_iter()
            .enumerate()
            .map(|(i, c)| ToolCall {
                id: c.id.unwrap_or_else(|| fallback::tool_call_id(req.protocol, seed ^ i as u64)),
                name: c.name,
                arguments: match c.arguments {
                    Value::String(s) => s,
                    Value::Null => "{}".to_owned(),
                    v => serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_owned()),
                },
            })
            .collect();
        // `max_tokens` bounds only the scripted text; reasoning and tool-call arguments are emitted as written.
        let mut truncated = false;
        let text = text.map(|t| match req.max_tokens {
            Some(max) => {
                let (t, cut) = tokenizer::truncate(&t, max);
                truncated = cut;
                t
            }
            None => t,
        });
        let finish = spec.finish_reason.unwrap_or(if truncated {
            FinishReason::Length
        } else if !tool_calls.is_empty() {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        });
        let over = spec.usage.unwrap_or_default();
        delivery.input_tokens_in_delta = over.input_tokens_in == Some(InputTokensIn::Delta);
        let reasoning_tokens = over
            .reasoning_tokens
            .unwrap_or_else(|| spec.reasoning.as_deref().map_or(0, tokenizer::count));
        let completion_tokens = over.completion_tokens.unwrap_or_else(|| {
            text.as_deref().map_or(0, tokenizer::count)
                + tool_calls.iter().map(|c| tokenizer::count(&c.arguments)).sum::<u32>()
                + reasoning_tokens
        });
        let cached_tokens = over.cached_tokens.unwrap_or(0);
        let cache_creation_tokens = over.cache_creation_tokens.unwrap_or(0);
        // Anthropic reports `input_tokens` without cache reads and writes, so a literal override is
        // stored as the total the frontend subtracts from again.
        let prompt_tokens = match (over.prompt_tokens, req.protocol) {
            (Some(n), Protocol::AnthropicMessages) => n.saturating_add(cached_tokens).saturating_add(cache_creation_tokens),
            (Some(n), _) => n,
            (None, _) => tokenizer::count_request(req),
        };
        let reply = Reply {
            text,
            tool_calls,
            reasoning: spec.reasoning,
            reasoning_opaque: spec.reasoning_opaque,
            refusal: None,
            finish,
            usage: Usage {
                prompt_tokens,
                completion_tokens,
                reasoning_tokens,
                cached_tokens,
                cache_creation_tokens,
            },
        };
        Outcome::Reply { reply, delivery, source }
    }

    /// The recorded exchange for `req`, if a cassette has one: its outcome with the engine's default delivery and
    /// `Source::Cassette`. A recorded reply without token counts (imported conversations) gets them from the tokenizer.
    fn replayed(&self, req: &Request) -> Option<Outcome> {
        let hit = self.cassettes.lookup(req)?;
        let source = Source::Cassette {
            name: hit.cassette,
            index: hit.index,
        };
        let mut outcome = hit.outcome.into_outcome(self.default_delivery.clone(), source);
        if let Outcome::Reply { reply, .. } = &mut outcome
            && reply.usage == Usage::default()
        {
            reply.usage = Self::count_usage(req, reply);
        }
        Some(outcome)
    }

    fn count_usage(req: &Request, reply: &Reply) -> Usage {
        Usage {
            prompt_tokens: tokenizer::count_request(req),
            completion_tokens: reply.text.as_deref().map_or(0, tokenizer::count)
                + reply.tool_calls.iter().map(|c| tokenizer::count(&c.arguments)).sum::<u32>()
                + reply.reasoning.as_deref().map_or(0, tokenizer::count),
            ..Usage::default()
        }
    }

    fn unscripted(&self, req: &Request) -> Outcome {
        if let Some(outcome) = self.replayed(req) {
            return outcome;
        }
        if self.strict {
            return Outcome::Error {
                error: ApiError::no_matching_rule(&req.model),
                delivery: self.default_delivery.clone(),
                source: Source::Strict,
            };
        }
        let mut reply = fallback::reply(req, &self.fallback);
        reply.usage = Self::count_usage(req, &reply);
        Outcome::Reply {
            reply,
            delivery: self.default_delivery.clone(),
            source: Source::Fallback,
        }
    }
}

/// `tokens_per_sec` to a per-token interval; `None` or `<= 0` means no gap.
pub fn interval_from_tps(tps: Option<f64>) -> Duration {
    match tps {
        Some(t) if t > 0.0 => Duration::try_from_secs_f64(1.0 / t).unwrap_or(Duration::MAX),
        _ => Duration::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Message, Role, ToolDef};
    use serde_json::json;

    fn user(text: &str) -> Request {
        Request::new("gpt-4o", vec![Message::text(Role::User, text)])
    }

    fn text_rule(m: Match, text: &str, times: Option<u32>) -> Rule {
        Rule {
            name: None,
            matcher: m,
            reply: ReplySpec {
                text: Some(text.into()),
                ..Default::default()
            },
            times,
        }
    }

    fn pat(s: &str) -> Option<Pattern> {
        Some(Pattern(s.into()))
    }

    #[test]
    fn parses_design_yaml() {
        let yaml = r#"
- match:
    model: gpt-*
    last_user: /天气/
    has_tool: get_weather
  reply:
    tool_calls:
      - name: get_weather
        arguments: { city: "上海" }
- match:
    last_tool_result: get_weather
  reply:
    text: "上海今天晴，22 度。"
    stream: { ttft_ms: 300, tokens_per_sec: 40 }
- match: { any: true }
  reply:
    error: { status: 429, retry_after: 2 }
  times: 1
"#;
        let rules = Rule::parse_many(yaml, ScriptFormat::Auto).unwrap();
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].matcher.has_tool.as_deref(), Some("get_weather"));
        assert_eq!(rules[1].reply.stream.as_ref().unwrap().ttft_ms, Some(300));
        assert_eq!(rules[2].times, Some(1));
        assert_eq!(rules[2].reply.error.as_ref().unwrap().retry_after, Some(2.0));
        Scripts::default().replace(rules).unwrap();
    }

    #[test]
    fn parses_json_forms_and_reports_index() {
        let arr = r#"[{"reply":{"text":"a"}}]"#;
        assert_eq!(Rule::parse_many(arr, ScriptFormat::Json).unwrap().len(), 1);
        let obj = r#"  {"rules":[{"reply":{"text":"a"}},{"reply":{"text":"b"}}]}"#;
        assert_eq!(Rule::parse_many(obj, ScriptFormat::Auto).unwrap().len(), 2);
        let bad = r#"[{"reply":{"text":"a"}},{"reply":{"txt":"b"}}]"#;
        let err = Rule::parse_many(bad, ScriptFormat::Json).unwrap_err();
        assert!(err.0.contains("rule 1"), "{err}");
        assert!(Rule::parse_many("{\"foo\":1}", ScriptFormat::Auto).is_err());
    }

    #[test]
    fn validation_errors() {
        let s = Scripts::default();
        let empty = Rule {
            name: None,
            matcher: Match::default(),
            reply: ReplySpec::default(),
            times: None,
        };
        assert!(s.replace(vec![empty]).is_err());

        let mut both = text_rule(Match::default(), "x", None);
        both.reply.error = Some(ErrorSpec {
            status: 500,
            ..Default::default()
        });
        assert!(s.replace(vec![both]).is_err());

        let mut tps = text_rule(Match::default(), "x", None);
        tps.reply.stream = Some(StreamSpec {
            tokens_per_sec: Some(0.0),
            ..Default::default()
        });
        assert!(s.replace(vec![tps]).is_err());

        let bad_re = text_rule(
            Match {
                last_user: pat("("),
                ..Default::default()
            },
            "x",
            None,
        );
        let err = s.append(vec![text_rule(Match::default(), "ok", None), bad_re]).unwrap_err();
        assert!(err.0.contains("rule 1"));
        assert!(s.is_empty());
    }

    #[test]
    fn pattern_forms() {
        assert!(Pattern("/ab+c/".into()).compile().unwrap().is_match("xabbc"));
        let ci = Pattern("/HELLO/i".into()).compile().unwrap();
        assert!(ci.is_match("say hello"));
        assert!(!Pattern("/HELLO/".into()).compile().unwrap().is_match("hello"));
        assert!(Pattern("a/b".into()).compile().unwrap().is_match("a/b"));
        // Lookaround and backreferences need the fancy engine; plain patterns stay on `regex`.
        assert!(matches!(Pattern("/ab+c/".into()).compile().unwrap(), Re::Plain(_)));
        let look = Pattern("/foo(?!bar)/".into()).compile().unwrap();
        assert!(matches!(look, Re::Fancy(_)));
        assert!(look.is_match("foobaz") && !look.is_match("foobar"));
        let behind = Pattern(r"/(?<=\$)\d+/".into()).compile().unwrap();
        assert!(behind.is_match("cost $42") && !behind.is_match("cost 42"));
        assert_eq!(look.try_replace_all_literal("foobar foobaz", "X").as_deref(), Some("foobar Xbaz"));
        assert!(Pattern("/(unclosed/".into()).compile().is_err());
    }

    #[test]
    fn order_and_times() {
        let s = Scripts::default();
        s.replace(vec![
            text_rule(
                Match {
                    last_user: pat("/hi/i"),
                    ..Default::default()
                },
                "first",
                Some(2),
            ),
            text_rule(Match::default(), "second", None),
        ])
        .unwrap();
        let req = user("Hi there");
        assert_eq!(s.resolve(&req).unwrap().0, 0);
        assert_eq!(s.resolve(&req).unwrap().0, 0);
        assert_eq!(s.resolve(&req).unwrap().0, 1);
        assert_eq!(s.resolve(&user("other")).unwrap().0, 1);
        let list = s.list();
        assert_eq!((list[0].hits, list[0].exhausted), (2, true));
        assert_eq!((list[1].hits, list[1].exhausted), (2, false));
        s.clear();
        assert!(s.resolve(&req).is_none());
    }

    #[test]
    fn field_matching() {
        let s = Scripts::default();
        let m = |m: Match| {
            s.replace(vec![text_rule(m, "x", None)]).unwrap();
            s.resolve(&{
                let mut r = Request::new(
                    "gpt-4o",
                    vec![Message::text(Role::System, "be brief"), Message::text(Role::User, "hello")],
                );
                r.tools = vec![ToolDef {
                    name: "bash".into(),
                    description: None,
                    parameters: None,
                    strict: None,
                }];
                r
            })
            .is_some()
        };
        assert!(m(Match {
            system: pat("brief"),
            has_tool: Some("bash".into()),
            no_tools: Some(false),
            has_tool_result: Some(false),
            stream: Some(false),
            turn: Some(0),
            any_message: pat("/HELLO/i"),
            any: Some(true),
            ..Default::default()
        }));
        assert!(!m(Match {
            has_tool: Some("zzz".into()),
            ..Default::default()
        }));
        assert!(!m(Match {
            no_tools: Some(true),
            ..Default::default()
        }));
        assert!(!m(Match {
            turn: Some(1),
            ..Default::default()
        }));
        assert!(!m(Match {
            last_tool_result: Some("bash".into()),
            ..Default::default()
        }));
    }

    fn tool_followup(content: &str) -> Request {
        let mut asst = Message::text(Role::Assistant, "");
        asst.content.clear();
        asst.tool_calls = vec![ToolCall {
            id: "c1".into(),
            name: "get_weather".into(),
            arguments: "{}".into(),
        }];
        let mut res = Message::text(Role::Tool, content);
        res.tool_call_id = Some("c1".into());
        Request::new("gpt-4o", vec![Message::text(Role::User, "weather?"), asst, res])
    }

    #[test]
    fn stateful_tool_result_matching() {
        let s = Scripts::default();
        let rules = Rule::parse_many(
            r#"[
              {"match":{"has_tool_result":false},"reply":{"tool_calls":[{"name":"get_weather","arguments":{"city":"SH"},"id":"call_x"}]}},
              {"match":{"last_tool_result":"get_weather","tool_result_contains":"/sunny/i"},"reply":{"text":"It is sunny."}}
            ]"#,
            ScriptFormat::Json,
        )
        .unwrap();
        s.replace(rules).unwrap();
        assert_eq!(s.resolve(&user("weather?")).unwrap().0, 0);
        assert_eq!(s.resolve(&tool_followup("Sunny, 22C")).unwrap().0, 1);
        assert!(s.resolve(&tool_followup("rain")).is_none());
    }

    fn engine(strict: bool) -> Engine {
        Engine::new(FallbackConfig { tokens: 10 }, strict, Delivery::default())
    }

    #[test]
    fn engine_tool_call_reply() {
        let e = engine(false);
        let rules = Rule::parse_many(
            r#"[{"reply":{"tool_calls":[{"name":"f","arguments":{"a":1},"id":"c0"},{"name":"g","arguments":"{bad","id":"c1"}],
                 "stream":{"ttft_ms":300,"tokens_per_sec":40,"chunk_tokens":3,"cut_after_chunks":2}}}]"#,
            ScriptFormat::Json,
        )
        .unwrap();
        e.scripts.replace(rules).unwrap();
        let Outcome::Reply { reply, delivery, source } = e.resolve(&user("x")) else {
            panic!()
        };
        assert_eq!(reply.tool_calls[0].arguments, r#"{"a":1}"#);
        assert_eq!(reply.tool_calls[1].arguments, "{bad");
        assert_eq!(reply.finish, FinishReason::ToolCalls);
        assert_eq!(delivery.ttft, Duration::from_millis(300));
        assert_eq!(delivery.token_interval, Duration::from_secs_f64(1.0 / 40.0));
        assert_eq!((delivery.chunk_tokens, delivery.cut_after_chunks), (3, Some(2)));
        assert_eq!(source, Source::Script { index: 0, name: None });
        assert!(reply.usage.completion_tokens > 0);
    }

    #[test]
    fn keep_hits_follows_names_and_after_is_checked() {
        let e = engine(false);
        let parse = |j: &str| Rule::parse_many(j, ScriptFormat::Json).unwrap();
        e.scripts
            .replace(parse(
                r#"[{"name":"a","reply":{"text":"A"}},{"name":"b","reply":{"text":"B"},"match":{"after":"a"}}]"#,
            ))
            .unwrap();
        e.resolve(&user("x"));
        // Reordered: "a" moves to index 1 and keeps its hit.
        e.scripts
            .replace_keep_hits(parse(r#"[{"name":"c","reply":{"text":"C"}},{"name":"a","reply":{"text":"A"}}]"#))
            .unwrap();
        let list = e.scripts.list();
        assert_eq!((list[0].hits, list[1].hits), (0, 1));
        // Unknown and cyclic gates are rejected, nothing changes.
        let err = e
            .scripts
            .replace(parse(r#"[{"name":"a","reply":{"text":"A"},"match":{"after":"nope"}}]"#))
            .unwrap_err();
        assert!(err.to_string().contains("names no rule"), "{err}");
        let err = e
            .scripts
            .replace(parse(
                r#"[{"name":"a","reply":{"text":"A"},"match":{"after":"b"}},{"name":"b","reply":{"text":"B"},"match":{"after":"a"}}]"#,
            ))
            .unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");
        assert_eq!(e.scripts.len(), 2);
    }

    #[test]
    fn count_tokens_does_not_spend_a_rules_budget() {
        let e = engine(false);
        e.scripts
            .replace(
                Rule::parse_many(
                    r#"[{"times":1,"reply":{"text":"hi","usage":{"prompt_tokens":77}}}]"#,
                    ScriptFormat::Json,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(e.count_input_tokens(&user("x")), 77);
        assert_eq!(e.count_input_tokens(&user("x")), 77);
        let info = &e.scripts.list()[0];
        assert_eq!((info.hits, info.reached), (0, 0));
        let Outcome::Reply { reply, .. } = e.resolve(&user("x")) else {
            panic!()
        };
        assert_eq!(reply.usage.prompt_tokens, 77);
    }

    #[test]
    fn engine_truncates_to_max_tokens() {
        let e = engine(false);
        e.scripts
            .replace(vec![text_rule(Match::default(), "one two three four five six seven eight", None)])
            .unwrap();
        let mut req = user("x");
        req.max_tokens = Some(2);
        let Outcome::Reply { reply, .. } = e.resolve(&req) else { panic!() };
        assert_eq!(reply.finish, FinishReason::Length);
        assert!(reply.usage.completion_tokens <= 2);
    }

    #[test]
    fn engine_error_defaults_and_overrides() {
        let e = engine(false);
        let rules = Rule::parse_many(
            r#"[{"match":{"last_user":"a"},"reply":{"error":{"status":429,"retry_after":2},"delay_ms":50}},
                {"reply":{"error":{"status":500,"type":"boom","code":"c","message":"m","param":"p"}}}]"#,
            ScriptFormat::Json,
        )
        .unwrap();
        e.scripts.replace(rules).unwrap();
        let Outcome::Error { error, delivery, .. } = e.resolve(&user("a")) else {
            panic!()
        };
        assert_eq!(error.code.as_deref(), Some("rate_limit_exceeded"));
        assert_eq!(error.retry_after, Some(Duration::from_secs(2)));
        assert_eq!(delivery.ttft, Duration::from_millis(50));
        let Outcome::Error { error, .. } = e.resolve(&user("b")) else {
            panic!()
        };
        assert_eq!((error.kind.as_str(), error.message.as_str()), ("boom", "m"));
        assert_eq!((error.code.as_deref(), error.param.as_deref()), (Some("c"), Some("p")));
    }

    #[test]
    fn engine_strict_and_fallback() {
        let strict = engine(true);
        let Outcome::Error { error, source, .. } = strict.resolve(&user("x")) else {
            panic!()
        };
        assert_eq!(source, Source::Strict);
        assert_eq!(error.code.as_deref(), Some("fakellm_no_matching_rule"));
        let lax = engine(false);
        let Outcome::Reply { reply, source, .. } = lax.resolve(&user("x")) else {
            panic!()
        };
        assert_eq!(source, Source::Fallback);
        assert!(reply.usage.prompt_tokens > 0);
    }

    #[test]
    fn tps_interval() {
        assert_eq!(interval_from_tps(None), Duration::ZERO);
        assert_eq!(interval_from_tps(Some(-1.0)), Duration::ZERO);
        assert_eq!(interval_from_tps(Some(10.0)), Duration::from_millis(100));
    }

    fn yaml(text: &str) -> Engine {
        let e = engine(true);
        e.scripts.replace(Rule::parse_many(text, ScriptFormat::Yaml).unwrap()).unwrap();
        e
    }

    fn is_script(o: &Outcome) -> bool {
        matches!(o.source(), Source::Script { .. })
    }

    #[test]
    fn protocol_matcher() {
        let e = yaml("- match: { protocol: anthropic.messages }\n  reply: { text: a }\n");
        let mut req = user("x");
        assert!(!is_script(&e.resolve(&req)));
        req.protocol = Protocol::AnthropicMessages;
        assert!(is_script(&e.resolve(&req)));
    }

    #[test]
    fn every_nth_counts_reached_requests() {
        let e = yaml("- match: { last_user: /go/, every_nth: 3 }\n  reply: { text: a }\n");
        let got: Vec<bool> = (0..6)
            .map(|i| {
                // Requests that fail another condition do not count as reached.
                assert!(!is_script(&e.resolve(&user("other"))));
                is_script(&e.resolve(&user(&format!("go {i}"))))
            })
            .collect();
        assert_eq!(got, [false, false, true, false, false, true]);
        let info = &e.scripts.list()[0];
        assert_eq!((info.hits, info.reached), (2, 6));
    }

    #[test]
    fn probability_is_deterministic_and_retries_vary() {
        let run = |p: f64| {
            let e = engine(true);
            e.scripts
                .replace(vec![text_rule(
                    Match {
                        probability: Some(p),
                        ..Default::default()
                    },
                    "a",
                    None,
                )])
                .unwrap();
            // Same request every time: only the reached counter varies the draw.
            (0..200).map(|_| is_script(&e.resolve(&user("same")))).collect::<Vec<_>>()
        };
        assert!(run(0.0).iter().all(|h| !h));
        assert!(run(1.0).iter().all(|h| *h));
        let half = run(0.5);
        assert_eq!(half, run(0.5));
        let hits = half.iter().filter(|h| **h).count();
        assert!((60..=140).contains(&hits), "{hits}");
    }

    #[test]
    fn probability_and_every_nth_validation() {
        for bad in ["probability: 1.5", "probability: -0.1", "every_nth: 0"] {
            let rules = Rule::parse_many(&format!("- match: {{ {bad} }}\n  reply: {{ text: a }}\n"), ScriptFormat::Yaml).unwrap();
            let err = Scripts::default().replace(rules).unwrap_err();
            assert!(err.0.contains("match."), "{err}");
        }
    }

    #[test]
    fn nan_probability_is_rejected() {
        let m = Match {
            probability: Some(f64::NAN),
            ..Default::default()
        };
        assert!(Scripts::default().replace(vec![text_rule(m, "a", None)]).is_err());
    }

    #[test]
    fn probability_and_every_nth_both_must_hold() {
        let e = yaml("- match: { probability: 0.0, every_nth: 1 }\n  reply: { text: a }\n");
        assert!(!is_script(&e.resolve(&user("x"))));
        assert_eq!(e.scripts.list()[0].reached, 1);
    }

    #[test]
    fn keep_hits_carries_reached() {
        let e = yaml("- match: { every_nth: 2 }\n  reply: { text: a }\n");
        e.resolve(&user("x"));
        e.resolve(&user("x"));
        let rules = e.scripts.list().into_iter().map(|r| r.rule).collect();
        e.scripts.replace_keep_hits(rules).unwrap();
        let info = &e.scripts.list()[0];
        assert_eq!((info.hits, info.reached), (1, 2));
    }

    #[test]
    fn hang_outcomes() {
        let e = yaml(
            "- match: { last_user: forever }\n  reply: { hang: true }\n- match: { last_user: later }\n  reply: { hang_ms: 500, text: hi }\n",
        );
        let Outcome::Hang { until: None, then } = e.resolve(&user("forever")) else {
            panic!()
        };
        let Outcome::Reply { reply, .. } = *then else { panic!() };
        assert_eq!(reply.text.as_deref(), Some(""));
        let Outcome::Hang { until, then } = e.resolve(&user("later")) else {
            panic!()
        };
        assert_eq!(until, Some(Duration::from_millis(500)));
        let Outcome::Reply { reply, .. } = *then else { panic!() };
        assert_eq!(reply.text.as_deref(), Some("hi"));
    }

    #[test]
    fn hang_then_error() {
        let e = yaml("- reply: { hang_ms: 10, error: { status: 503 } }\n");
        let Outcome::Hang { then, .. } = e.resolve(&user("x")) else {
            panic!()
        };
        assert!(matches!(*then, Outcome::Error { .. }));
    }

    #[test]
    fn error_in_stream_headers_and_protocol_defaults() {
        let e = yaml(
            "- reply: { error: { status: 429, in_stream: true, retry_after: 1, headers: { Retry-After-Ms: '1500', x-should-retry: 'true' } } }\n",
        );
        let Outcome::Error { error, .. } = e.resolve(&user("x")) else {
            panic!()
        };
        assert!(error.in_stream);
        assert_eq!(error.retry_after, Some(Duration::from_secs(1)));
        assert_eq!(
            error.headers,
            [
                ("retry-after-ms".to_owned(), "1500".to_owned()),
                ("x-should-retry".to_owned(), "true".to_owned())
            ]
        );
        let mut req = user("x");
        req.protocol = Protocol::AnthropicMessages;
        let Outcome::Error { error, .. } = e.resolve(&req) else { panic!() };
        assert_eq!(error.kind, ApiError::for_status_in(Protocol::AnthropicMessages, 429).kind);
    }

    #[test]
    fn reasoning_opaque_and_anthropic_tool_ids() {
        let e = yaml("- reply: { reasoning: think, reasoning_opaque: sig, tool_calls: [{ name: f }] }\n");
        let mut req = user("x");
        req.protocol = Protocol::AnthropicMessages;
        let Outcome::Reply { reply, .. } = e.resolve(&req) else { panic!() };
        assert_eq!(reply.reasoning_opaque.as_deref(), Some("sig"));
        assert!(reply.tool_calls[0].id.starts_with("toolu_"));
    }

    #[test]
    fn trajectory_and_policy_replies_validate() {
        let rule = |reply: serde_json::Value| Rule::parse_many(&json!([{ "reply": reply }]).to_string(), ScriptFormat::Json);
        let ok = |reply: serde_json::Value| Scripts::default().replace(rule(reply).unwrap()).is_ok();
        assert!(ok(json!({"trajectory": "t"})));
        // hang_ms and delay_ms / ttft_ms mean different things; combining them is refused.
        assert!(ok(json!({"text": "x", "hang_ms": 50})));
        assert!(!ok(json!({"text": "x", "hang_ms": 50, "delay_ms": 5})));
        assert!(!ok(json!({"text": "x", "hang_ms": 50, "stream": {"ttft_ms": 5}})));
        let err = Scripts::default()
            .replace(rule(json!({"text": "x", "hang_ms": 50, "delay_ms": 5})).unwrap())
            .unwrap_err();
        assert!(err.0.contains("hang_ms") && err.0.contains("delay_ms"), "{}", err.0);
        assert!(ok(json!({"policy": "p", "stream": {"tokens_per_sec": 10}, "delay_ms": 5})));
        assert!(!ok(json!({"trajectory": "t", "text": "x"})));
        assert!(!ok(json!({"policy": "p", "error": {"status": 500}})));
        assert!(!ok(json!({"policy": "p", "trajectory": "t"})));
        assert!(!ok(json!({"text": "x", "stream": {"args_chunk_bytes": 0}})));
        assert!(
            validate_leaf_reply(&ReplySpec {
                text: Some("x".into()),
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            validate_leaf_reply(&ReplySpec {
                trajectory: Some("t".into()),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn delegated_replies_inherit_delivery_and_policy_needs_async() {
        let base = ReplySpec {
            policy: Some("p".into()),
            delay_ms: Some(40),
            usage: Some(UsageSpec {
                prompt_tokens: Some(9),
                ..Default::default()
            }),
            ..Default::default()
        };
        let step = ReplySpec {
            text: Some("t".into()),
            delay_ms: Some(5),
            ..Default::default()
        };
        let merged = step.with_defaults_from(&base);
        assert_eq!((merged.delay_ms, merged.usage.and_then(|u| u.prompt_tokens)), (Some(5), Some(9)));
        let e = engine(false);
        e.scripts
            .replace(vec![Rule {
                name: None,
                matcher: Match::default(),
                reply: base,
                times: None,
            }])
            .unwrap();
        let Outcome::Error { error, source, .. } = e.resolve(&user("x")) else {
            panic!()
        };
        assert_eq!(error.code.as_deref(), Some("fakellm_policy_error"));
        assert!(matches!(source, Source::Policy { .. }));
    }

    #[test]
    fn last_user_raw_and_args_chunk_bytes() {
        let raw = Match {
            last_user_raw: pat("hello"),
            ..Default::default()
        };
        let s = Scripts::default();
        s.replace(vec![text_rule(raw, "a", None)]).unwrap();
        assert!(s.resolve(&user("hello there")).is_some());
        assert!(s.resolve(&user("bye")).is_none());
        let e = engine(false);
        e.scripts
            .replace(vec![Rule {
                name: None,
                matcher: Match::default(),
                reply: ReplySpec {
                    text: Some("x".into()),
                    stream: Some(StreamSpec {
                        args_chunk_bytes: Some(3),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                times: None,
            }])
            .unwrap();
        let Outcome::Reply { delivery, .. } = e.resolve(&user("x")) else {
            panic!()
        };
        assert_eq!(delivery.args_chunk_bytes, 3);
    }

    #[test]
    fn anthropic_input_tokens_override_is_literal() {
        let e = engine(false);
        e.scripts
            .replace(vec![Rule {
                name: None,
                matcher: Match::default(),
                reply: ReplySpec {
                    text: Some("x".into()),
                    usage: Some(UsageSpec {
                        prompt_tokens: Some(20236),
                        cached_tokens: Some(5),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                times: None,
            }])
            .unwrap();
        let mut anthropic = user("x");
        anthropic.protocol = Protocol::AnthropicMessages;
        let Outcome::Reply { reply, .. } = e.resolve(&anthropic) else {
            panic!()
        };
        assert_eq!((reply.usage.prompt_tokens, reply.usage.cached_tokens), (20241, 5));
        let Outcome::Reply { reply, .. } = e.resolve(&user("x")) else {
            panic!()
        };
        assert_eq!(reply.usage.prompt_tokens, 20236);
        assert_eq!(e.count_input_tokens(&anthropic), 20236);
    }
}
