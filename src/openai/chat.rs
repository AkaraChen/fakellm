//! Chat Completions: request JSON to IR, and IR reply to the sync body or the chunk sequence.
//!
//! Wire fidelity rules the clients in `docs/research.md` depend on:
//! - sync body: `id`, `object: "chat.completion"`, `created`, `model`, `choices[]` with `index`,
//!   `message {role, content, refusal: null, annotations: [], tool_calls?}`, `logprobs: null`,
//!   `finish_reason`; `usage` with `prompt_tokens_details {cached_tokens, audio_tokens}` and
//!   `completion_tokens_details {reasoning_tokens, audio_tokens, accepted_prediction_tokens,
//!   rejected_prediction_tokens}`; `service_tier: "default"`, `system_fingerprint`.
//! - tool calls in a sync body carry `type: "function"` and `function {name, arguments}`.
//! - chunks: `object: "chat.completion.chunk"`; every chunk has `choices` (an empty array on the
//!   usage chunk) and every choice has `index`, `delta`, `logprobs: null`, `finish_reason`.
//! - the first chunk per choice is `delta: {role: "assistant", content: "", refusal: null}`.
//! - text streams one token per chunk (`Delivery::chunk_tokens` tokens, really).
//! - tool calls: first fragment `{index, id, type: "function", function: {name, arguments: ""}}`,
//!   then `{index, function: {arguments: "<piece>"}}` of `Delivery::args_chunk_bytes` bytes.
//! - exactly one finish chunk per choice: `delta: {}`, `finish_reason` set.
//! - when `include_usage`: every chunk has `usage: null` and one final chunk has `choices: []`
//!   and the `usage` object. Without it, chunks have no `usage` key at all.
//! - `n > 1` produces each choice's chunks back to back (choice 0 fully, then choice 1), each with its own role/finish chunks.

use serde_json::{Map, Value, json};

use crate::ir::{
    ApiError, FinishReason, Message, Part, Protocol, Reply, Request, ResponseFormat, Role, ToolCall, ToolChoice, ToolDef, Usage,
};

/// Identity shared by all chunks of one response.
#[derive(Clone, Debug)]
pub struct ResponseMeta {
    /// `chatcmpl-...`
    pub id: String,
    /// Unix seconds from the server clock.
    pub created: i64,
    /// Echoed from the request.
    pub model: String,
    /// `fp_...`
    pub system_fingerprint: String,
}

fn bad_type(param: &str, expected: &str) -> ApiError {
    ApiError::invalid_request(format!("Invalid type for '{param}': expected {expected}."))
        .code("invalid_type")
        .param(param)
}

/// `None` for absent or `null`.
fn field<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    obj.get(key).filter(|v| !v.is_null())
}

fn opt_str(obj: &Map<String, Value>, key: &str, param: &str) -> Result<Option<String>, ApiError> {
    match field(obj, key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(bad_type(param, "a string")),
    }
}

fn opt_bool(obj: &Map<String, Value>, key: &str) -> Result<Option<bool>, ApiError> {
    match field(obj, key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(bad_type(key, "a boolean")),
    }
}

fn opt_f64(obj: &Map<String, Value>, key: &str) -> Result<Option<f64>, ApiError> {
    match field(obj, key) {
        None => Ok(None),
        Some(v) => v.as_f64().map(Some).ok_or_else(|| bad_type(key, "a number")),
    }
}

fn opt_u32(obj: &Map<String, Value>, key: &str) -> Result<Option<u32>, ApiError> {
    match field(obj, key) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| bad_type(key, "a non-negative integer")),
    }
}

