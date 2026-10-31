//! Responses API: request JSON to IR (resolving `previous_response_id` from the store), and an
//! IR reply to the `response` object, the stored form and the output items. Wire rules are in
//! docs/m1-spec.md ("OpenAI Responses") and the codex section of docs/research.md.
//!
//! The Responses input is a flat list of items, while the IR is a list of messages. Assistant-side
//! items (reasoning, assistant message, function call) are merged into one `Assistant` message so
//! `Request::assistant_turns()` counts model turns the same way it does for Chat. Clients are
//! tested on what they get back, not on what we do with unknown items, so unknown item types are
//! kept (as `Part::Other`) rather than rejected.

use serde_json::{Map, Value, json};

use crate::fallback;
use crate::ir::{ApiError, FinishReason, Message, Part, Protocol, Reply, Request, ResponseFormat, Role, ToolCall, ToolChoice, ToolDef};
use crate::openai::responses_store::{ResponseStore, StoredResponse};

/// Identity shared by every event of one response.
#[derive(Clone, Debug)]
pub struct ResponseMeta {
    /// `resp_...`, from `fallback::responses_id("resp", seed, 0)`.
    pub id: String,
    /// Unix seconds from the server clock.
    pub created: i64,
    /// Echoed from the request.
    pub model: String,
    /// Seed that item ids (`rs_`, `msg_`, `fc_`) and the default `encrypted_content` derive from:
    /// the request seed mixed with the per-server counter, so retries get fresh ids.
    pub seed: u64,
}

/// Fields the response object echoes, so they stay in `Request::extra` even when captured.
const MIRRORED: &[&str] = &[
    "instructions",
    "tools",
    "text",
    "reasoning",
    "include",
    "service_tier",
    "truncation",
    "prompt_cache_key",
    "safety_identifier",
    "client_metadata",
];

/// Fields captured into typed IR fields and therefore removed from `extra`.
const CAPTURED: &[&str] = &[
    "model",
    "input",
    "max_output_tokens",
    "parallel_tool_calls",
    "stream",
    "temperature",
    "top_p",
    "store",
    "previous_response_id",
    "metadata",
    "user",
];

fn bad_type(param: &str, expected: &str) -> ApiError {
    ApiError::invalid_request(format!("Invalid type for '{param}': expected {expected}."))
        .code("invalid_type")
        .param(param)
}

fn field<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    obj.get(key).filter(|v| !v.is_null())
}

