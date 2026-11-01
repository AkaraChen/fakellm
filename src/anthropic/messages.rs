//! Messages API: request JSON to IR, and an IR reply to the sync `message` object.
//! Wire rules are in docs/m1-spec.md ("Anthropic Messages").
//!
//! Parsing follows Anthropic's validation wording (`field: problem`) because SDK users and
//! harnesses match on it. One deliberate reshaping: a user message mixing `tool_result` blocks
//! and other blocks is split into `Tool` messages and `User` messages, in order, so scripts keyed
//! on tool results see the same shape as with the OpenAI protocols.

use serde_json::{Map, Value, json};

use crate::fallback;
use crate::ir::{
    ApiError, FinishReason, Message, Part, Protocol, Reply, Request, Role, ThinkingConfig, ToolCall, ToolChoice, ToolDef, Usage,
};

/// Identity shared by every event of one response.
#[derive(Clone, Debug)]
pub struct MessageMeta {
    /// `msg_...`, from `fallback::message_id`.
    pub id: String,
    /// Echoed from the request.
    pub model: String,
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::invalid_request(message)
}

fn field<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    obj.get(key).filter(|v| !v.is_null())
}

/// Parses a Messages request body into the IR (`Protocol::AnthropicMessages`).
///
/// `require_max_tokens` is `true` for `POST /v1/messages` (missing `max_tokens` is a 400
/// `invalid_request_error` "max_tokens: Field required", `param: None`) and `false` for
/// `count_tokens`, which has no `max_tokens`. See the spec for the full field mapping and the
/// error messages.
pub fn parse(body: &Value, require_max_tokens: bool) -> Result<Request, ApiError> {
    let Some(obj) = body.as_object() else {
        return Err(invalid("Input should be a valid dictionary or object"));
    };
    let model = match field(obj, "model") {
        None => return Err(invalid("model: Field required")),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(invalid("model: Input should be a valid string")),
    };
    let raw_messages = match field(obj, "messages") {
        None => return Err(invalid("messages: Field required")),
        Some(Value::Array(a)) if a.is_empty() => return Err(invalid("messages: at least one message is required")),
        Some(Value::Array(a)) => a,
        Some(_) => return Err(invalid("messages: Input should be a valid list")),
    };

    let max_tokens = match field(obj, "max_tokens") {
        None if require_max_tokens => return Err(invalid("max_tokens: Field required")),
        None => None,
        Some(v) => match v.as_i64() {
            None if v.as_u64().is_some() => Some(u32::MAX),
            None => return Err(invalid("max_tokens: Input should be a valid integer")),
            Some(n) if n < 1 => return Err(invalid("max_tokens: Input should be greater than or equal to 1")),
            Some(n) => Some(u32::try_from(n).unwrap_or(u32::MAX)),
        },
    };

    let thinking = match field(obj, "thinking") {
        None => None,
        Some(v) => Some(parse_thinking(v, max_tokens)?),
    };

    let mut messages = Vec::new();
    if let Some(system) = parse_system(field(obj, "system"))? {
        messages.push(system);
    }
    for (i, m) in raw_messages.iter().enumerate() {
        parse_message(i, m, &mut messages)?;
    }

    let mut req = Request::new(model, messages);
    req.protocol = Protocol::AnthropicMessages;
    req.max_tokens = max_tokens;
    req.thinking = thinking;

    let mut tools = Vec::new();
    match field(obj, "tools") {
        None => {}
        Some(Value::Array(a)) => {
            for (i, t) in a.iter().enumerate() {
                tools.push(parse_tool(i, t)?);
            }
        }
        Some(_) => return Err(invalid("tools: Input should be a valid list")),
    }
    req.tools = tools;

    let mut extra = Map::new();
    for (k, v) in obj {
        let typed = matches!(
            k.as_str(),
            "model" | "messages" | "system" | "max_tokens" | "thinking" | "tools" | "stop_sequences" | "metadata" | "stream"
        );
        if !typed && !v.is_null() {
            extra.insert(k.clone(), v.clone());
        }
    }
    if let Some(tc) = extra.remove("tool_choice") {
        match tool_choice(&tc) {
            Some((choice, parallel)) => {
                req.tool_choice = Some(choice);
                req.parallel_tool_calls = parallel;
            }
            None => {
                extra.insert("tool_choice".into(), tc);
            }
        }
    }
    req.temperature = extra.remove("temperature").and_then(|v| v.as_f64());
    req.top_p = extra.remove("top_p").and_then(|v| v.as_f64());
    req.stream = field(obj, "stream").and_then(Value::as_bool).unwrap_or(false);
    match field(obj, "stop_sequences") {
        None => {}
        Some(Value::Array(a)) => req.stop = a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect(),
        Some(_) => return Err(invalid("stop_sequences: Input should be a valid list")),
    }
    if let Some(Value::Object(m)) = field(obj, "metadata") {
        req.user = m.get("user_id").and_then(Value::as_str).map(str::to_owned);
        req.metadata = m.clone();
    } else if let Some(v) = field(obj, "metadata") {
        extra.insert("metadata".into(), v.clone());
    }
    req.extra = extra;
    Ok(req)
}