/// Parses a Chat Completions request body. Errors mirror the real API: missing `model` is
/// `"you must provide a model parameter"` (no param), missing/empty `messages` is
/// `missing_required_parameter` with `param: "messages"`, a message with an unknown role or
/// without content is a 400 with `param: "messages[i].role"` / `"messages[i].content"`.
/// Unknown top-level fields go to `Request::extra`; unknown content part types become `Part::Other`.
pub fn parse(body: &Value) -> Result<Request, ApiError> {
    let Some(obj) = body.as_object() else {
        return Err(ApiError::invalid_request("Invalid request body: expected a JSON object."));
    };
    let model = match field(obj, "model") {
        None => return Err(ApiError::invalid_request("you must provide a model parameter")),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(bad_type("model", "a string")),
    };
    let messages = match field(obj, "messages") {
        None => return Err(ApiError::missing_parameter("messages")),
        Some(Value::Array(a)) if a.is_empty() => return Err(ApiError::missing_parameter("messages")),
        Some(Value::Array(a)) => a
            .iter()
            .enumerate()
            .map(|(i, m)| parse_message(i, m))
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(bad_type("messages", "an array")),
    };
    let mut req = Request::new(model, messages);
    req.protocol = Protocol::OpenaiChat;

    let mut extra = Map::new();
    for (k, v) in obj {
        if !matches!(k.as_str(), "model" | "messages") && !v.is_null() {
            extra.insert(k.clone(), v.clone());
        }
    }

    if let Some(Value::Array(tools)) = take(&mut extra, "tools") {
        for (i, t) in tools.iter().enumerate() {
            if let Some(def) = parse_tool(i, t)? {
                req.tools.push(def);
            }
        }
    } else if obj.get("tools").is_some_and(|v| !v.is_null()) {
        return Err(bad_type("tools", "an array"));
    }

    if let Some(tc) = take(&mut extra, "tool_choice") {
        match parse_tool_choice(&tc)? {
            Some(c) => req.tool_choice = Some(c),
            None => {
                extra.insert("tool_choice".into(), tc);
            }
        }
    }

    req.parallel_tool_calls = opt_bool(obj, "parallel_tool_calls")?;
    take(&mut extra, "parallel_tool_calls");
    req.stream = opt_bool(obj, "stream")?.unwrap_or(false);
    take(&mut extra, "stream");
    if let Some(so) = take(&mut extra, "stream_options") {
        req.include_usage = so.get("include_usage").and_then(Value::as_bool).unwrap_or(false);
    }

    let max_completion = opt_u32(obj, "max_completion_tokens")?;
    let max_tokens = opt_u32(obj, "max_tokens")?;
    take(&mut extra, "max_completion_tokens");
    if max_completion.is_some() {
        req.max_tokens = max_completion;
    } else {
        req.max_tokens = max_tokens;
        take(&mut extra, "max_tokens");
    }
    for (name, v) in [("max_completion_tokens", max_completion), ("max_tokens", max_tokens)] {
        if v == Some(0) {
            return Err(ApiError::invalid_request(format!(
                "Invalid '{name}': integer below minimum value. Expected a value >= 1, but got 0 instead."
            ))
            .code("integer_below_min_value")
            .param(name));
        }
    }
    if let Some(n) = opt_u32(obj, "n")? {
        if n == 0 {
            return Err(
                ApiError::invalid_request("Invalid 'n': integer below minimum value. Expected a value >= 1, but got 0 instead.")
                    .code("integer_below_min_value")
                    .param("n"),
            );
        }
        req.n = n;
    }
    take(&mut extra, "n");
    req.temperature = opt_f64(obj, "temperature")?;
    take(&mut extra, "temperature");
    req.top_p = opt_f64(obj, "top_p")?;
    take(&mut extra, "top_p");
    req.seed = match field(obj, "seed") {
        None => None,
        Some(v) => Some(v.as_i64().ok_or_else(|| bad_type("seed", "an integer"))?),
    };
    take(&mut extra, "seed");
    match take(&mut extra, "stop") {
        None => {}
        Some(Value::String(s)) => req.stop = vec![s],
        Some(Value::Array(a)) => {
            for s in a {
                req.stop.push(
                    s.as_str()
                        .ok_or_else(|| bad_type("stop", "a string or an array of strings"))?
                        .to_owned(),
                );
            }
        }
        Some(_) => return Err(bad_type("stop", "a string or an array of strings")),
    }
    if let Some(rf) = take(&mut extra, "response_format") {
        req.response_format = Some(parse_response_format(&rf)?);
    }
    req.reasoning_effort = opt_str(obj, "reasoning_effort", "reasoning_effort")?;
    take(&mut extra, "reasoning_effort");
    req.store = opt_bool(obj, "store")?;
    take(&mut extra, "store");
    req.user = opt_str(obj, "user", "user")?;
    take(&mut extra, "user");
    match take(&mut extra, "metadata") {
        None => {}
        Some(Value::Object(m)) => req.metadata = m,
        Some(_) => return Err(bad_type("metadata", "an object")),
    }
    req.extra = extra;
    Ok(req)
}

