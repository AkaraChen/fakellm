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

use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use regex::Regex;

use crate::fallback::{self, FallbackConfig};
use crate::ir::{ApiError, Delivery, FinishReason, Outcome, Reply, Request, Source, ToolCall, Usage};
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
    /// Regex over the most recent user message (`Request::last_user_text`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user: Option<Pattern>,
    /// Regex over the system/developer text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<Pattern>,
    /// Regex over the text of any message, any role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub any_message: Option<Pattern>,
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
    /// Streaming requests only (`true`) or sync only (`false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Number of assistant turns so far (0 on the first request).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<usize>,
    /// Explicit catch-all, for readability; `{}` matches everything too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub any: Option<bool>,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
}

/// What to answer. Exactly one of `text`/`tool_calls` (together is fine) or `error` must be set.
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

impl Pattern {
    fn compile(&self) -> Result<Regex, regex::Error> {
        let s = self.0.as_str();
        let (body, ci) = if s.len() >= 2 && s.starts_with('/') && s.ends_with('/') {
            (&s[1..s.len() - 1], false)
        } else if s.len() >= 3 && s.starts_with('/') && s.ends_with("/i") {
            (&s[1..s.len() - 2], true)
        } else {
            (s, false)
        };
        if ci { Regex::new(&format!("(?i){body}")) } else { Regex::new(body) }
    }
}

#[derive(Default)]
struct Matchers {
    last_user: Option<Regex>,
    system: Option<Regex>,
    any_message: Option<Regex>,
    tool_result_contains: Option<Regex>,
}

impl Matchers {
    fn compile(m: &Match) -> Result<Self, String> {
        let one = |name: &str, p: &Option<Pattern>| -> Result<Option<Regex>, String> {
            p.as_ref()
                .map(|p| p.compile().map_err(|e| format!("match.{name}: invalid regex {:?}: {e}", p.0)))
                .transpose()
        };
        Ok(Self {
            last_user: one("last_user", &m.last_user)?,
            system: one("system", &m.system)?,
            any_message: one("any_message", &m.any_message)?,
            tool_result_contains: one("tool_result_contains", &m.tool_result_contains)?,
        })
    }
}