fn parse_thinking(v: &Value, max_tokens: Option<u32>) -> Result<ThinkingConfig, ApiError> {
    let Some(t) = v.as_object() else {
        return Err(invalid("thinking: Input should be a valid dictionary or object"));
    };
    let kind = match t.get("type").and_then(Value::as_str) {
        Some(k @ ("enabled" | "adaptive" | "disabled")) => k.to_owned(),
        _ => return Err(invalid("thinking.type: Input should be 'enabled', 'adaptive' or 'disabled'")),
    };
    let budget = field(t, "budget_tokens").and_then(Value::as_i64);
    if kind == "enabled" {
        match budget {
            None => return Err(invalid("thinking.enabled.budget_tokens: Field required")),
            Some(b) if b < 1024 => {
                return Err(invalid(
                    "thinking.enabled.budget_tokens: Input should be greater than or equal to 1024",
                ));
            }
            Some(b) if max_tokens.is_some_and(|m| b >= i64::from(m)) => {
                return Err(invalid("thinking.enabled.budget_tokens: Input should be less than max_tokens"));
            }
            Some(_) => {}
        }
    }
    Ok(ThinkingConfig {
        kind,
        budget_tokens: budget.and_then(|b| u32::try_from(b).ok()),
    })
}

fn parse_system(v: Option<&Value>) -> Result<Option<Message>, ApiError> {
    let parts = match v {
        None => return Ok(None),
        Some(Value::String(s)) if s.is_empty() => return Ok(None),
        Some(Value::String(s)) => vec![Part::Text { text: s.clone() }],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(
                |b| match (b.get("type").and_then(Value::as_str), b.get("text").and_then(Value::as_str)) {
                    (Some("text"), Some(text)) => Part::Text { text: text.to_owned() },
                    _ => Part::Other { value: b.clone() },
                },
            )
            .collect(),
        Some(_) => return Err(invalid("system: Input should be a valid string or a list of text blocks")),
    };
    let mut m = Message::text(Role::System, "");
    m.content = parts;
    Ok(Some(m))
}