fn take(extra: &mut Map<String, Value>, key: &str) -> Option<Value> {
    extra.shift_remove(key)
}

fn parse_response_format(v: &Value) -> Result<ResponseFormat, ApiError> {
    match v.get("type").and_then(Value::as_str) {
        Some("text") => Ok(ResponseFormat::Text),
        Some("json_object") => Ok(ResponseFormat::JsonObject),
        Some("json_schema") => {
            let js = v.get("json_schema");
            Ok(ResponseFormat::JsonSchema {
                name: js.and_then(|j| j.get("name")).and_then(Value::as_str).map(str::to_owned),
                schema: js.and_then(|j| j.get("schema")).cloned(),
                strict: js.and_then(|j| j.get("strict")).and_then(Value::as_bool),
            })
        }
        _ => Err(ApiError::invalid_request("Invalid 'response_format': unknown type.")
            .code("invalid_value")
            .param("response_format.type")),
    }
}

fn parse_tool(i: usize, t: &Value) -> Result<Option<ToolDef>, ApiError> {
    let Some(obj) = t.as_object() else {
        return Err(bad_type(&format!("tools[{i}]"), "an object"));
    };
    let kind = obj
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| obj.contains_key("function").then_some("function"));
    if kind != Some("function") {
        return Ok(None);
    }
    let Some(f) = obj.get("function").and_then(Value::as_object) else {
        return Err(ApiError::missing_parameter(&format!("tools[{i}].function")));
    };
    let Some(name) = f.get("name").and_then(Value::as_str) else {
        return Err(ApiError::missing_parameter(&format!("tools[{i}].function.name")));
    };
    Ok(Some(ToolDef {
        name: name.to_owned(),
        description: f.get("description").and_then(Value::as_str).map(str::to_owned),
        parameters: field(f, "parameters").cloned(),
        strict: f.get("strict").and_then(Value::as_bool),
    }))
}

/// `Ok(None)` for shapes this server does not model (e.g. `allowed_tools`); the caller keeps them in `extra`.
fn parse_tool_choice(v: &Value) -> Result<Option<ToolChoice>, ApiError> {
    match v {
        Value::String(s) => match s.as_str() {
            "auto" => Ok(Some(ToolChoice::Auto)),
            "none" => Ok(Some(ToolChoice::None)),
            "required" => Ok(Some(ToolChoice::Required)),
            other => Err(ApiError::invalid_request(format!(
                "Invalid value: '{other}'. Supported values are: 'none', 'auto', and 'required'."
            ))
            .code("invalid_value")
            .param("tool_choice")),
        },
        Value::Object(o) => Ok(o
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .filter(|_| o.get("type").and_then(Value::as_str) == Some("function"))
            .map(|name| ToolChoice::Tool { name: name.to_owned() })),
        _ => Err(bad_type("tool_choice", "a string or an object")),
    }
}