fn opt_str(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, ApiError> {
    match field(obj, key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(bad_type(key, "a string")),
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

fn s(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_owned)
}

/// Parses a Responses request body into the IR (`Protocol::OpenaiResponses`).
///
/// `previous_response_id` is resolved against `store` here: the stored conversation is prepended
/// to the new input, and a missing id is a 404 (see [`previous_not_found`]). The system message
/// made from `instructions` goes first of all. Full field mapping in the spec.
pub fn parse(body: &Value, store: &ResponseStore) -> Result<Request, ApiError> {
    let Some(obj) = body.as_object() else {
        return Err(ApiError::invalid_request("Invalid request body: expected a JSON object."));
    };
    let model = match field(obj, "model") {
        None => return Err(ApiError::missing_parameter("model")),
        Some(Value::String(m)) => m.clone(),
        Some(_) => return Err(bad_type("model", "a string")),
    };
    let previous = opt_str(obj, "previous_response_id")?;
    let input = field(obj, "input");
    if input.is_none() && previous.is_none() {
        return Err(ApiError::missing_parameter("input"));
    }
    if input.is_some_and(|i| !i.is_string() && !i.is_array()) {
        return Err(bad_type("input", "a string or array"));
    }
    let max_tokens = match field(obj, "max_output_tokens") {
        None => None,
        Some(v) => {
            let Some(n) = v.as_i64() else {
                return Err(bad_type("max_output_tokens", "an integer"));
            };
            if n < 16 {
                return Err(ApiError::invalid_request(format!(
                    "Invalid 'max_output_tokens': integer below minimum value. Expected a value >= 16, but got {n} instead."
                ))
                .code("integer_below_min_value")
                .param("max_output_tokens"));
            }
            Some(u32::try_from(n).unwrap_or(u32::MAX))
        }
    };
    let instructions = opt_str(obj, "instructions")?;

    let mut messages = Vec::new();
    if let Some(text) = instructions.as_deref().filter(|t| !t.is_empty()) {
        messages.push(Message::text(Role::System, text));
    }
    if let Some(id) = &previous {
        let stored = store.get(id).ok_or_else(|| previous_not_found(id))?;
        messages.extend(stored.conversation.iter().cloned());
    }

    let mut extra = Map::new();
    for (k, v) in obj {
        if !v.is_null() && (MIRRORED.contains(&k.as_str()) || !CAPTURED.contains(&k.as_str())) {
            extra.insert(k.clone(), v.clone());
        }
    }
    extra.shift_remove("tool_choice");

    match input {
        Some(Value::String(t)) => messages.push(Message::text(Role::User, t.clone())),
        Some(Value::Array(items)) => parse_items(items, &mut messages, &mut extra),
        _ => {}
    }

    let tools_value = field(obj, "tools");
    let mut tools = Vec::new();
    if let Some(t) = tools_value {
        let Some(list) = t.as_array() else {
            return Err(bad_type("tools", "an array"));
        };
        tools = list.iter().filter_map(parse_tool).collect();
    }
    let tool_choice = match field(obj, "tool_choice") {
        None => None,
        Some(v) => {
            let mapped = parse_tool_choice(v);
            if mapped.is_none() {
                extra.insert("tool_choice".into(), v.clone());
            }
            mapped
        }
    };

    let text = field(obj, "text").and_then(Value::as_object);
    let response_format = text.and_then(|t| t.get("format")).and_then(parse_format);
    let reasoning_effort = field(obj, "reasoning").and_then(|r| s(r.get("effort")));
    let metadata = match field(obj, "metadata") {
        None => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err(bad_type("metadata", "an object")),
    };

    let mut req = Request::new(model, messages);
    req.protocol = Protocol::OpenaiResponses;
    req.tools = tools;
    req.tool_choice = tool_choice;
    req.parallel_tool_calls = opt_bool(obj, "parallel_tool_calls")?;
    req.stream = opt_bool(obj, "stream")?.unwrap_or(false);
    req.max_tokens = max_tokens;
    req.temperature = opt_f64(obj, "temperature")?;
    req.top_p = opt_f64(obj, "top_p")?;
    req.response_format = response_format;
    req.reasoning_effort = reasoning_effort;
    req.previous_response_id = previous;
    req.store = opt_bool(obj, "store")?;
    req.user = opt_str(obj, "user")?;
    req.metadata = metadata;
    req.extra = extra;
    Ok(req)
}

fn parse_format(f: &Value) -> Option<ResponseFormat> {
    match f.get("type").and_then(Value::as_str)? {
        "text" => Some(ResponseFormat::Text),
        "json_object" => Some(ResponseFormat::JsonObject),
        "json_schema" => Some(ResponseFormat::JsonSchema {
            name: s(f.get("name")),
            schema: f.get("schema").cloned(),
            strict: f.get("strict").and_then(Value::as_bool),
        }),
        _ => None,
    }
}

/// `function` and `custom` tools become [`ToolDef`]s; hosted tools (`web_search`, `mcp`,
/// `namespace`, ...) stay only in `extra["tools"]`.
fn parse_tool(t: &Value) -> Option<ToolDef> {
    let kind = t.get("type").and_then(Value::as_str)?;
    let name = s(t.get("name"))?;
    match kind {
        "function" => Some(ToolDef {
            name,
            description: s(t.get("description")),
            parameters: t.get("parameters").filter(|p| !p.is_null()).cloned(),
            strict: t.get("strict").and_then(Value::as_bool),
        }),
        "custom" => Some(ToolDef {
            name,
            description: s(t.get("description")),
            parameters: None,
            strict: None,
        }),
        _ => None,
    }
}

fn parse_tool_choice(v: &Value) -> Option<ToolChoice> {
    match v {
        Value::String(c) => match c.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => None,
        },
        Value::Object(o) => {
            if o.get("type").and_then(Value::as_str).is_some_and(|t| t != "function") {
                return None;
            }
            let name = o.get("name").or_else(|| o.get("function").and_then(|f| f.get("name")));
            s(name).map(|name| ToolChoice::Tool { name })
        }
        _ => None,
    }
}

fn parse_part(p: &Value) -> Part {
    match p.get("type").and_then(Value::as_str) {
        Some("input_text" | "output_text" | "text") => Part::Text {
            text: s(p.get("text")).unwrap_or_default(),
        },
        Some("refusal") => Part::Refusal {
            text: s(p.get("refusal")).unwrap_or_default(),
        },
        Some("input_image") => match s(p.get("image_url")).or_else(|| s(p.get("file_id")).map(|id| format!("file_id:{id}"))) {
            Some(url) => Part::Image {
                url,
                detail: s(p.get("detail")),
            },
            None => Part::Other { value: p.clone() },
        },
        Some("input_file") => Part::File {
            file_id: s(p.get("file_id")),
            filename: s(p.get("filename")),
            file_data: s(p.get("file_data")).or_else(|| s(p.get("file_url"))),
        },
        _ => Part::Other { value: p.clone() },
    }
}

fn parse_content(v: Option<&Value>) -> Vec<Part> {
    match v {
        Some(Value::String(t)) => vec![Part::Text { text: t.clone() }],
        Some(Value::Array(parts)) => parts.iter().map(parse_part).collect(),
        _ => Vec::new(),
    }
}

fn assistant_message() -> Message {
    let mut m = Message::text(Role::Assistant, "");
    m.content.clear();
    m
}

fn parse_items(items: &[Value], out: &mut Vec<Message>, extra: &mut Map<String, Value>) {
    let mut group: Option<Message> = None;
    let flush = |group: &mut Option<Message>, out: &mut Vec<Message>| out.extend(group.take());
    for item in items {
        let kind = item.get("type").and_then(Value::as_str);
        let role = item.get("role").and_then(Value::as_str);
        match (kind, role) {
            (Some("message") | None, Some(role)) => {
                let content = parse_content(item.get("content"));
                if role == "assistant" {
                    group.get_or_insert_with(assistant_message).content.extend(content);
                } else {
                    flush(&mut group, out);
                    let role = match role {
                        "system" => Role::System,
                        "developer" => Role::Developer,
                        _ => Role::User,
                    };
                    let mut m = assistant_message();
                    m.role = role;
                    m.content = content;
                    out.push(m);
                }
            }
            (Some("function_call" | "custom_tool_call"), _) => {
                let arguments = s(item.get("arguments")).or_else(|| s(item.get("input"))).unwrap_or_default();
                group.get_or_insert_with(assistant_message).tool_calls.push(ToolCall {
                    id: s(item.get("call_id")).or_else(|| s(item.get("id"))).unwrap_or_default(),
                    name: s(item.get("name")).unwrap_or_default(),
                    arguments,
                });
            }
            (Some("reasoning"), _) => {
                let g = group.get_or_insert_with(assistant_message);
                let texts: Vec<String> = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|p| s(p.get("text"))).collect())
                    .unwrap_or_default();
                if !texts.is_empty() {
                    let joined = texts.join("\n");
                    g.reasoning = Some(match g.reasoning.take() {
                        Some(prev) => format!("{prev}\n{joined}"),
                        None => joined,
                    });
                }
                if let Some(enc) = s(item.get("encrypted_content")) {
                    g.reasoning_opaque = Some(enc);
                }
            }
            (Some("function_call_output" | "custom_tool_call_output"), _) => {
                flush(&mut group, out);
                out.push(tool_message(item));
            }
            (Some("item_reference"), _) => {
                if let Some(id) = s(item.get("id")) {
                    let refs = extra.entry("item_references").or_insert_with(|| json!([]));
                    if let Some(a) = refs.as_array_mut() {
                        a.push(id.into());
                    }
                }
            }
            (Some(t), _) if t.ends_with("_call") || t == "compaction" => {
                group
                    .get_or_insert_with(assistant_message)
                    .content
                    .push(Part::Other { value: item.clone() });
            }
            (Some(t), _) if t.ends_with("_output") => {
                flush(&mut group, out);
                let mut m = tool_message(item);
                m.content = vec![Part::Other { value: item.clone() }];
                out.push(m);
            }
            _ => {
                flush(&mut group, out);
                let mut m = assistant_message();
                m.role = Role::User;
                m.content = vec![Part::Other { value: item.clone() }];
                out.push(m);
            }
        }
    }
    flush(&mut group, out);
}

