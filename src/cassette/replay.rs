//! Semantic matching of a live request against recorded exchanges: normalize both into the same comparable form
//! (docs/m4-spec.md section 3.3) and compare that, never bytes. A request recorded through Chat matches the
//! same conversation sent through Responses or Messages because the form is built from the IR.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use crate::cassette::CassetteError;
use crate::cassette::format::Cassette;
use crate::cassette::normalize::{IgnoreSet, Normalizers, canonical};
use crate::ir::{Message, Part, Request, Role, ToolChoice};
use crate::reqlog::glob_match;

/// The canonical text of a normalized request; two requests match exactly when their keys are equal.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key(pub String);

/// One cassette's compiled matching rules.
#[derive(Clone, Debug)]
pub struct Matcher {
    ignore: IgnoreSet,
    normalizers: Normalizers,
    match_tool_schemas: bool,
    model_glob: String,
}

impl Matcher {
    /// Compiles the cassette's `ignore`, `normalize`, `normalize_defaults`, `match_tool_schemas` and `model_glob`.
    pub fn compile(cassette: &Cassette) -> Result<Self, CassetteError> {
        if cassette.model_glob.is_empty() {
            return Err(CassetteError("model_glob must not be empty".into()));
        }
        Ok(Self {
            ignore: IgnoreSet::compile(&[&cassette.ignore])?,
            normalizers: Normalizers::compile(&cassette.normalize, cassette.normalize_defaults)?,
            match_tool_schemas: cassette.match_tool_schemas,
            model_glob: cassette.model_glob.clone(),
        })
    }

    /// Whether the cassette answers requests for `model` (`model_glob`).
    pub fn accepts_model(&self, model: &str) -> bool {
        glob_match(&self.model_glob, model)
    }