fn parse_message(i: usize, m: &Value) -> Result<Message, ApiError> {
    let Some(obj) = m.as_object() else {
        return Err(bad_type(&format!("messages[{i}]"), "an object"));
    };
    let role_param = format!("messages[{i}].role");
    let role = match obj.get("role").and_then(Value::as_str) {
        None => return Err(ApiError::missing_parameter(&role_param)),
        Some("system") => Role::System,
        Some("developer") => Role::Developer,
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some("tool" | "function") => Role::Tool,
        Some(other) => {
            return Err(ApiError::invalid_request(format!(
                "Invalid value: '{other}'. Supported values are: 'system', 'assistant', 'user', 'function', 'tool', and 'developer'."
            ))
            .code("invalid_value")
            .param(role_param));
        }
    };
    let content_param = format!("messages[{i}].content");
    let mut tool_calls = Vec::new();
    if let Some(tcs) = field(obj, "tool_calls") {
        let Some(arr) = tcs.as_array() else {
            return Err(bad_type(&format!("messages[{i}].tool_calls"), "an array"));
        };
        for (j, tc) in arr.iter().enumerate() {
            let at = format!("messages[{i}].tool_calls[{j}]");
            let f = tc
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| ApiError::missing_parameter(&format!("{at}.function")))?;
            let arguments = match f.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                None | Some(Value::Null) => String::new(),
                Some(other) => other.to_string(),
            };
            tool_calls.push(ToolCall {
                id: tc.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                name: f.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
                arguments,
            });
        }
    }
    if let Some(fc) = field(obj, "function_call").and_then(Value::as_object) {
        tool_calls.push(ToolCall {
            id: String::new(),
            name: fc.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
            arguments: match fc.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                None | Some(Value::Null) => String::new(),
                Some(other) => other.to_string(),
            },
        });
    }
    let content = match field(obj, "content") {
        Some(Value::String(s)) => vec![Part::Text { text: s.clone() }],
        Some(Value::Array(parts)) => parts.iter().map(parse_part).collect(),
        Some(_) => return Err(bad_type(&content_param, "a string or an array")),
        None if role == Role::Assistant && (!tool_calls.is_empty() || field(obj, "refusal").is_some()) => Vec::new(),
        None => return Err(ApiError::missing_parameter(&content_param)),
    };
    let mut content = content;
    if let Some(Value::String(r)) = field(obj, "refusal") {
        content.push(Part::Refusal { text: r.clone() });
    }
    let reasoning = ["reasoning_content", "reasoning"]
        .iter()
        .find_map(|k| field(obj, k).and_then(Value::as_str))
        .map(str::to_owned);
    Ok(Message {
        role,
        name: field(obj, "name").and_then(Value::as_str).map(str::to_owned),
        content,
        tool_calls,
        tool_call_id: field(obj, "tool_call_id").and_then(Value::as_str).map(str::to_owned),
        reasoning,
    })
}

fn parse_part(p: &Value) -> Part {
    let s = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_owned);
    match p.get("type").and_then(Value::as_str) {
        Some("text") => Part::Text {
            text: s(p.get("text")).unwrap_or_default(),
        },
        Some("refusal") => Part::Refusal {
            text: s(p.get("refusal")).unwrap_or_default(),
        },
        Some("image_url") => {
            let iu = p.get("image_url");
            // Some clients send the URL as a bare string.
            let url = match iu {
                Some(Value::String(u)) => Some(u.clone()),
                _ => s(iu.and_then(|i| i.get("url"))),
            };
            match url {
                Some(url) => Part::Image {
                    url,
                    detail: s(iu.and_then(|i| i.get("detail"))),
                },
                None => Part::Other { value: p.clone() },
            }
        }
        Some("input_audio") => match s(p.get("input_audio").and_then(|a| a.get("data"))) {
            Some(data) => Part::Audio {
                data,
                format: s(p.get("input_audio").and_then(|a| a.get("format"))),
            },
            None => Part::Other { value: p.clone() },
        },
        Some("file") => {
            let f = p.get("file");
            Part::File {
                file_id: s(f.and_then(|f| f.get("file_id"))),
                filename: s(f.and_then(|f| f.get("filename"))),
                file_data: s(f.and_then(|f| f.get("file_data"))),
            }
        }
        _ => Part::Other { value: p.clone() },
    }
}