fn tool_message(item: &Value) -> Message {
    let mut m = assistant_message();
    m.role = Role::Tool;
    m.tool_call_id = s(item.get("call_id"));
    m.content = parse_content(item.get("output"));
    m
}

/// 404 `invalid_request_error`, code `previous_response_not_found`, param `previous_response_id`:
/// "Previous response with id '<id>' not found."
pub fn previous_not_found(id: &str) -> ApiError {
    ApiError::invalid_request(format!("Previous response with id '{id}' not found."))
        .with_status(404)
        .code("previous_response_not_found")
        .param("previous_response_id")
}

/// 404 `invalid_request_error` for `GET`/`DELETE /v1/responses/{id}`:
/// "Response with id '<id>' not found."
pub fn not_found(id: &str) -> ApiError {
    ApiError::invalid_request(format!("Response with id '{id}' not found.")).with_status(404)
}

fn wants_encrypted(req: &Request) -> bool {
    req.extra
        .get("include")
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("reasoning.encrypted_content")))
}

fn incomplete_reason(finish: FinishReason) -> Option<&'static str> {
    match finish {
        FinishReason::Length => Some("max_output_tokens"),
        FinishReason::ContentFilter => Some("content_filter"),
        FinishReason::Stop | FinishReason::ToolCalls => None,
    }
}