fn parse_message(i: usize, m: &Value, out: &mut Vec<Message>) -> Result<(), ApiError> {
    let role = match m.get("role").and_then(Value::as_str) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        // Claude Code injects mid-conversation system turns as messages with role "system"; be lenient like a fake should be.
        Some("system") => Role::System,
        _ => return Err(invalid(format!("messages.{i}.role: Input should be 'user' or 'assistant'"))),
    };
    let blocks = match m.get("content") {
        Some(Value::String(s)) => {
            out.push(Message::text(role, s.clone()));
            return Ok(());
        }
        Some(Value::Array(a)) => a,
        None | Some(Value::Null) => return Err(invalid(format!("messages.{i}.content: Field required"))),
        Some(_) => return Err(invalid(format!("messages.{i}.content: Input should be a valid list"))),
    };
    if blocks.is_empty() {
        let mut empty = Message::text(role, "");
        empty.content.clear();
        out.push(empty);
        return Ok(());
    }

    if role == Role::Assistant {
        let mut msg = Message::text(Role::Assistant, "");
        msg.content.clear();
        let mut thinking = Vec::new();
        for (j, b) in blocks.iter().enumerate() {
            let ty = block_type(b, &format!("messages.{i}.content.{j}"))?;
            match ty {
                "tool_use" => {
                    let input = b.get("input").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!({}));
                    msg.tool_calls.push(ToolCall {
                        id: b.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        name: b.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        arguments: serde_json::to_string(&input).unwrap_or_else(|_| "{}".into()),
                    });
                }
                "thinking" => {
                    let text = b.get("thinking").and_then(Value::as_str).unwrap_or_default().to_owned();
                    let signature = b.get("signature").and_then(Value::as_str).map(str::to_owned);
                    thinking.push(text.clone());
                    if signature.is_some() {
                        msg.reasoning_opaque = signature.clone();
                    }
                    msg.content.push(Part::Thinking { text, signature });
                }
                _ => msg.content.push(block_part(b, ty)),
            }
        }
        if !thinking.is_empty() {
            msg.reasoning = Some(thinking.join("\n"));
        }
        out.push(msg);
        return Ok(());
    }

    // User: cut into maximal runs of tool_result / other blocks.
    let mut run: Vec<Part> = Vec::new();
    for (j, b) in blocks.iter().enumerate() {
        let path = format!("messages.{i}.content.{j}");
        let ty = block_type(b, &path)?;
        if ty == "tool_result" {
            if !run.is_empty() {
                let mut u = Message::text(Role::User, "");
                u.content = std::mem::take(&mut run);
                out.push(u);
            }
            out.push(tool_result_message(b, &path)?);
        } else {
            run.push(block_part(b, ty));
        }
    }
    if !run.is_empty() {
        let mut u = Message::text(Role::User, "");
        u.content = run;
        out.push(u);
    }
    Ok(())
}

fn block_type<'a>(b: &'a Value, path: &str) -> Result<&'a str, ApiError> {
    b.get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{path}.type: Field required")))
}

fn data_url(source: &Value) -> Option<String> {
    let media = source
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");
    let data = source.get("data").and_then(Value::as_str)?;
    Some(format!("data:{media};base64,{data}"))
}

fn block_part(b: &Value, ty: &str) -> Part {
    let str_of = |key: &str| b.get(key).and_then(Value::as_str).map(str::to_owned);
    let source = b.get("source");
    let source_str = |key: &str| source.and_then(|s| s.get(key)).and_then(Value::as_str).map(str::to_owned);
    match ty {
        "text" => Part::Text {
            text: str_of("text").unwrap_or_default(),
        },
        "image" => {
            let url = match source.and_then(|s| s.get("type")).and_then(Value::as_str) {
                Some("base64") => source.and_then(data_url),
                Some("url") => source_str("url"),
                _ => None,
            };
            match url {
                Some(url) => Part::Image { url, detail: None },
                None => Part::Other { value: b.clone() },
            }
        }
        "document" => {
            let file_data = match source.and_then(|s| s.get("type")).and_then(Value::as_str) {
                Some("base64") => source.and_then(data_url),
                Some("url") => source_str("url"),
                Some("text") => source_str("data"),
                _ => None,
            };
            Part::File {
                file_id: source_str("file_id"),
                filename: str_of("title"),
                file_data,
            }
        }
        "thinking" => Part::Thinking {
            text: str_of("thinking").unwrap_or_default(),
            signature: str_of("signature"),
        },
        "redacted_thinking" => Part::RedactedThinking {
            data: str_of("data").unwrap_or_default(),
        },
        _ => Part::Other { value: b.clone() },
    }
}

fn tool_result_message(b: &Value, path: &str) -> Result<Message, ApiError> {
    let mut m = Message::text(Role::Tool, "");
    m.content.clear();
    m.tool_call_id = Some(b.get("tool_use_id").and_then(Value::as_str).unwrap_or_default().to_owned());
    m.is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    match b.get("content") {
        Some(Value::String(s)) => m.content.push(Part::Text { text: s.clone() }),
        Some(Value::Array(a)) => {
            for (k, inner) in a.iter().enumerate() {
                let inner_path = format!("{path}.content.{k}");
                let ty = block_type(inner, &inner_path)?;
                m.content.push(block_part(inner, ty));
            }
        }
        _ => {}
    }
    Ok(m)
}