/// Completion and reasoning tokens are summed over the `n` choices, like the real API.
fn usage_json(u: &Usage, n: u32) -> Value {
    let n = n.max(1);
    let completion = u.completion_tokens.saturating_mul(n);
    let reasoning = u.reasoning_tokens.saturating_mul(n);
    json!({
        "prompt_tokens": u.prompt_tokens,
        "completion_tokens": completion,
        "total_tokens": u.prompt_tokens.saturating_add(completion),
        "prompt_tokens_details": {"cached_tokens": u.cached_tokens, "audio_tokens": 0},
        "completion_tokens_details": {
            "reasoning_tokens": reasoning,
            "audio_tokens": 0,
            "accepted_prediction_tokens": 0,
            "rejected_prediction_tokens": 0,
        },
    })
}

/// The sync `chat.completion` object. `n` choices get the same reply.
pub fn render_sync(req: &Request, reply: &Reply, meta: &ResponseMeta) -> Value {
    let choices: Vec<Value> = (0..req.n.max(1))
        .map(|index| {
            let mut message = Map::new();
            message.insert("role".into(), "assistant".into());
            message.insert("content".into(), reply.text.clone().map_or(Value::Null, Value::String));
            if let Some(r) = reply.reasoning.as_ref().filter(|r| !r.is_empty()) {
                message.insert("reasoning_content".into(), r.clone().into());
            }
            message.insert("refusal".into(), reply.refusal.clone().map_or(Value::Null, Value::String));
            message.insert("annotations".into(), json!([]));
            if !reply.tool_calls.is_empty() {
                let calls: Vec<Value> = reply
                    .tool_calls
                    .iter()
                    .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}}))
                    .collect();
                message.insert("tool_calls".into(), calls.into());
            }
            json!({"index": index, "message": message, "logprobs": null, "finish_reason": reply.finish.as_str()})
        })
        .collect();
    json!({
        "id": meta.id,
        "object": "chat.completion",
        "created": meta.created,
        "model": meta.model,
        "choices": choices,
        "usage": usage_json(&reply.usage, req.n),
        "service_tier": "default",
        "system_fingerprint": meta.system_fingerprint,
    })
}

/// What a chunk carries, so the streamer knows which ones take a token interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkKind {
    /// The role chunk; sent at `ttft` together with the first content chunk.
    Role,
    /// Text or tool-call fragment; one `token_interval` apart.
    Content,
    /// Finish and usage chunks; sent right after the previous chunk.
    Trailer,
}

/// The whole chunk sequence for a streamed reply, in order, without the `[DONE]` sentinel.
pub fn chunks(req: &Request, reply: &Reply, meta: &ResponseMeta, chunk_tokens: usize, args_chunk_bytes: usize) -> Vec<(ChunkKind, Value)> {
    let chunk_tokens = chunk_tokens.max(1);
    let args_chunk_bytes = args_chunk_bytes.max(1);
    let wrap = |choices: Vec<Value>| -> Map<String, Value> {
        let mut o = Map::new();
        o.insert("id".into(), meta.id.clone().into());
        o.insert("object".into(), "chat.completion.chunk".into());
        o.insert("created".into(), meta.created.into());
        o.insert("model".into(), meta.model.clone().into());
        o.insert("service_tier".into(), "default".into());
        o.insert("system_fingerprint".into(), meta.system_fingerprint.clone().into());
        o.insert("choices".into(), choices.into());
        if req.include_usage {
            o.insert("usage".into(), Value::Null);
        }
        o
    };
    let delta_chunk = |index: u32, delta: Value, finish: Option<FinishReason>| {
        Value::Object(wrap(vec![json!({
            "index": index,
            "delta": delta,
            "logprobs": null,
            "finish_reason": finish.map(FinishReason::as_str),
        })]))
    };

    let mut out = Vec::new();
    for index in 0..req.n.max(1) {
        out.push((
            ChunkKind::Role,
            delta_chunk(index, json!({"role": "assistant", "content": "", "refusal": null}), None),
        ));
        if let Some(r) = reply.reasoning.as_deref().filter(|r| !r.is_empty()) {
            for group in crate::tokenizer::tokenize(r).chunks(chunk_tokens) {
                out.push((
                    ChunkKind::Content,
                    delta_chunk(index, json!({"reasoning_content": group.concat()}), None),
                ));
            }
        }
        if let Some(text) = reply.text.as_deref() {
            for group in crate::tokenizer::tokenize(text).chunks(chunk_tokens) {
                out.push((ChunkKind::Content, delta_chunk(index, json!({"content": group.concat()}), None)));
            }
        }
        if let Some(refusal) = reply.refusal.as_deref().filter(|r| !r.is_empty()) {
            out.push((ChunkKind::Content, delta_chunk(index, json!({"refusal": refusal}), None)));
        }
        for (k, call) in reply.tool_calls.iter().enumerate() {
            let head = json!({"tool_calls": [{
                "index": k,
                "id": call.id,
                "type": "function",
                "function": {"name": call.name, "arguments": ""},
            }]});
            out.push((ChunkKind::Content, delta_chunk(index, head, None)));
            for frag in split_bytes(&call.arguments, args_chunk_bytes) {
                let d = json!({"tool_calls": [{"index": k, "function": {"arguments": frag}}]});
                out.push((ChunkKind::Content, delta_chunk(index, d, None)));
            }
        }
        out.push((ChunkKind::Trailer, delta_chunk(index, json!({}), Some(reply.finish))));
    }
    if req.include_usage {
        let mut o = wrap(Vec::new());
        o.insert("usage".into(), usage_json(&reply.usage, req.n));
        out.push((ChunkKind::Trailer, Value::Object(o)));
    }
    out
}