/// The status a finished reply has: `completed`, or `incomplete` for `Length` / `ContentFilter`.
pub fn reply_status(reply: &Reply) -> &'static str {
    if incomplete_reason(reply.finish).is_some() {
        "incomplete"
    } else {
        "completed"
    }
}

/// The `output` array: an optional `reasoning` item, an optional `message` item, then one
/// `function_call` item per tool call, ids derived from `meta.seed`.
pub fn output_items(req: &Request, reply: &Reply, meta: &ResponseMeta) -> Vec<Value> {
    let mut items = Vec::new();
    let reasoning = reply.reasoning.as_deref().filter(|r| !r.is_empty());
    if let Some(text) = reasoning {
        let mut item = json!({
            "id": fallback::responses_id("rs", meta.seed, items.len() as u64),
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": text}],
        });
        if wants_encrypted(req) {
            item["encrypted_content"] = reasoning_opaque(reply, meta).into();
        }
        items.push(item);
    }
    let text = reply.text.as_deref().filter(|t| !t.is_empty());
    let refusal = reply.refusal.as_deref().filter(|t| !t.is_empty());
    if text.is_some() || refusal.is_some() || (reasoning.is_none() && reply.tool_calls.is_empty()) {
        let mut content = Vec::new();
        if text.is_some() || refusal.is_none() {
            content.push(json!({"type": "output_text", "text": text.unwrap_or(""), "annotations": [], "logprobs": []}));
        }
        if let Some(r) = refusal {
            content.push(json!({"type": "refusal", "refusal": r}));
        }
        items.push(json!({
            "id": fallback::responses_id("msg", meta.seed, items.len() as u64),
            "type": "message",
            "status": if reply_status(reply) == "completed" { "completed" } else { "incomplete" },
            "role": "assistant",
            "content": content,
        }));
    }
    for call in &reply.tool_calls {
        let id_len = items.len() as u64;
        if is_custom_tool(req, &call.name) {
            items.push(json!({
                "id": fallback::responses_id("ctc", meta.seed, id_len),
                "type": "custom_tool_call",
                "status": "completed",
                "call_id": call.id,
                "name": call.name,
                "input": custom_input(&call.arguments),
            }));
            continue;
        }
        items.push(json!({
            "id": fallback::responses_id("fc", meta.seed, id_len),
            "type": "function_call",
            "status": "completed",
            "call_id": call.id,
            "name": call.name,
            "arguments": call.arguments,
        }));
    }
    items
}