    /// The comparable form of `req`: the ignore list (this cassette's plus `exchange_ignore`) applied, texts
    /// normalized, roles unified (`developer` is `system`), reasoning and thinking dropped, tool calls as name and
    /// parsed arguments without ids, tool results as text and `is_error`, tools as a sorted name list (with
    /// canonical parameter schemas under `match_tool_schemas`), and the remaining scalar fields.
    pub fn key(&self, req: &Request, exchange_ignore: &[String]) -> Key {
        let extended;
        let ignore = if exchange_ignore.is_empty() {
            &self.ignore
        } else {
            extended = self.ignore.with(exchange_ignore).unwrap_or_else(|_| self.ignore.clone());
            &extended
        };
        let mut doc = Map::new();
        let drop_system = ignore.ignores("system", None);
        let messages: Vec<Value> = req
            .messages
            .iter()
            .filter(|m| !(drop_system && matches!(m.role, Role::System | Role::Developer)))
            .map(|m| self.message(m))
            .collect();
        doc.insert("messages".into(), Value::Array(messages));
        if !ignore.ignores("tools", None) {
            let mut tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    if self.match_tool_schemas {
                        json!({"name": t.name, "parameters": t.parameters})
                    } else {
                        Value::String(t.name.clone())
                    }
                })
                .collect();
            tools.sort_by_key(canonical);
            doc.insert("tools".into(), Value::Array(tools));
        }
        let choice = req.tool_choice.as_ref().map(|c| match c {
            ToolChoice::Auto => "auto".to_string(),
            ToolChoice::None => "none".into(),
            ToolChoice::Required => "required".into(),
            ToolChoice::Tool { name } => format!("tool:{name}"),
        });
        let mut put = |field: &str, value: Value, text: Option<&str>| {
            if !ignore.ignores(field, text) {
                doc.insert(field.into(), value);
            }
        };
        put(
            "tool_choice",
            serde_json::to_value(&req.tool_choice).unwrap_or_default(),
            Some(choice.as_deref().unwrap_or("auto")),
        );
        put("parallel_tool_calls", json!(req.parallel_tool_calls), None);
        put("n", json!(req.n), None);
        put("stop", json!(req.stop), None);
        put("thinking", serde_json::to_value(&req.thinking).unwrap_or_default(), None);
        put(
            "response_format",
            serde_json::to_value(&req.response_format).unwrap_or_default(),
            None,
        );
        put("temperature", json!(req.temperature), None);
        put("top_p", json!(req.top_p), None);
        put("seed", json!(req.seed), None);
        put("user", json!(req.user), None);
        put("metadata", Value::Object(req.metadata.clone()), None);
        put("stream", json!(req.stream), None);
        put("include_usage", json!(req.include_usage), None);
        put("store", json!(req.store), None);
        put("max_tokens", json!(req.max_tokens), None);
        put("reasoning_effort", json!(req.reasoning_effort), None);
        put("previous_response_id", json!(req.previous_response_id), None);
        for (k, v) in &req.extra {
            put(&format!("extra.{k}"), v.clone(), None);
        }
        Key(canonical(&Value::Object(doc)))
    }

    /// The token set of a whole conversation for `fuzzy` lookups: the lower-cased alphanumeric words of every
    /// message (normalized like the exact key, thinking left out), tool-call names and arguments included. Computed
    /// once per live request and once per exchange at registration.
    pub fn tokens(&self, req: &Request) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut add = |text: &str| {
            let text = self.normalizers.apply(text);
            out.extend(
                text.split(|c: char| !c.is_alphanumeric())
                    .filter(|w| !w.is_empty())
                    .map(str::to_lowercase),
            );
        };
        for m in &req.messages {
            for part in &m.content {
                if let Some(text) = self.part(part) {
                    add(&text);
                }
            }
            for call in &m.tool_calls {
                add(&call.name);
                add(&call.arguments);
            }
        }
        out
    }

    /// Jaccard similarity of two token sets: `|a & b| / |a | b|`; two empty sets are not similar (0).
    pub fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
        let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
        let inter = small.iter().filter(|t| large.contains(*t)).count();
        let union = a.len() + b.len() - inter;
        if union == 0 { 0.0 } else { inter as f64 / union as f64 }
    }

    fn message(&self, m: &Message) -> Value {
        let role = match m.role {
            Role::Developer => Role::System,
            r => r,
        };
        let mut out = Map::new();
        out.insert("role".into(), serde_json::to_value(role).unwrap_or_default());
        let texts: Vec<String> = m.content.iter().filter_map(|p| self.part(p)).collect();
        // One entry per part, so `["a", "b"]` and `["a\nb"]` stay different.
        let texts = texts.iter().map(|t| Value::String(self.normalizers.apply(t))).collect();
        out.insert("content".into(), Value::Array(texts));
        if !m.tool_calls.is_empty() {
            let calls = m
                .tool_calls
                .iter()
                .map(|c| {
                    let arguments = match serde_json::from_str::<Value>(&c.arguments) {
                        Ok(v) => self.normalize_strings(v),
                        Err(_) => Value::String(self.normalizers.apply(&c.arguments)),
                    };
                    json!({"name": c.name, "arguments": arguments})
                })
                .collect();
            out.insert("tool_calls".into(), Value::Array(calls));
        }
        if m.is_error {
            out.insert("is_error".into(), Value::Bool(true));
        }
        Value::Object(out)
    }

    /// The text a part contributes; `None` for parts matching ignores (thinking).
    fn part(&self, part: &Part) -> Option<String> {
        Some(match part {
            Part::Text { text } | Part::Refusal { text } => text.clone(),
            Part::Image { url, .. } => format!("image:{url}"),
            Part::Audio { data, format } => {
                use std::hash::{DefaultHasher, Hash, Hasher};
                let mut h = DefaultHasher::new();
                data.hash(&mut h);
                format!("audio:{}:{}:{:016x}", format.as_deref().unwrap_or(""), data.len(), h.finish())
            }
            Part::File { .. } | Part::Other { .. } => canonical(&serde_json::to_value(part).unwrap_or_default()),
            Part::Thinking { .. } | Part::RedactedThinking { .. } => return None,
        })
    }

    fn normalize_strings(&self, value: Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.normalizers.apply(&s)),
            Value::Array(a) => Value::Array(a.into_iter().map(|v| self.normalize_strings(v)).collect()),
            Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, self.normalize_strings(v))).collect()),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassette::format::Normalizer;
    use crate::ir::{Protocol, ToolCall, ToolDef};

    fn user(text: &str) -> Message {
        Message::text(Role::User, text)
    }

    fn matcher(c: &Cassette) -> Matcher {
        Matcher::compile(c).unwrap()
    }

    fn key(c: &Cassette, req: &Request) -> Key {
        matcher(c).key(req, &[])
    }

    fn tool(name: &str, parameters: Value) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: None,
            parameters: Some(parameters),
            strict: None,
        }
    }

    #[test]
    fn protocol_sampling_ids_and_roles_do_not_matter() {
        let c = Cassette::new("c");
        let mut a = Request::new(
            "gpt-4o",
            vec![Message::text(Role::System, "be brief"), user("hi at 2026-11-01T10:00:00Z")],
        );
        let mut b = Request::new(
            "claude",
            vec![Message::text(Role::Developer, "be brief"), user("hi at 2027-01-02T03:04:05Z")],
        );
        b.protocol = Protocol::AnthropicMessages;
        b.temperature = Some(0.2);
        b.stream = true;
        b.max_tokens = Some(10);
        b.extra.insert("foo".into(), json!(1));
        assert_eq!(key(&c, &a), key(&c, &b));
        a.messages.push(user("more"));
        assert_ne!(key(&c, &a), key(&c, &b));
    }

    #[test]
    fn tool_calls_compare_by_name_and_parsed_arguments_without_ids() {
        let c = Cassette::new("c");
        let call = |id: &str, args: &str| {
            let mut m = Message::text(Role::Assistant, "");
            m.tool_calls = vec![ToolCall {
                id: id.into(),
                name: "ls".into(),
                arguments: args.into(),
            }];
            let mut r = Message::text(Role::Tool, "out /tmp/x");
            r.tool_call_id = Some(id.into());
            Request::new("m", vec![user("go"), m, r])
        };
        let a = call("call_1", r#"{"a": 1, "path": "req_abcdefghijklmnop"}"#);
        let b = call("toolu_9", r#"{"path":"req_zzzzzzzzzzzzzzzzz","a":1}"#);
        assert_eq!(key(&c, &a), key(&c, &b));
        assert_ne!(key(&c, &a), key(&c, &call("x", r#"{"a":2,"path":"req_abcdefghijklmnop"}"#)));
        let mut errored = call("x", r#"{"a": 1, "path": "req_abcdefghijklmnop"}"#);
        errored.messages[2].is_error = true;
        assert_ne!(key(&c, &a), key(&c, &errored));
    }

    #[test]
    fn tools_by_name_unless_schemas_matter() {
        let mut a = Request::new("m", vec![user("hi")]);
        a.tools = vec![tool("b", json!({"x": 1})), tool("a", json!({}))];
        let mut b = a.clone();
        b.tools = vec![tool("a", json!({"changed": true})), tool("b", json!({"x": 1}))];
        let mut c = Cassette::new("c");
        assert_eq!(key(&c, &a), key(&c, &b));
        c.match_tool_schemas = true;
        assert_ne!(key(&c, &a), key(&c, &b));
        c.match_tool_schemas = false;
        c.ignore = vec!["tools".into()];
        b.tools.clear();
        assert_eq!(key(&c, &a), key(&c, &b));
    }

    #[test]
    fn ignore_system_and_removed_defaults() {
        let mut c = Cassette::new("c");
        let a = Request::new("m", vec![Message::text(Role::System, "one"), user("hi")]);
        let b = Request::new("m", vec![Message::text(Role::System, "two"), user("hi")]);
        assert_ne!(key(&c, &a), key(&c, &b));
        c.ignore = vec!["system".into(), "!max_tokens".into()];
        assert_eq!(key(&c, &a), key(&c, &b));
        let mut d = a.clone();
        d.max_tokens = Some(5);
        assert_ne!(key(&c, &a), key(&c, &d));
        assert_eq!(
            matcher(&c).key(&d, &["max_tokens".into()]),
            matcher(&c).key(&a, &["max_tokens".into()])
        );
    }

    #[test]
    fn user_normalizers_and_model_glob() {
        let mut c = Cassette::new("c");
        c.normalize = vec![Normalizer {
            pattern: r"\d{4}-\d{2}-\d{2}".into(),
            replace: "<date>".into(),
        }];
        c.model_glob = "gpt-*".into();
        let a = Request::new("m", vec![user("today is 2025-03-14")]);
        let b = Request::new("m", vec![user("today is 2026-11-01")]);
        assert_eq!(key(&c, &a), key(&c, &b));
        assert!(matcher(&c).accepts_model("gpt-4o") && !matcher(&c).accepts_model("claude-3"));
    }

    #[test]
    fn jaccard_over_the_whole_conversation() {
        let c = Cassette::new("c");
        let m = matcher(&c);
        let mk = |texts: &[&str]| {
            Request::new(
                "m",
                texts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| Message::text(if i % 2 == 0 { Role::User } else { Role::Assistant }, *t))
                    .collect(),
            )
        };
        let sim = |a: &Request, b: &Request| Matcher::jaccard(&m.tokens(a), &m.tokens(b));
        let rec = mk(&["Please fix the failing test in parser.rs", "ok", "run it again"]);
        assert_eq!(sim(&rec, &rec), 1.0);
        // Case, punctuation and message boundaries do not matter.
        assert_eq!(
            sim(&rec, &mk(&["please FIX the failing test, in parser.rs!", "ok", "run it again"])),
            1.0
        );
        // One word of ten differs: near.
        let near = sim(&rec, &mk(&["Please fix the failing test in lexer.rs", "ok", "run it again"]));
        assert!(near > 0.7 && near < 1.0, "{near}");
        // Unrelated: far.
        assert!(sim(&rec, &mk(&["what is the weather in Paris"])) < 0.2);
        assert_eq!(Matcher::jaccard(&HashSet::new(), &HashSet::new()), 0.0);
    }
}