/// Pieces of at most `max` bytes, split on char boundaries (a char wider than `max` gets its own piece).
fn split_bytes(s: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let mut end = max.min(rest.len());
        while !rest.is_char_boundary(end) {
            end += 1;
        }
        let (a, b) = rest.split_at(end);
        out.push(a);
        rest = b;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Usage;

    fn meta() -> ResponseMeta {
        ResponseMeta {
            id: "chatcmpl-1".into(),
            created: 1_700_000_000,
            model: "gpt-4o".into(),
            system_fingerprint: "fp_x".into(),
        }
    }

    fn reply(text: Option<&str>, calls: Vec<ToolCall>, finish: FinishReason) -> Reply {
        Reply {
            text: text.map(str::to_owned),
            tool_calls: calls,
            reasoning: None,
            refusal: None,
            finish,
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                reasoning_tokens: 2,
                cached_tokens: 1,
            },
        }
    }

    #[test]
    fn parses_ai_sdk_body() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA", "detail": "low"}},
                    {"type": "input_audio", "input_audio": {"data": "QQ==", "format": "wav"}},
                    {"type": "file", "file": {"filename": "a.pdf", "file_data": "xx"}},
                    {"type": "weird", "x": 1}
                ]},
                {"role": "assistant", "content": null, "reasoning_content": "hmm", "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "ok"}
            ],
            "tools": [{"type": "function", "function": {"name": "f", "description": "d", "parameters": {"type": "object"}, "strict": true}}],
            "tool_choice": {"type": "function", "function": {"name": "f"}},
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_tokens": 5,
            "max_completion_tokens": 9,
            "stop": "x",
            "temperature": 0.5,
            "response_format": {"type": "json_schema", "json_schema": {"name": "s", "schema": {"type": "object"}, "strict": true}},
            "parallel_tool_calls": false,
            "metadata": {"a": "b"},
            "prompt_cache_key": "k"
        });
        let req = parse(&body).unwrap();
        assert_eq!(req.model, "gpt-4o");
        assert_eq!(req.messages.len(), 4);
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[1].content.len(), 5);
        assert!(matches!(&req.messages[1].content[1], Part::Image { detail: Some(d), .. } if d == "low"));
        assert!(matches!(&req.messages[1].content[2], Part::Audio { format: Some(f), .. } if f == "wav"));
        assert!(matches!(&req.messages[1].content[3], Part::File { filename: Some(f), .. } if f == "a.pdf"));
        assert!(matches!(&req.messages[1].content[4], Part::Other { .. }));
        assert!(req.messages[2].content.is_empty());
        assert_eq!(req.messages[2].reasoning.as_deref(), Some("hmm"));
        assert_eq!(req.messages[2].tool_calls[0].arguments, "{\"a\":1}");
        assert_eq!(req.messages[3].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(req.tools[0].name, "f");
        assert_eq!(req.tools[0].strict, Some(true));
        assert_eq!(req.tool_choice, Some(ToolChoice::Tool { name: "f".into() }));
        assert!(req.stream && req.include_usage);
        assert_eq!(req.max_tokens, Some(9));
        assert_eq!(req.n, 1);
        assert_eq!(req.stop, vec!["x"]);
        assert_eq!(req.temperature, Some(0.5));
        assert_eq!(req.parallel_tool_calls, Some(false));
        assert_eq!(req.metadata["a"], "b");
        assert!(matches!(
            req.response_format,
            Some(ResponseFormat::JsonSchema { strict: Some(true), .. })
        ));
        assert_eq!(req.extra["prompt_cache_key"], "k");
        assert_eq!(req.extra["max_tokens"], 5);
        assert!(!req.extra.contains_key("stream"));
    }

    #[test]
    fn parse_errors() {
        let e = parse(&json!({"messages": [{"role": "user", "content": "x"}]})).unwrap_err();
        assert_eq!(
            (e.status, e.message.as_str(), e.param),
            (400, "you must provide a model parameter", None)
        );
        let e = parse(&json!({"model": "m"})).unwrap_err();
        assert_eq!(e.code.as_deref(), Some("missing_required_parameter"));
        assert_eq!(e.param.as_deref(), Some("messages"));
        let e = parse(&json!({"model": "m", "messages": "hi"})).unwrap_err();
        assert_eq!(e.param.as_deref(), Some("messages"));
        let e = parse(&json!({"model": "m", "messages": [{"role": "user"}]})).unwrap_err();
        assert_eq!(e.param.as_deref(), Some("messages[0].content"));
        let e = parse(&json!({"model": "m", "messages": [{"role": "boss", "content": "x"}]})).unwrap_err();
        assert_eq!(e.param.as_deref(), Some("messages[0].role"));
        assert!(parse(&json!([1])).is_err());
        let req = parse(&json!({"model": "m", "messages": [{"role": "function", "content": "x"}], "stop": ["a", "b"], "n": 2})).unwrap();
        assert_eq!(req.messages[0].role, Role::Tool);
        assert_eq!((req.stop.len(), req.n), (2, 2));
    }

    #[test]
    fn sync_shape() {
        let req = Request::new("gpt-4o", vec![Message::text(Role::User, "hi")]);
        let call = ToolCall {
            id: "c".into(),
            name: "f".into(),
            arguments: "{}".into(),
        };
        let v = render_sync(&req, &reply(None, vec![call], FinishReason::ToolCalls), &meta());
        assert_eq!(v["object"], "chat.completion");
        let msg = &v["choices"][0]["message"];
        assert!(msg["content"].is_null() && msg["refusal"].is_null());
        assert_eq!(msg["annotations"], json!([]));
        assert_eq!(msg["tool_calls"][0]["type"], "function");
        assert_eq!(msg["tool_calls"][0]["function"]["arguments"], "{}");
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(v["usage"]["total_tokens"], 15);
        assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 1);
        assert_eq!(v["usage"]["completion_tokens_details"]["reasoning_tokens"], 2);
        assert_eq!(v["service_tier"], "default");

        let mut req = req;
        req.n = 2;
        let v = render_sync(&req, &reply(Some("yo"), vec![], FinishReason::Stop), &meta());
        assert_eq!(v["choices"].as_array().unwrap().len(), 2);
        assert_eq!(v["choices"][1]["index"], 1);
        assert_eq!(v["choices"][1]["message"]["content"], "yo");
        assert!(v["choices"][1]["message"].get("tool_calls").is_none());
    }

    #[test]
    fn stream_text_and_usage() {
        let mut req = Request::new("gpt-4o", vec![Message::text(Role::User, "hi")]);
        req.stream = true;
        req.include_usage = true;
        let text = "Hello there, world! How are you?";
        let cs = chunks(&req, &reply(Some(text), vec![], FinishReason::Stop), &meta(), 1, 16);
        assert_eq!(cs[0].0, ChunkKind::Role);
        assert_eq!(
            cs[0].1["choices"][0]["delta"],
            json!({"role": "assistant", "content": "", "refusal": null})
        );
        let joined: String = cs
            .iter()
            .filter_map(|(_, v)| v["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(joined, text);
        for (_, v) in &cs {
            assert!(v.get("usage").is_some());
            assert_eq!(v["object"], "chat.completion.chunk");
            assert_eq!(v["service_tier"], "default");
        }
        let n = cs.len();
        assert_eq!(cs[n - 2].0, ChunkKind::Trailer);
        assert_eq!(cs[n - 2].1["choices"][0]["finish_reason"], "stop");
        assert_eq!(cs[n - 2].1["choices"][0]["delta"], json!({}));
        assert_eq!(cs[n - 1].1["choices"], json!([]));
        assert_eq!(cs[n - 1].1["usage"]["completion_tokens"], 5);
        assert_eq!(cs.iter().filter(|(_, v)| v["choices"][0]["finish_reason"].is_string()).count(), 1);

        req.include_usage = false;
        let cs = chunks(&req, &reply(Some("a"), vec![], FinishReason::Stop), &meta(), 3, 16);
        assert!(cs.iter().all(|(_, v)| v.get("usage").is_none()));
        assert_eq!(cs.last().unwrap().0, ChunkKind::Trailer);
    }

    #[test]
    fn stream_empty_and_tool_calls() {
        let mut req = Request::new("gpt-4o", vec![Message::text(Role::User, "hi")]);
        let cs = chunks(&req, &reply(Some(""), vec![], FinishReason::Stop), &meta(), 1, 16);
        assert_eq!(cs.len(), 2);
        let args = "{\"city\":\"Zürich\",\"unit\":\"celsius\"}";
        let calls = vec![
            ToolCall {
                id: "c0".into(),
                name: "a".into(),
                arguments: args.into(),
            },
            ToolCall {
                id: "c1".into(),
                name: "b".into(),
                arguments: String::new(),
            },
        ];
        req.n = 2;
        let cs = chunks(&req, &reply(None, calls, FinishReason::ToolCalls), &meta(), 1, 5);
        let mut rebuilt = [String::new(), String::new()];
        let mut heads = 0;
        for (_, v) in cs.iter().filter(|(_, v)| v["choices"][0]["index"] == 0) {
            let Some(tc) = v["choices"][0]["delta"]["tool_calls"].get(0) else {
                continue;
            };
            let k = tc["index"].as_u64().unwrap() as usize;
            if tc.get("id").is_some() {
                heads += 1;
                assert_eq!(tc["type"], "function");
                assert_eq!(tc["function"]["arguments"], "");
            } else {
                assert!(tc["function"]["arguments"].as_str().unwrap().len() <= 8);
            }
            rebuilt[k].push_str(tc["function"]["arguments"].as_str().unwrap());
        }
        assert_eq!(heads, 2);
        assert_eq!(rebuilt[0], args);
        assert_eq!(rebuilt[1], "");
        let finishes: Vec<_> = cs
            .iter()
            .filter(|(_, v)| v["choices"][0]["finish_reason"] == "tool_calls")
            .collect();
        assert_eq!(finishes.len(), 2);
        assert_eq!(finishes[1].1["choices"][0]["index"], 1);
    }

    #[test]
    fn stream_reasoning_before_text() {
        let req = Request::new("m", vec![Message::text(Role::User, "hi")]);
        let mut r = reply(Some("answer"), vec![], FinishReason::Stop);
        r.reasoning = Some("think hard".into());
        let cs = chunks(&req, &r, &meta(), 1, 16);
        let first_reasoning = cs
            .iter()
            .position(|(_, v)| v["choices"][0]["delta"].get("reasoning_content").is_some())
            .unwrap();
        let first_text = cs
            .iter()
            .position(|(_, v)| v["choices"][0]["delta"].get("content").is_some_and(|c| c == "answer"))
            .unwrap();
        assert!(first_reasoning < first_text);
    }
}