/// Whether the request declared `name` as a freeform `custom` tool (codex `apply_patch`); calls
/// to it must come back as `custom_tool_call` or the client cannot dispatch them.
fn is_custom_tool(req: &Request, name: &str) -> bool {
    req.extra
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(|t| t["type"] == "custom" && t["name"].as_str() == Some(name)))
}

/// The raw input of a freeform call: the script's `arguments` string, or its `input` field when
/// the arguments are a JSON object that has one.
fn custom_input(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(o)) => match o.get("input") {
            Some(Value::String(s)) => s.clone(),
            _ => arguments.to_owned(),
        },
        _ => arguments.to_owned(),
    }
}

fn reasoning_opaque(reply: &Reply, meta: &ResponseMeta) -> String {
    reply
        .reasoning_opaque
        .clone()
        .unwrap_or_else(|| fallback::reasoning_opaque(meta.seed))
}

fn usage_json(reply: &Reply) -> Value {
    let u = &reply.usage;
    json!({
        "input_tokens": u.prompt_tokens,
        "input_tokens_details": {"cached_tokens": u.cached_tokens},
        "output_tokens": u.completion_tokens,
        "output_tokens_details": {"reasoning_tokens": u.reasoning_tokens},
        "total_tokens": u.total_tokens(),
    })
}

fn tool_choice_json(req: &Request) -> Value {
    match &req.tool_choice {
        None => req.extra.get("tool_choice").cloned().unwrap_or_else(|| "auto".into()),
        Some(ToolChoice::Auto) => "auto".into(),
        Some(ToolChoice::None) => "none".into(),
        Some(ToolChoice::Required) => "required".into(),
        Some(ToolChoice::Tool { name }) => json!({"type": "function", "name": name}),
    }
}

/// The full response object with the given `status` (`completed`, `incomplete`, `failed`).
/// `output` is what [`output_items`] returned, or `[]` for a failed response; `error` is
/// `Some({code, message})` only when failed. Used for the sync body, the stored copy and the
/// `response` field of lifecycle events (`response.created` carries `status: "in_progress"` and
/// an empty `output`).
pub fn response_object(
    req: &Request,
    reply: Option<&Reply>,
    meta: &ResponseMeta,
    status: &str,
    output: &[Value],
    error: Option<Value>,
) -> Value {
    let extra = |k: &str| req.extra.get(k).cloned();
    let incomplete = reply
        .filter(|_| status == "incomplete")
        .and_then(|r| incomplete_reason(r.finish))
        .map(|reason| json!({"reason": reason}));
    json!({
        "id": meta.id,
        "object": "response",
        "created_at": meta.created,
        "status": status,
        "background": false,
        "error": error,
        "incomplete_details": incomplete,
        "instructions": extra("instructions"),
        "max_output_tokens": req.max_tokens,
        "model": meta.model,
        "output": output,
        "parallel_tool_calls": req.parallel_tool_calls.unwrap_or(true),
        "previous_response_id": req.previous_response_id,
        "reasoning": extra("reasoning").unwrap_or_else(|| json!({"effort": null, "summary": null})),
        "safety_identifier": extra("safety_identifier"),
        "service_tier": extra("service_tier").unwrap_or_else(|| "default".into()),
        "store": req.store.unwrap_or(true),
        "temperature": req.temperature.unwrap_or(1.0),
        "text": extra("text").unwrap_or_else(|| json!({"format": {"type": "text"}})),
        "tool_choice": tool_choice_json(req),
        "tools": extra("tools").unwrap_or_else(|| json!([])),
        "top_p": req.top_p.unwrap_or(1.0),
        "truncation": extra("truncation").unwrap_or_else(|| "disabled".into()),
        "usage": reply.filter(|_| status != "in_progress" && status != "failed").map(usage_json),
        "user": req.user,
        "metadata": req.metadata,
        "prompt_cache_key": extra("prompt_cache_key"),
    })
}

/// The sync body: `response_object` with the reply's status (`incomplete` with
/// `incomplete_details {reason: "max_output_tokens"}` when `finish == Length`).
pub fn render_sync(req: &Request, reply: &Reply, meta: &ResponseMeta) -> Value {
    response_object(req, Some(reply), meta, reply_status(reply), &output_items(req, reply, meta), None)
}