fn validate(rule: &Rule) -> Result<(), String> {
    let r = &rule.reply;
    let has_content = r.text.is_some() || r.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
    match (&r.error, has_content) {
        (None, false) => return Err("reply needs `text`, `tool_calls` or `error`".into()),
        (Some(_), true) => return Err("reply `error` excludes `text` and `tool_calls`".into()),
        _ => {}
    }
    if let Some(tps) = r.stream.as_ref().and_then(|s| s.tokens_per_sec)
        && !(tps > 0.0 && tps.is_finite() && Duration::try_from_secs_f64(1.0 / tps).is_ok())
    {
        return Err("reply.stream.tokens_per_sec must be greater than 0 and not vanishingly small".into());
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

/// The registered rules with their compiled matchers and hit counters.
#[derive(Default)]
pub struct Scripts {
    inner: Mutex<Vec<Compiled>>,
}

struct Compiled {
    rule: Rule,
    hits: u64,
    re: Matchers,
}

impl Compiled {
    fn new(rule: Rule, index: usize) -> Result<Self, ScriptError> {
        validate(&rule).map_err(|e| ScriptError(format!("rule {index}: {e}")))?;
        let re = Matchers::compile(&rule.matcher).map_err(|e| ScriptError(format!("rule {index}: {e}")))?;
        Ok(Self { rule, hits: 0, re })
    }

    fn exhausted(&self) -> bool {
        self.rule.times.is_some_and(|t| self.hits >= u64::from(t))
    }

    fn matches(&self, req: &Request) -> bool {
        let m = &self.rule.matcher;
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

fn compile_all(rules: Vec<Rule>, offset: usize) -> Result<Vec<Compiled>, ScriptError> {
    rules.into_iter().enumerate().map(|(i, r)| Compiled::new(r, offset + i)).collect()
}

impl Scripts {
    /// Replaces all rules. Fails without changing anything if any rule is invalid.
    pub fn replace(&self, rules: Vec<Rule>) -> Result<(), ScriptError> {
        let compiled = compile_all(rules, 0)?;
        *self.inner.lock() = compiled;
        Ok(())
    }

    /// Appends rules after the existing ones.
    pub fn append(&self, rules: Vec<Rule>) -> Result<(), ScriptError> {
        let mut inner = self.inner.lock();
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
                exhausted: c.exhausted(),
            })
            .collect()
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

    /// The first unexhausted rule matching `req`, counting the hit. Returns the rule's index and a
    /// clone of its reply spec.
    pub fn resolve(&self, req: &Request) -> Option<(usize, Option<String>, ReplySpec)> {
        let mut inner = self.inner.lock();
        let (index, c) = inner.iter_mut().enumerate().find(|(_, c)| !c.exhausted() && c.matches(req))?;
        c.hits += 1;
        Some((index, c.rule.name.clone(), c.rule.reply.clone()))
    }
}

/// Decides every request: scripts, then fallback or strict error. Owned by the control plane.
pub struct Engine {
    pub scripts: Scripts,
    pub fallback: FallbackConfig,
    pub strict: bool,
    /// Delivery used when the matching rule has no `stream`/`delay_ms`.
    pub default_delivery: Delivery,
}

impl Engine {
    pub fn new(fallback: FallbackConfig, strict: bool, default_delivery: Delivery) -> Self {
        Self {
            scripts: Scripts::default(),
            fallback,
            strict,
            default_delivery,
        }
    }

    /// Produces the outcome for a parsed request: the matching rule's reply (text truncated to
    /// `max_tokens`, tool call ids filled in, `usage` computed with the tokenizer, delivery from
    /// the rule's `stream`/`delay_ms` over `default_delivery`), or the fallback, or the strict
    /// error. Pure apart from the hit counters.
    pub fn resolve(&self, req: &Request) -> Outcome {
        let Some((index, name, spec)) = self.scripts.resolve(req) else {
            return self.unscripted(req);
        };
        let source = Source::Script { index, name };
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
        } else if let Some(ms) = spec.delay_ms {
            delivery.ttft = Duration::from_millis(ms);
        }

        if let Some(e) = spec.error {
            let mut error = ApiError::for_status(e.status);
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
            if let Some(ms) = spec.delay_ms {
                delivery.ttft = Duration::from_millis(ms);
            }
            return Outcome::Error { error, delivery, source };
        }

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
                id: c.id.unwrap_or_else(|| fallback::call_id(seed ^ i as u64)),
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
        let text = spec.text.map(|t| match req.max_tokens {
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
        let reasoning_tokens = spec.reasoning.as_deref().map_or(0, tokenizer::count);
        let completion_tokens = text.as_deref().map_or(0, tokenizer::count)
            + tool_calls.iter().map(|c| tokenizer::count(&c.arguments)).sum::<u32>()
            + reasoning_tokens;
        let reply = Reply {
            text,
            tool_calls,
            reasoning: spec.reasoning,
            refusal: None,
            finish,
            usage: Usage {
                prompt_tokens: tokenizer::count_request(req),
                completion_tokens,
                reasoning_tokens,
                cached_tokens: 0,
            },
        };
        Outcome::Reply { reply, delivery, source }
    }

    fn unscripted(&self, req: &Request) -> Outcome {
        if self.strict {
            return Outcome::Error {
                error: ApiError::no_matching_rule(&req.model),
                delivery: self.default_delivery.clone(),
                source: Source::Strict,
            };
        }
        let mut reply = fallback::reply(req, &self.fallback);
        reply.usage.prompt_tokens = tokenizer::count_request(req);
        reply.usage.completion_tokens = reply.text.as_deref().map_or(0, tokenizer::count)
            + reply.tool_calls.iter().map(|c| tokenizer::count(&c.arguments)).sum::<u32>()
            + reply.reasoning.as_deref().map_or(0, tokenizer::count);
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
            kind: None,
            code: None,
            message: None,
            param: None,
            retry_after: None,
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
}