fn parse_tool(i: usize, t: &Value) -> Result<ToolDef, ApiError> {
    let Some(name) = t.get("name").and_then(Value::as_str) else {
        return Err(invalid(format!("tools.{i}.name: Field required")));
    };
    Ok(ToolDef {
        name: name.to_owned(),
        description: t.get("description").and_then(Value::as_str).map(str::to_owned),
        parameters: t.get("input_schema").filter(|v| !v.is_null()).cloned(),
        strict: None,
    })
}

/// The mapped choice and `parallel_tool_calls`, or `None` for a shape this does not know.
fn tool_choice(v: &Value) -> Option<(ToolChoice, Option<bool>)> {
    let choice = match v.get("type").and_then(Value::as_str)? {
        "auto" => ToolChoice::Auto,
        "any" => ToolChoice::Required,
        "none" => ToolChoice::None,
        "tool" => ToolChoice::Tool {
            name: v.get("name").and_then(Value::as_str)?.to_owned(),
        },
        _ => return None,
    };
    let parallel = (v.get("disable_parallel_tool_use").and_then(Value::as_bool) == Some(true)).then_some(false);
    Some((choice, parallel))
}

/// One content block of a reply, in wire order.
pub(crate) enum Block<'a> {
    Thinking { text: &'a str, signature: String },
    Text(String),
    ToolUse(&'a ToolCall),
}

/// The blocks of a reply: thinking, text, then one `tool_use` per call. The text block is present
/// when there is text, or when nothing else would be (a response never has empty `content`).
/// A refusal is text.
pub(crate) fn blocks<'a>(req: &Request, reply: &'a Reply) -> Vec<Block<'a>> {
    let mut out = Vec::new();
    if let Some(text) = reply.reasoning.as_deref().filter(|t| !t.is_empty()) {
        let signature = reply
            .reasoning_opaque
            .clone()
            .unwrap_or_else(|| fallback::reasoning_opaque(fallback::request_seed(req)));
        out.push(Block::Thinking { text, signature });
    }
    let mut text = reply.text.clone().unwrap_or_default();
    if let Some(r) = reply.refusal.as_deref().filter(|r| !r.is_empty()) {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(r);
    }
    if !text.is_empty() || (out.is_empty() && reply.tool_calls.is_empty()) {
        out.push(Block::Text(text));
    }
    out.extend(reply.tool_calls.iter().map(Block::ToolUse));
    out
}

/// Tool arguments as a JSON value; `{}` when they do not parse. Only the sync path uses this: the
/// stream sends the raw `partial_json` fragments, so cut-off arguments stay observable there.
pub(crate) fn tool_input(call: &ToolCall) -> Value {
    serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}))
}

/// The sync `message` object: `{id, type: "message", role: "assistant", model, content[],
/// stop_reason, stop_sequence: null, usage}`.
pub fn render_sync(req: &Request, reply: &Reply, meta: &MessageMeta) -> Value {
    let content: Vec<Value> = blocks(req, reply)
        .into_iter()
        .map(|b| match b {
            Block::Thinking { text, signature } => json!({"type": "thinking", "thinking": text, "signature": signature}),
            Block::Text(text) => json!({"type": "text", "text": text}),
            Block::ToolUse(c) => json!({"type": "tool_use", "id": c.id, "name": c.name, "input": tool_input(c)}),
        })
        .collect();
    json!({
        "id": meta.id,
        "type": "message",
        "role": "assistant",
        "model": meta.model,
        "content": content,
        "stop_reason": stop_reason(reply.finish),
        "stop_sequence": null,
        "usage": usage_json(&reply.usage),
    })
}

/// `Stop` to `end_turn`, `Length` to `max_tokens`, `ToolCalls` to `tool_use`,
/// `ContentFilter` to `refusal`.
pub fn stop_reason(finish: FinishReason) -> &'static str {
    match finish {
        FinishReason::Stop => "end_turn",
        FinishReason::Length => "max_tokens",
        FinishReason::ToolCalls => "tool_use",
        FinishReason::ContentFilter => "refusal",
    }
}

/// `{input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens}`.
/// `input_tokens` excludes cached tokens (see the spec).
pub fn usage_json(usage: &Usage) -> Value {
    json!({
        "input_tokens": input_tokens(usage),
        "cache_creation_input_tokens": usage.cache_creation_tokens,
        "cache_read_input_tokens": usage.cached_tokens,
        "cache_creation": cache_creation_json(usage.cache_creation_tokens),
        "output_tokens": usage.completion_tokens,
        "service_tier": SERVICE_TIER,
        "inference_geo": INFERENCE_GEO,
    })
}