/// The store entry for this response: `render_sync` as `response`, and `conversation` per
/// [`StoredResponse::conversation`]. The pipeline inserts it unless `req.store == Some(false)`.
pub fn to_stored(req: &Request, reply: &Reply, meta: &ResponseMeta) -> StoredResponse {
    let has_instructions = req.extra.get("instructions").and_then(Value::as_str).is_some_and(|i| !i.is_empty());
    let skip = usize::from(has_instructions && req.messages.first().is_some_and(|m| m.role == Role::System));
    let mut conversation: Vec<Message> = req.messages[skip..].to_vec();

    let mut content = Vec::new();
    if let Some(t) = reply.text.as_deref().filter(|t| !t.is_empty()) {
        content.push(Part::Text { text: t.to_owned() });
    }
    if let Some(r) = reply.refusal.as_deref().filter(|t| !t.is_empty()) {
        content.push(Part::Refusal { text: r.to_owned() });
    }
    let reasoning = reply.reasoning.clone().filter(|r| !r.is_empty());
    let reasoning_opaque = reasoning.as_ref().map(|_| reasoning_opaque(reply, meta));
    let mut m = assistant_message();
    m.content = content;
    m.tool_calls = reply.tool_calls.clone();
    m.reasoning = reasoning;
    m.reasoning_opaque = reasoning_opaque;
    conversation.push(m);

    StoredResponse {
        id: meta.id.clone(),
        created_at: meta.created,
        response: render_sync(req, reply, meta),
        conversation,
    }
}

/// `{"id": "<id>", "object": "response", "deleted": true}`.
pub fn deleted_json(id: &str) -> Value {
    json!({"id": id, "object": "response", "deleted": true})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Usage;

    fn meta() -> ResponseMeta {
        ResponseMeta {
            id: "resp_1".into(),
            created: 1_700_000_000,
            model: "gpt-5.5".into(),
            seed: 7,
        }
    }

    fn reply(text: Option<&str>) -> Reply {
        Reply {
            text: text.map(str::to_owned),
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                reasoning_tokens: 2,
                cached_tokens: 1,
                cache_creation_tokens: 0,
            },
            ..Reply::default()
        }
    }

    fn parse_ok(body: Value) -> Request {
        parse(&body, &ResponseStore::default()).unwrap()
    }

    #[test]
    fn parses_codex_shaped_request() {
        let req = parse_ok(json!({
            "model": "gpt-5.5",
            "instructions": "You are codex.",
            "input": [
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "dev rules"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}], "encrypted_content": "ENC"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]},
                {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"cmd\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "a.txt"},
                {"type": "local_shell_call", "id": "x"},
                {"type": "item_reference", "id": "rs_9"},
                {"type": "totally_new", "x": 1}
            ],
            "tools": [
                {"type": "function", "name": "shell", "description": "run", "parameters": {"type": "object"}, "strict": false},
                {"type": "custom", "name": "apply_patch", "description": "patch"},
                {"type": "web_search"}
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "reasoning": {"effort": "medium", "summary": "auto"},
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "abc",
            "text": {"format": {"type": "json_schema", "name": "n", "schema": {"type": "object"}, "strict": true}}
        }));
        assert_eq!(req.protocol, Protocol::OpenaiResponses);
        assert!(req.stream);
        assert_eq!(req.store, Some(false));
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[1].role, Role::Developer);
        assert_eq!(req.messages[2].role, Role::User);
        let a = &req.messages[3];
        assert_eq!(a.role, Role::Assistant);
        assert_eq!(a.text_content(), "ok");
        assert_eq!(a.reasoning.as_deref(), Some("thinking"));
        assert_eq!(a.reasoning_opaque.as_deref(), Some("ENC"));
        assert_eq!(a.tool_calls[0].id, "call_1");
        assert!(a.content.iter().any(|p| matches!(p, Part::Other { .. })) || req.messages[4].role == Role::Tool);
        assert_eq!(req.messages[4].role, Role::Tool);
        assert_eq!(req.messages[4].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(req.assistant_turns(), 2);
        assert_eq!(req.messages.last().unwrap().role, Role::User);
        assert_eq!(req.extra["item_references"], json!(["rs_9"]));
        assert_eq!(req.tools.len(), 2);
        assert_eq!(req.tools[1].parameters, None);
        assert_eq!(req.extra["tools"].as_array().unwrap().len(), 3);
        assert_eq!(req.tool_choice, Some(ToolChoice::Auto));
        assert!(!req.extra.contains_key("tool_choice"));
        assert_eq!(req.parallel_tool_calls, Some(false));
        assert_eq!(req.reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(req.extra["instructions"], "You are codex.");
        assert_eq!(req.extra["prompt_cache_key"], "abc");
        assert!(matches!(
            req.response_format,
            Some(ResponseFormat::JsonSchema { strict: Some(true), .. })
        ));
    }

    #[test]
    fn parses_ai_sdk_shaped_request() {
        let req = parse_ok(json!({
            "model": "gpt-4o",
            "input": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": [{"type": "input_text", "text": "hello"}, {"type": "input_image", "image_url": "data:image/png;base64,AA"}]}
            ],
            "temperature": 0.5,
            "max_output_tokens": 100,
            "tool_choice": {"type": "function", "name": "get"},
            "tools": [{"type": "function", "name": "get", "parameters": {}}]
        }));
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[1].content.len(), 2);
        assert!(matches!(req.messages[1].content[1], Part::Image { .. }));
        assert_eq!(req.max_tokens, Some(100));
        assert_eq!(req.temperature, Some(0.5));
        assert_eq!(req.tool_choice, Some(ToolChoice::Tool { name: "get".into() }));
        let s = parse_ok(json!({"model": "m", "input": "hi"}));
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.last_user_text().as_deref(), Some("hi"));
    }

    #[test]
    fn parse_errors() {
        let store = ResponseStore::default();
        let err = |b: Value| parse(&b, &store).unwrap_err();
        assert_eq!(err(json!({"input": "x"})).param.as_deref(), Some("model"));
        assert_eq!(err(json!({"model": "m"})).param.as_deref(), Some("input"));
        assert_eq!(err(json!({"model": "m", "input": 3})).code.as_deref(), Some("invalid_type"));
        let e = err(json!({"model": "m", "input": "x", "max_output_tokens": 5}));
        assert_eq!(e.code.as_deref(), Some("integer_below_min_value"));
        assert!(e.message.contains("got 5 instead"));
        let e = err(json!({"model": "m", "input": "x", "previous_response_id": "resp_x"}));
        assert_eq!(
            (e.status, e.code.as_deref(), e.param.as_deref()),
            (404, Some("previous_response_not_found"), Some("previous_response_id"))
        );
    }

    #[test]
    fn sync_shape() {
        let req = parse_ok(json!({"model": "gpt-5.5", "input": "hi", "instructions": "i"}));
        let v = render_sync(&req, &reply(Some("hello")), &meta());
        assert_eq!(v["id"], "resp_1");
        assert_eq!(v["object"], "response");
        assert_eq!(v["created_at"], 1_700_000_000);
        assert_eq!(v["status"], "completed");
        assert_eq!(v["model"], "gpt-5.5");
        assert_eq!(v["instructions"], "i");
        assert_eq!(v["store"], true);
        assert_eq!(v["usage"]["input_tokens"], 10);
        assert_eq!(v["usage"]["output_tokens"], 5);
        assert_eq!(v["usage"]["total_tokens"], 15);
        assert_eq!(v["usage"]["input_tokens_details"]["cached_tokens"], 1);
        assert_eq!(v["usage"]["output_tokens_details"]["reasoning_tokens"], 2);
        let m = &v["output"][0];
        assert_eq!(m["type"], "message");
        assert_eq!(m["role"], "assistant");
        assert_eq!(m["content"][0]["text"], "hello");
        assert!(m["id"].as_str().unwrap().starts_with("msg_"));
        assert!(v["incomplete_details"].is_null() && v["error"].is_null());
    }

    #[test]
    fn incomplete_on_length() {
        let req = parse_ok(json!({"model": "m", "input": "hi"}));
        let mut r = reply(Some("cut"));
        r.finish = FinishReason::Length;
        let v = render_sync(&req, &r, &meta());
        assert_eq!(v["status"], "incomplete");
        assert_eq!(v["incomplete_details"]["reason"], "max_output_tokens");
        assert_eq!(v["output"][0]["status"], "incomplete");
    }

    #[test]
    fn output_items_reasoning_and_calls() {
        let req = parse_ok(json!({"model": "m", "input": "hi", "include": ["reasoning.encrypted_content"]}));
        let mut r = reply(None);
        r.reasoning = Some("plan".into());
        r.tool_calls = vec![ToolCall {
            id: "call_a".into(),
            name: "f".into(),
            arguments: "{}".into(),
        }];
        let items = output_items(&req, &r, &meta());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "reasoning");
        assert_eq!(items[0]["summary"][0]["text"], "plan");
        assert_eq!(items[0]["encrypted_content"], fallback::reasoning_opaque(7));
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "call_a");
        let plain = parse_ok(json!({"model": "m", "input": "hi"}));
        assert!(output_items(&plain, &r, &meta())[0].get("encrypted_content").is_none());
        let empty = output_items(&plain, &reply(None), &meta());
        assert_eq!(empty[0]["type"], "message");
    }

    #[test]
    fn custom_tool_call_for_declared_custom_tools() {
        let req = parse_ok(json!({
            "model": "m", "input": "hi",
            "tools": [{"type": "custom", "name": "apply_patch"}, {"type": "function", "name": "f", "parameters": {}}],
        }));
        let call = |name: &str, arguments: &str| ToolCall {
            id: "call_a".into(),
            name: name.into(),
            arguments: arguments.into(),
        };
        let mut r = reply(None);
        r.tool_calls = vec![
            call("apply_patch", r#"{"input":"*** Begin Patch"}"#),
            call("apply_patch", "raw"),
            call("f", "{}"),
        ];
        let items = output_items(&req, &r, &meta());
        assert_eq!(items[0]["type"], "custom_tool_call");
        assert_eq!(items[0]["input"], "*** Begin Patch");
        assert!(items[0]["id"].as_str().unwrap().starts_with("ctc_"));
        assert_eq!(items[1]["input"], "raw");
        assert_eq!(items[2]["type"], "function_call");
    }

    #[test]
    fn chaining_and_store() {
        let store = ResponseStore::default();
        let req = parse(&json!({"model": "m", "instructions": "sys", "input": "one"}), &store).unwrap();
        let mut m1 = meta();
        m1.id = "resp_a".into();
        store.insert(to_stored(&req, &reply(Some("first")), &m1));
        let stored = store.get("resp_a").unwrap();
        assert_eq!(stored.conversation.len(), 2);
        assert_eq!(stored.conversation[0].role, Role::User);
        assert_eq!(stored.response["id"], "resp_a");

        let req2 = parse(&json!({"model": "m", "input": "two", "previous_response_id": "resp_a"}), &store).unwrap();
        assert_eq!(req2.messages.len(), 3);
        assert_eq!(req2.assistant_turns(), 1);
        assert_eq!(req2.previous_response_id.as_deref(), Some("resp_a"));
        let mut m2 = meta();
        m2.id = "resp_b".into();
        store.insert(to_stored(&req2, &reply(Some("second")), &m2));
        let req3 = parse(&json!({"model": "m", "input": "three", "previous_response_id": "resp_b"}), &store).unwrap();
        assert_eq!(req3.messages.len(), 5);
        assert_eq!(req3.assistant_turns(), 2);

        assert!(store.remove("resp_b"));
        let e = parse(&json!({"model": "m", "input": "x", "previous_response_id": "resp_b"}), &store).unwrap_err();
        assert_eq!(e.status, 404);
    }

    #[test]
    fn store_false_is_echoed() {
        let req = parse_ok(json!({"model": "m", "input": "hi", "store": false}));
        assert_eq!(render_sync(&req, &reply(Some("x")), &meta())["store"], false);
    }

    #[test]
    fn errors_and_deleted() {
        assert_eq!(not_found("resp_x").message, "Response with id 'resp_x' not found.");
        assert_eq!(not_found("resp_x").status, 404);
        assert_eq!(deleted_json("resp_x")["deleted"], true);
    }
}