pub(crate) const SERVICE_TIER: &str = "standard";
pub(crate) const INFERENCE_GEO: &str = "not_available";

/// `usage.cache_creation`: the written tokens split by lifetime; all of them count as the 5 minute kind.
pub(crate) fn cache_creation_json(tokens: u32) -> Value {
    json!({"ephemeral_5m_input_tokens": tokens, "ephemeral_1h_input_tokens": 0})
}

/// `message_delta.context_management`: `{"applied_edits": []}` when the request asked for context management,
/// else `null`.
pub(crate) fn context_management(req: &Request) -> Value {
    if req.extra.contains_key("context_management") {
        json!({"applied_edits": []})
    } else {
        Value::Null
    }
}

/// Prompt tokens that were neither read from nor written to the cache.
pub(crate) fn input_tokens(usage: &Usage) -> u32 {
    usage
        .prompt_tokens
        .saturating_sub(usage.cached_tokens)
        .saturating_sub(usage.cache_creation_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(body: Value, require: bool) -> String {
        let e = parse(&body, require).unwrap_err();
        assert_eq!(
            (e.status, e.kind.as_str(), e.param.as_deref()),
            (400, "invalid_request_error", None)
        );
        e.message
    }

    fn meta() -> MessageMeta {
        MessageMeta {
            id: "msg_1".into(),
            model: "claude-sonnet-4-5".into(),
        }
    }

    fn ai_sdk_request() -> Value {
        json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 4096,
            "temperature": 0.5,
            "top_k": 40,
            "stream": true,
            "stop_sequences": ["END"],
            "metadata": {"user_id": "u-1"},
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "system": [
                {"type": "text", "text": "You are terse.", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "Use tools."}
            ],
            "tools": [
                {"name": "get_weather", "description": "Weather", "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}},
                 "cache_control": {"type": "ephemeral"}},
                {"type": "web_search_20250305", "name": "web_search"}
            ],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "weather in Paris?", "cache_control": {"type": "ephemeral"}}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "Need the tool.", "signature": "sig-abc"},
                    {"type": "text", "text": "Checking."},
                    {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Paris"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "sunny"}], "is_error": true},
                    {"type": "text", "text": "<system-reminder>be brief</system-reminder>"}
                ]}
            ],
            "service_tier": "auto",
            "context_management": null
        })
    }

    #[test]
    fn parses_ai_sdk_request() {
        let req = parse(&ai_sdk_request(), true).unwrap();
        assert_eq!(req.protocol, Protocol::AnthropicMessages);
        assert_eq!(req.model, "claude-sonnet-4-5");
        assert_eq!(req.max_tokens, Some(4096));
        assert!(req.stream);
        assert_eq!(req.stop, vec!["END"]);
        assert_eq!(req.user.as_deref(), Some("u-1"));
        assert_eq!(req.temperature, Some(0.5));
        assert_eq!(
            req.thinking,
            Some(ThinkingConfig {
                kind: "enabled".into(),
                budget_tokens: Some(2048)
            })
        );
        assert_eq!(req.tool_choice, Some(ToolChoice::Auto));
        assert_eq!(req.parallel_tool_calls, Some(false));
        assert_eq!(req.extra["top_k"], 40);
        assert_eq!(req.extra["service_tier"], "auto");
        assert!(!req.extra.contains_key("context_management"));
        assert!(!req.extra.contains_key("tool_choice"));

        assert_eq!(req.tools.len(), 2);
        assert_eq!(req.tools[0].name, "get_weather");
        assert!(req.tools[0].parameters.is_some());
        assert_eq!(req.tools[1].parameters, None);

        let roles: Vec<Role> = req.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::System, Role::User, Role::Assistant, Role::Tool, Role::User]);
        assert_eq!(req.system_text(), "You are terse.\nUse tools.");

        let a = &req.messages[2];
        assert_eq!(a.reasoning.as_deref(), Some("Need the tool."));
        assert_eq!(a.reasoning_opaque.as_deref(), Some("sig-abc"));
        assert_eq!(a.text_content(), "Checking.");
        assert!(matches!(&a.content[0], Part::Thinking { signature: Some(s), .. } if s == "sig-abc"));
        assert_eq!(
            a.tool_calls,
            [ToolCall {
                id: "toolu_1".into(),
                name: "get_weather".into(),
                arguments: r#"{"city":"Paris"}"#.into()
            }]
        );

        let t = &req.messages[3];
        assert_eq!(t.tool_call_id.as_deref(), Some("toolu_1"));
        assert!(t.is_error);
        assert_eq!(t.text_content(), "sunny");
        assert_eq!(req.messages[4].text_content(), "<system-reminder>be brief</system-reminder>");

        assert!(req.ends_with_tool_result());
        let results = req.trailing_tool_results();
        assert_eq!(results[0].name, Some("get_weather"));
    }

    #[test]
    fn content_block_mapping() {
        let req = parse(
            &json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                {"type": "image", "source": {"type": "url", "url": "https://x/y.png"}},
                {"type": "document", "title": "doc.pdf", "source": {"type": "base64", "media_type": "application/pdf", "data": "BBBB"}},
                {"type": "redacted_thinking", "data": "zzz"},
                {"type": "search_result", "x": 1}
            ]}, {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "n"}]}]}),
            true,
        )
        .unwrap();
        let c = &req.messages[0].content;
        assert_eq!(
            c[0],
            Part::Image {
                url: "data:image/png;base64,AAAA".into(),
                detail: None
            }
        );
        assert_eq!(
            c[1],
            Part::Image {
                url: "https://x/y.png".into(),
                detail: None
            }
        );
        assert!(
            matches!(&c[2], Part::File { filename: Some(f), file_data: Some(d), .. } if f == "doc.pdf" && d.starts_with("data:application/pdf;base64,"))
        );
        assert_eq!(c[3], Part::RedactedThinking { data: "zzz".into() });
        assert!(matches!(&c[4], Part::Other { .. }));
        assert_eq!(req.messages[1].tool_calls[0].arguments, "{}");
    }

    #[test]
    fn splits_user_runs_in_order() {
        let req = parse(
            &json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": [
                {"type": "text", "text": "a"},
                {"type": "tool_result", "tool_use_id": "1", "content": "r1"},
                {"type": "tool_result", "tool_use_id": "2", "content": "r2"},
                {"type": "text", "text": "b"}
            ]}]}),
            true,
        )
        .unwrap();
        let roles: Vec<Role> = req.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::User, Role::Tool, Role::Tool, Role::User]);
    }

    #[test]
    fn validation_messages() {
        let ok = || json!({"model": "m", "max_tokens": 10, "messages": [{"role": "user", "content": "hi"}]});
        let with = |k: &str, v: Value| {
            let mut b = ok();
            b[k] = v;
            b
        };
        assert_eq!(err(json!([]), true), "Input should be a valid dictionary or object");
        assert_eq!(err(json!({"messages": []}), true), "model: Field required");
        assert_eq!(err(with("model", json!(1)), true), "model: Input should be a valid string");
        assert_eq!(err(with("messages", json!([])), true), "messages: at least one message is required");
        assert_eq!(err(with("messages", json!("x")), true), "messages: Input should be a valid list");
        assert_eq!(
            err(with("messages", json!([{"role": "tool", "content": "x"}])), true),
            "messages.0.role: Input should be 'user' or 'assistant'"
        );
        let mut b = ok();
        b.as_object_mut().unwrap().remove("max_tokens");
        assert_eq!(err(b.clone(), true), "max_tokens: Field required");
        assert!(parse(&b, false).is_ok());
        assert_eq!(
            err(with("max_tokens", json!("x")), true),
            "max_tokens: Input should be a valid integer"
        );
        assert_eq!(
            err(with("max_tokens", json!(0)), true),
            "max_tokens: Input should be greater than or equal to 1"
        );
        assert_eq!(
            err(with("thinking", json!({"type": "on"})), true),
            "thinking.type: Input should be 'enabled', 'adaptive' or 'disabled'"
        );
        assert_eq!(
            err(with("thinking", json!({"type": "enabled", "budget_tokens": 100})), true),
            "thinking.enabled.budget_tokens: Input should be greater than or equal to 1024"
        );
        assert_eq!(
            err(with("thinking", json!({"type": "enabled", "budget_tokens": 2000})), true),
            "thinking.enabled.budget_tokens: Input should be less than max_tokens"
        );
        assert_eq!(
            err(with("tools", json!([{"description": "x"}])), true),
            "tools.0.name: Field required"
        );
        assert_eq!(
            err(with("messages", json!([{"role": "user", "content": [{"text": "x"}]}])), true),
            "messages.0.content.0.type: Field required"
        );
        assert!(parse(&with("thinking", json!({"type": "adaptive"})), true).is_ok());
    }

    #[test]
    fn tool_choice_mapping() {
        let tc = |v: Value| {
            let mut b = json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}]});
            b["tool_choice"] = v;
            parse(&b, true).unwrap().tool_choice
        };
        assert_eq!(tc(json!({"type": "any"})), Some(ToolChoice::Required));
        assert_eq!(tc(json!({"type": "none"})), Some(ToolChoice::None));
        assert_eq!(
            tc(json!({"type": "tool", "name": "f"})),
            Some(ToolChoice::Tool { name: "f".into() })
        );
    }

    #[test]
    fn sync_text_shape() {
        let req = parse(
            &json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}]}),
            true,
        )
        .unwrap();
        let reply = Reply {
            text: Some("hello".into()),
            usage: Usage {
                prompt_tokens: 30,
                completion_tokens: 2,
                cached_tokens: 10,
                cache_creation_tokens: 5,
                ..Usage::default()
            },
            ..Reply::default()
        };
        let v = render_sync(&req, &reply, &meta());
        assert_eq!(
            v,
            json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
                "content": [{"type": "text", "text": "hello"}], "stop_reason": "end_turn", "stop_sequence": null,
                "usage": {"input_tokens": 15, "cache_creation_input_tokens": 5, "cache_read_input_tokens": 10,
                    "cache_creation": {"ephemeral_5m_input_tokens": 5, "ephemeral_1h_input_tokens": 0},
                    "output_tokens": 2, "service_tier": "standard", "inference_geo": "not_available"}})
        );
    }

    #[test]
    fn sync_thinking_and_tools() {
        let req = parse(
            &json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}]}),
            true,
        )
        .unwrap();
        let reply = Reply {
            reasoning: Some("hmm".into()),
            reasoning_opaque: Some("SIG".into()),
            text: Some(String::new()),
            tool_calls: vec![
                ToolCall {
                    id: "toolu_a".into(),
                    name: "f".into(),
                    arguments: r#"{"a":1}"#.into(),
                },
                ToolCall {
                    id: "toolu_b".into(),
                    name: "g".into(),
                    arguments: "{broken".into(),
                },
            ],
            finish: FinishReason::ToolCalls,
            ..Reply::default()
        };
        let v = render_sync(&req, &reply, &meta());
        assert_eq!(v["stop_reason"], "tool_use");
        assert_eq!(
            v["content"],
            json!([
                {"type": "thinking", "thinking": "hmm", "signature": "SIG"},
                {"type": "tool_use", "id": "toolu_a", "name": "f", "input": {"a": 1}},
                {"type": "tool_use", "id": "toolu_b", "name": "g", "input": {}}
            ])
        );
        let derived = render_sync(
            &req,
            &Reply {
                reasoning_opaque: None,
                ..reply
            },
            &meta(),
        );
        assert_eq!(
            derived["content"][0]["signature"],
            fallback::reasoning_opaque(fallback::request_seed(&req))
        );
    }

    #[test]
    fn empty_reply_has_an_empty_text_block() {
        let req = Request::new("m", vec![]);
        let v = render_sync(&req, &Reply::default(), &meta());
        assert_eq!(v["content"], json!([{"type": "text", "text": ""}]));
    }

    #[test]
    fn stop_reasons() {
        assert_eq!(stop_reason(FinishReason::Stop), "end_turn");
        assert_eq!(stop_reason(FinishReason::Length), "max_tokens");
        assert_eq!(stop_reason(FinishReason::ToolCalls), "tool_use");
        assert_eq!(stop_reason(FinishReason::ContentFilter), "refusal");
    }
}
