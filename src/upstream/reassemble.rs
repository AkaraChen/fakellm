//! Turns a copy of an upstream response, as the bytes came off the wire, back into an IR outcome: the inverse of
//! the frontends' renderers. One parser per protocol for SSE (Chat chunks, Responses events, Anthropic events) and
//! one for each sync body. The mapping tables are in docs/m4-spec.md section 4.3; unknown event types and fields
//! are skipped, never an error, because upstreams add them freely.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::cassette::format::CassetteOutcome;
use crate::ir::{ApiError, FinishReason, Protocol, Reply, ToolCall, Usage};

/// What a finished upstream response amounts to.
#[derive(Clone, Debug, PartialEq)]
pub struct Reassembled {
    /// A reply, or the error the upstream answered (an HTTP error status, or an error event inside a 200 stream,
    /// which sets `ApiError::in_stream`).
    pub outcome: CassetteOutcome,
    /// For Responses: the final response object (`response.completed.response`, or the sync body), so the server can
    /// store it under the upstream's own id for `previous_response_id`. `None` for the other protocols and for errors.
    pub response: Option<Value>,
}

/// Incremental reassembly: feed the body in whatever pieces it arrives, then ask for the outcome.
pub struct Reassembler {
    protocol: Protocol,
    buf: Vec<u8>,
    /// The body outgrew [`MAX_BODY`]: the copy was dropped and the exchange cannot be recorded.
    overflow: bool,
}

/// Most bytes of one upstream response that are kept for reassembly; a larger one is relayed but not recorded.
pub const MAX_BODY: usize = 64 * 1024 * 1024;

impl Reassembler {
    pub fn new(protocol: Protocol) -> Self {
        Self {
            protocol,
            buf: Vec::new(),
            overflow: false,
        }
    }

    /// Appends bytes of the body. Pieces may split an SSE frame, a line, or a UTF-8 character anywhere.
    pub fn push(&mut self, chunk: &[u8]) {
        if self.overflow {
            return;
        }
        if self.buf.len() + chunk.len() > MAX_BODY {
            self.overflow = true;
            self.buf = Vec::new();
            return;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// The outcome of the whole body. `status` and `headers` decide the reading: 400 and above is an error body
    /// (`retry-after` becomes `ApiError::retry_after`; `x-should-retry` and `retry-after-ms` are kept in
    /// `ApiError::headers`); `content-type: text/event-stream` is SSE; anything else is a sync JSON body. Fails
    /// with a message when the body is cut off (an SSE stream without its final event) or is not what the protocol
    /// sends; the caller then records nothing.
    pub fn finish(self, status: u16, headers: &http::HeaderMap) -> Result<Reassembled, String> {
        if self.overflow {
            return Err(format!(
                "the response body is larger than {} MiB: relayed, not recorded",
                MAX_BODY >> 20
            ));
        }
        if status >= 400 {
            let error = error_from_body(self.protocol, status, &self.buf, headers);
            return Ok(Reassembled {
                outcome: CassetteOutcome::Error { error },
                response: None,
            });
        }
        let sse = headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/event-stream"));
        if sse {
            let text = String::from_utf8_lossy(&self.buf);
            let events = sse_events(&text);
            return match self.protocol {
                Protocol::OpenaiChat => chat_sse(&events),
                Protocol::AnthropicMessages => anthropic_sse(&events),
                Protocol::OpenaiResponses => responses_sse(&events),
                Protocol::OpenaiEmbeddings => Err("an embeddings response cannot be an event stream".into()),
                Protocol::Custom => Err("a custom endpoint response cannot be an event stream".into()),
            };
        }
        let body: Value = serde_json::from_slice(&self.buf).map_err(|e| format!("the response body is not JSON: {e}"))?;
        sync_outcome(self.protocol, &body)
    }
}

fn sync_outcome(protocol: Protocol, body: &Value) -> Result<Reassembled, String> {
    if protocol == Protocol::OpenaiResponses {
        return responses_outcome(body);
    }
    Ok(Reassembled {
        outcome: CassetteOutcome::Reply {
            reply: sync_reply(protocol, body)?,
        },
        response: None,
    })
}

/// A sync success body of `protocol` as a reply (Chat completion, Anthropic message, Responses object).
pub fn sync_reply(protocol: Protocol, body: &Value) -> Result<Reply, String> {
    match protocol {
        Protocol::OpenaiChat => chat_sync(body),
        Protocol::AnthropicMessages => anthropic_sync(body),
        Protocol::OpenaiResponses => responses_sync(body),
        Protocol::OpenaiEmbeddings => Ok(Reply {
            usage: Usage {
                prompt_tokens: u32_at(&body["usage"], "prompt_tokens")
                    .or_else(|| u32_at(&body["usage"], "total_tokens"))
                    .unwrap_or(0),
                ..Usage::default()
            },
            ..Reply::default()
        }),
        Protocol::Custom => Err("a custom endpoint is never relayed upstream".into()),
    }
}

/// An error response as an [`ApiError`]: OpenAI's `{"error": {message, type, code, param}}`, Anthropic's
/// `{"type": "error", "error": {type, message}}`; a body that is neither keeps its text as the message.
pub fn error_from_body(protocol: Protocol, status: u16, body: &[u8], headers: &http::HeaderMap) -> ApiError {
    let fallback = ApiError::for_status_in(protocol, status);
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let mut error = match parsed.as_ref().and_then(|v| v.get("error")) {
        Some(e) if e.is_object() => {
            let mut err = ApiError::new(
                status,
                e.get("type").and_then(Value::as_str).unwrap_or(&fallback.kind),
                e.get("message").and_then(Value::as_str).unwrap_or(&fallback.message),
            );
            err.code = scalar_text(e.get("code"));
            err.param = scalar_text(e.get("param"));
            err
        }
        Some(Value::String(s)) => ApiError::new(status, fallback.kind.clone(), s.clone()),
        _ => {
            let text = String::from_utf8_lossy(body);
            let text: String = text.trim().chars().take(500).collect();
            let message = if text.is_empty() { fallback.message.clone() } else { text };
            ApiError::new(status, fallback.kind.clone(), message)
        }
    };
    if let Some(secs) = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
    {
        error.retry_after = Some(Duration::from_secs_f64(secs));
    }
    for name in ["retry-after-ms", "x-should-retry"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            error.headers.push((name.into(), v.into()));
        }
    }
    error
}

fn scalar_text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn u32_at(v: &Value, key: &str) -> Option<u32> {
    v.get(key).and_then(Value::as_u64).map(|n| n.min(u32::MAX as u64) as u32)
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

/// The status an error found inside a stream gets, from its type or code.
fn in_stream_status(kind: &str) -> u16 {
    match kind {
        "rate_limit_error" | "rate_limit_exceeded" | "insufficient_quota" => 429,
        "overloaded_error" => 529,
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "permission_error" => 403,
        "not_found_error" => 404,
        _ => 500,
    }
}

/// An error object found inside a 200 stream (`{"error": {...}}`, an `error` event, or a bare error object).
fn in_stream_error(e: &Value) -> ApiError {
    let kind = e
        .get("type")
        .and_then(Value::as_str)
        .filter(|t| *t != "error")
        .unwrap_or("api_error");
    let code = scalar_text(e.get("code"));
    let status = in_stream_status(code.as_deref().filter(|c| in_stream_status(c) != 500).unwrap_or(kind));
    let mut err = ApiError::new(status, kind, e.get("message").and_then(Value::as_str).unwrap_or("stream error"));
    err.code = code;
    err.param = scalar_text(e.get("param"));
    err.in_stream = true;
    err
}

fn in_stream_outcome(e: &Value) -> Reassembled {
    Reassembled {
        outcome: CassetteOutcome::Error { error: in_stream_error(e) },
        response: None,
    }
}

// ---------------------------------------------------------------------------------------------------- SSE framing

struct Event {
    name: Option<String>,
    data: String,
}

impl Event {
    fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.data).ok()
    }
}

/// Splits a stream into events: blank-line separated, `data:` lines joined with `\n`, comments dropped. A trailing
/// event without its blank line is kept (the caller decides whether the stream was complete).
fn sse_events(text: &str) -> Vec<Event> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut name = None;
        let mut data: Vec<&str> = Vec::new();
        for line in block.lines() {
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => name = Some(value.to_owned()),
                "data" => data.push(value),
                _ => {}
            }
        }
        if !data.is_empty() {
            out.push(Event {
                name,
                data: data.join("\n"),
            });
        }
    }
    out
}

fn finish_from_chat(s: &str) -> FinishReason {
    match s {
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
    }
}

fn finish_from_anthropic(s: &str) -> FinishReason {
    match s {
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "refusal" => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
    }
}

// ------------------------------------------------------------------------------------------------------------ Chat

fn chat_usage(u: &Value) -> Usage {
    Usage {
        prompt_tokens: u32_at(u, "prompt_tokens").unwrap_or(0),
        completion_tokens: u32_at(u, "completion_tokens").unwrap_or(0),
        reasoning_tokens: u32_at(&u["completion_tokens_details"], "reasoning_tokens").unwrap_or(0),
        cached_tokens: u32_at(&u["prompt_tokens_details"], "cached_tokens").unwrap_or(0),
        cache_creation_tokens: 0,
    }
}

fn str_at<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_str))
}

fn chat_sync(body: &Value) -> Result<Reply, String> {
    let choice = body["choices"].get(0).ok_or("the chat completion has no choices")?;
    let msg = &choice["message"];
    let tool_calls = msg["tool_calls"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| ToolCall {
                    id: t["id"].as_str().unwrap_or_default().to_owned(),
                    name: t["function"]["name"].as_str().unwrap_or_default().to_owned(),
                    arguments: t["function"]["arguments"].as_str().unwrap_or_default().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Reply {
        text: msg["content"].as_str().map(str::to_owned).filter(|s| !s.is_empty()),
        tool_calls,
        reasoning: str_at(msg, &["reasoning_content", "reasoning"])
            .map(str::to_owned)
            .filter(|s| !s.is_empty()),
        refusal: msg["refusal"].as_str().map(str::to_owned).filter(|s| !s.is_empty()),
        reasoning_opaque: None,
        finish: finish_from_chat(choice["finish_reason"].as_str().unwrap_or("stop")),
        usage: chat_usage(&body["usage"]),
    })
}

fn chat_sse(events: &[Event]) -> Result<Reassembled, String> {
    let (mut text, mut refusal, mut reasoning) = (String::new(), String::new(), String::new());
    let mut calls: BTreeMap<u64, ToolCall> = BTreeMap::new();
    let mut finish = None;
    let mut usage = Usage::default();
    for ev in events {
        if ev.data.trim() == "[DONE]" {
            break;
        }
        let Some(v) = ev.json() else { continue };
        if let Some(e) = v.get("error").filter(|e| e.is_object()) {
            return Ok(in_stream_outcome(e));
        }
        if v["usage"].is_object() {
            usage = chat_usage(&v["usage"]);
        }
        let Some(choice) = v["choices"].get(0) else { continue };
        let delta = &choice["delta"];
        if let Some(s) = delta["content"].as_str() {
            text.push_str(s);
        }
        if let Some(s) = delta["refusal"].as_str() {
            refusal.push_str(s);
        }
        if let Some(s) = str_at(delta, &["reasoning_content", "reasoning"]) {
            reasoning.push_str(s);
        }
        if let Some(tcs) = delta["tool_calls"].as_array() {
            for tc in tcs {
                let idx = match tc["index"].as_u64() {
                    Some(i) => i,
                    None if tc["id"].as_str().is_some_and(|s| !s.is_empty()) => calls.keys().next_back().map_or(0, |k| k + 1),
                    None => calls.keys().next_back().copied().unwrap_or(0),
                };
                let call = calls.entry(idx).or_insert_with(|| ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
                if call.id.is_empty()
                    && let Some(id) = tc["id"].as_str()
                {
                    call.id = id.to_owned();
                }
                if call.name.is_empty()
                    && let Some(n) = tc["function"]["name"].as_str()
                {
                    call.name = n.to_owned();
                }
                if let Some(a) = tc["function"]["arguments"].as_str() {
                    call.arguments.push_str(a);
                }
            }
        }
        if let Some(f) = choice["finish_reason"].as_str() {
            finish = Some(finish_from_chat(f));
        }
    }
    let finish = finish.ok_or("the chat stream ended without a finish_reason")?;
    Ok(Reassembled {
        outcome: CassetteOutcome::Reply {
            reply: Reply {
                text: non_empty(text),
                tool_calls: calls.into_values().collect(),
                reasoning: non_empty(reasoning),
                refusal: non_empty(refusal),
                reasoning_opaque: None,
                finish,
                usage,
            },
        },
        response: None,
    })
}

// -------------------------------------------------------------------------------------------------------- Anthropic

/// Anthropic's `prompt_tokens` is the total: fresh input plus both cache counters.
fn anthropic_usage(u: &Value, into: &mut Usage) {
    let input = u32_at(u, "input_tokens");
    let read = u32_at(u, "cache_read_input_tokens");
    let created = u32_at(u, "cache_creation_input_tokens");
    if input.is_some() || read.is_some() || created.is_some() {
        let (read, created) = (read.unwrap_or(0), created.unwrap_or(0));
        into.prompt_tokens = input.unwrap_or(0) + read + created;
        into.cached_tokens = read;
        into.cache_creation_tokens = created;
    }
    if let Some(out) = u32_at(u, "output_tokens") {
        into.completion_tokens = out;
    }
}

fn anthropic_sync(body: &Value) -> Result<Reply, String> {
    let content = body["content"].as_array().ok_or("the message has no content array")?;
    let (mut text, mut reasoning, mut opaque) = (String::new(), String::new(), None);
    let mut tool_calls = Vec::new();
    for block in content {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
            Some("tool_use") => tool_calls.push(ToolCall {
                id: block["id"].as_str().unwrap_or_default().to_owned(),
                name: block["name"].as_str().unwrap_or_default().to_owned(),
                arguments: block.get("input").map_or_else(|| "{}".into(), |i| i.to_string()),
            }),
            Some("thinking") => {
                reasoning.push_str(block["thinking"].as_str().unwrap_or_default());
                if let Some(s) = block["signature"].as_str() {
                    opaque = Some(s.to_owned());
                }
            }
            _ => {}
        }
    }
    let mut usage = Usage::default();
    anthropic_usage(&body["usage"], &mut usage);
    Ok(Reply {
        text: non_empty(text),
        tool_calls,
        reasoning: non_empty(reasoning),
        refusal: None,
        reasoning_opaque: opaque,
        finish: finish_from_anthropic(body["stop_reason"].as_str().unwrap_or("end_turn")),
        usage,
    })
}

enum Block {
    Text,
    Thinking,
    Tool(usize),
    Other,
}

fn anthropic_sse(events: &[Event]) -> Result<Reassembled, String> {
    let (mut text, mut reasoning, mut opaque) = (String::new(), String::new(), None::<String>);
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut blocks: BTreeMap<u64, Block> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut stop = None;
    let mut done = false;
    for ev in events {
        let Some(v) = ev.json() else { continue };
        let kind = v["type"].as_str().or(ev.name.as_deref()).unwrap_or_default();
        match kind {
            "message_start" => anthropic_usage(&v["message"]["usage"], &mut usage),
            "content_block_start" => {
                let idx = v["index"].as_u64().unwrap_or(0);
                let b = &v["content_block"];
                let block = match b["type"].as_str() {
                    Some("text") => {
                        text.push_str(b["text"].as_str().unwrap_or_default());
                        Block::Text
                    }
                    Some("thinking") => {
                        reasoning.push_str(b["thinking"].as_str().unwrap_or_default());
                        Block::Thinking
                    }
                    Some("tool_use") => {
                        calls.push(ToolCall {
                            id: b["id"].as_str().unwrap_or_default().to_owned(),
                            name: b["name"].as_str().unwrap_or_default().to_owned(),
                            arguments: String::new(),
                        });
                        Block::Tool(calls.len() - 1)
                    }
                    _ => Block::Other,
                };
                blocks.insert(idx, block);
            }
            "content_block_delta" => {
                let d = &v["delta"];
                let block = blocks.get(&v["index"].as_u64().unwrap_or(0));
                match d["type"].as_str() {
                    Some("text_delta") if matches!(block, Some(Block::Text)) => text.push_str(d["text"].as_str().unwrap_or_default()),
                    Some("thinking_delta") if matches!(block, Some(Block::Thinking)) => {
                        reasoning.push_str(d["thinking"].as_str().unwrap_or_default());
                    }
                    Some("signature_delta") if matches!(block, Some(Block::Thinking)) => {
                        opaque
                            .get_or_insert_with(String::new)
                            .push_str(d["signature"].as_str().unwrap_or_default());
                    }
                    Some("input_json_delta") => {
                        if let Some(Block::Tool(i)) = block {
                            calls[*i].arguments.push_str(d["partial_json"].as_str().unwrap_or_default());
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(s) = v["delta"]["stop_reason"].as_str() {
                    stop = Some(s.to_owned());
                }
                anthropic_usage(&v["usage"], &mut usage);
            }
            "message_stop" => {
                done = true;
                break;
            }
            "error" => return Ok(in_stream_outcome(&v["error"])),
            _ => {}
        }
    }
    if !done {
        return Err("the Anthropic stream ended without message_stop".into());
    }
    for c in &mut calls {
        if c.arguments.trim().is_empty() {
            c.arguments = "{}".into();
        }
    }
    Ok(Reassembled {
        outcome: CassetteOutcome::Reply {
            reply: Reply {
                text: non_empty(text),
                tool_calls: calls,
                reasoning: non_empty(reasoning),
                refusal: None,
                reasoning_opaque: opaque.filter(|s| !s.is_empty()),
                finish: finish_from_anthropic(stop.as_deref().unwrap_or("end_turn")),
                usage,
            },
        },
        response: None,
    })
}

// ------------------------------------------------------------------------------------------------------- Responses

fn responses_sync(body: &Value) -> Result<Reply, String> {
    let output = body["output"].as_array().ok_or("the response object has no output array")?;
    let (mut text, mut refusal, mut reasoning) = (String::new(), String::new(), Vec::<String>::new());
    let mut opaque = None;
    let mut tool_calls = Vec::new();
    for item in output {
        match item["type"].as_str() {
            Some("message") => {
                for part in item["content"].as_array().into_iter().flatten() {
                    match part["type"].as_str() {
                        Some("output_text") => text.push_str(part["text"].as_str().unwrap_or_default()),
                        Some("refusal") => refusal.push_str(part["refusal"].as_str().unwrap_or_default()),
                        _ => {}
                    }
                }
            }
            Some("function_call") => tool_calls.push(ToolCall {
                id: str_at(item, &["call_id", "id"]).unwrap_or_default().to_owned(),
                name: item["name"].as_str().unwrap_or_default().to_owned(),
                arguments: item["arguments"].as_str().unwrap_or_default().to_owned(),
            }),
            Some("custom_tool_call") => tool_calls.push(ToolCall {
                id: str_at(item, &["call_id", "id"]).unwrap_or_default().to_owned(),
                name: item["name"].as_str().unwrap_or_default().to_owned(),
                arguments: item["input"].as_str().unwrap_or_default().to_owned(),
            }),
            Some("reasoning") => {
                let parts: Vec<&str> = item["summary"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s["text"].as_str())
                    .collect();
                if !parts.is_empty() {
                    reasoning.push(parts.join("\n\n"));
                }
                if let Some(e) = item["encrypted_content"].as_str() {
                    opaque = Some(e.to_owned());
                }
            }
            _ => {}
        }
    }
    let finish = if body["status"] == "incomplete" {
        match body["incomplete_details"]["reason"].as_str() {
            Some("max_output_tokens") => FinishReason::Length,
            Some("content_filter") => FinishReason::ContentFilter,
            _ => FinishReason::Stop,
        }
    } else if !tool_calls.is_empty() {
        FinishReason::ToolCalls
    } else {
        FinishReason::Stop
    };
    let u = &body["usage"];
    Ok(Reply {
        text: non_empty(text),
        tool_calls,
        reasoning: non_empty(reasoning.join("\n\n")),
        refusal: non_empty(refusal),
        reasoning_opaque: opaque,
        finish,
        usage: Usage {
            prompt_tokens: u32_at(u, "input_tokens").unwrap_or(0),
            completion_tokens: u32_at(u, "output_tokens").unwrap_or(0),
            reasoning_tokens: u32_at(&u["output_tokens_details"], "reasoning_tokens").unwrap_or(0),
            cached_tokens: u32_at(&u["input_tokens_details"], "cached_tokens").unwrap_or(0),
            cache_creation_tokens: 0,
        },
    })
}

/// A Responses object (sync body or the `response` of a final event) as an outcome: `status: failed` is an error.
fn responses_outcome(obj: &Value) -> Result<Reassembled, String> {
    if obj["status"] == "failed" {
        let mut err = in_stream_error(&obj["error"]);
        err.in_stream = false;
        return Ok(Reassembled {
            outcome: CassetteOutcome::Error { error: err },
            response: None,
        });
    }
    Ok(Reassembled {
        outcome: CassetteOutcome::Reply {
            reply: responses_sync(obj)?,
        },
        response: Some(obj.clone()),
    })
}

fn responses_sse(events: &[Event]) -> Result<Reassembled, String> {
    for ev in events {
        let Some(v) = ev.json() else { continue };
        match v["type"].as_str().or(ev.name.as_deref()).unwrap_or_default() {
            "response.completed" | "response.incomplete" => return responses_outcome(&v["response"]),
            "response.failed" => {
                let mut out = responses_outcome(&v["response"])?;
                if let CassetteOutcome::Error { error } = &mut out.outcome {
                    error.in_stream = true;
                    return Ok(out);
                }
                return Ok(in_stream_outcome(&v["response"]["error"]));
            }
            "error" => {
                let e = if v["error"].is_object() { &v["error"] } else { &v };
                return Ok(in_stream_outcome(e));
            }
            _ => {}
        }
    }
    Err("the Responses stream ended without response.completed".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_over_the_cap_is_not_recorded() {
        let mut r = Reassembler::new(Protocol::OpenaiChat);
        let chunk = vec![b' '; MAX_BODY / 2 + 1];
        r.push(&chunk);
        r.push(&chunk);
        assert!(r.buf.is_empty());
        let err = r.finish(200, &http::HeaderMap::new()).unwrap_err();
        assert!(err.contains("not recorded"), "{err}");
    }

    fn headers(ct: &str) -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        h.insert(http::header::CONTENT_TYPE, ct.parse().unwrap());
        h
    }

    /// Feeds `body` in pieces of `n` bytes.
    fn run(protocol: Protocol, body: &str, n: usize, status: u16, ct: &str) -> Result<Reassembled, String> {
        let mut r = Reassembler::new(protocol);
        for piece in body.as_bytes().chunks(n) {
            r.push(piece);
        }
        r.finish(status, &headers(ct))
    }

    fn reply(r: Reassembled) -> Reply {
        match r.outcome {
            CassetteOutcome::Reply { reply } => reply,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    const CHAT: &str = include_str!("../../tests/fixtures/sse/chat.sse");
    const ANTHROPIC: &str = include_str!("../../tests/fixtures/sse/anthropic.sse");
    const RESPONSES: &str = include_str!("../../tests/fixtures/sse/responses.sse");

    #[test]
    fn chat_stream_at_any_split() {
        for n in [1, 2, 7, 64, 100_000] {
            let r = reply(run(Protocol::OpenaiChat, CHAT, n, 200, "text/event-stream; charset=utf-8").unwrap());
            assert_eq!(r.text.as_deref(), Some("Hello, wörld"), "n={n}");
            assert_eq!(r.finish, FinishReason::ToolCalls);
            assert_eq!(r.tool_calls.len(), 1);
            assert_eq!(r.tool_calls[0].id, "call_1");
            assert_eq!(r.tool_calls[0].name, "get_weather");
            assert_eq!(r.tool_calls[0].arguments, r#"{"city":"Paris"}"#);
            assert_eq!(r.reasoning.as_deref(), Some("thinking"));
            assert_eq!(
                r.usage,
                Usage {
                    prompt_tokens: 16,
                    completion_tokens: 30,
                    reasoning_tokens: 4,
                    cached_tokens: 8,
                    cache_creation_tokens: 0
                }
            );
        }
    }

    #[test]
    fn chat_stream_without_finish_is_incomplete() {
        let cut = &CHAT[..CHAT.find("\"finish_reason\":\"tool_calls\"").unwrap()];
        assert!(run(Protocol::OpenaiChat, cut, 50, 200, "text/event-stream").is_err());
    }

    #[test]
    fn chat_in_stream_error() {
        let body = "data: {\"error\":{\"message\":\"slow down\",\"type\":\"rate_limit_exceeded\",\"code\":\"rate_limit_exceeded\"}}\r\n\r\ndata: [DONE]\r\n\r\n";
        let CassetteOutcome::Error { error } = run(Protocol::OpenaiChat, body, 5, 200, "text/event-stream").unwrap().outcome else {
            panic!()
        };
        assert_eq!((error.status, error.in_stream, error.message.as_str()), (429, true, "slow down"));
    }

    #[test]
    fn anthropic_stream_at_any_split() {
        for n in [1, 3, 11, 100_000] {
            let r = reply(run(Protocol::AnthropicMessages, ANTHROPIC, n, 200, "text/event-stream").unwrap());
            assert_eq!(r.text.as_deref(), Some("Let me check."), "n={n}");
            assert_eq!(r.reasoning.as_deref(), Some("Hmm."));
            assert_eq!(r.reasoning_opaque.as_deref(), Some("sig123"));
            assert_eq!(r.tool_calls.len(), 1);
            assert_eq!(r.tool_calls[0].id, "toolu_1");
            assert_eq!(r.tool_calls[0].arguments, r#"{"city":"Paris"}"#);
            assert_eq!(r.finish, FinishReason::ToolCalls);
            assert_eq!(
                r.usage,
                Usage {
                    prompt_tokens: 25 + 10 + 5,
                    completion_tokens: 42,
                    reasoning_tokens: 0,
                    cached_tokens: 10,
                    cache_creation_tokens: 5
                }
            );
        }
    }

    #[test]
    fn anthropic_stream_without_stop_and_error_event() {
        let cut = &ANTHROPIC[..ANTHROPIC.find("event: message_stop").unwrap()];
        assert!(run(Protocol::AnthropicMessages, cut, 40, 200, "text/event-stream").is_err());
        let body = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        let CassetteOutcome::Error { error } = run(Protocol::AnthropicMessages, body, 9, 200, "text/event-stream").unwrap().outcome else {
            panic!()
        };
        assert_eq!(
            (error.status, error.kind.as_str(), error.in_stream),
            (529, "overloaded_error", true)
        );
    }

    #[test]
    fn responses_stream_at_any_split() {
        for n in [1, 5, 13, 100_000] {
            let out = run(Protocol::OpenaiResponses, RESPONSES, n, 200, "text/event-stream").unwrap();
            assert_eq!(out.response.as_ref().unwrap()["id"], "resp_abc", "n={n}");
            let r = reply(out);
            assert_eq!(r.text.as_deref(), Some("Hi there"));
            assert_eq!(r.reasoning.as_deref(), Some("First.\n\nSecond."));
            assert_eq!(r.reasoning_opaque.as_deref(), Some("enc=="));
            assert_eq!(r.tool_calls.len(), 1);
            assert_eq!(r.tool_calls[0].id, "call_9");
            assert_eq!(r.tool_calls[0].arguments, r#"{"a":1}"#);
            assert_eq!(r.finish, FinishReason::ToolCalls);
            assert_eq!(
                r.usage,
                Usage {
                    prompt_tokens: 12,
                    completion_tokens: 20,
                    reasoning_tokens: 6,
                    cached_tokens: 3,
                    cache_creation_tokens: 0
                }
            );
        }
    }

    #[test]
    fn responses_stream_without_final_event_and_failed() {
        let cut = &RESPONSES[..RESPONSES.find("event: response.completed").unwrap()];
        assert!(run(Protocol::OpenaiResponses, cut, 30, 200, "text/event-stream").is_err());
        let body = "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"r\",\"status\":\"failed\",\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"too fast\"}}}\n\n";
        let CassetteOutcome::Error { error } = run(Protocol::OpenaiResponses, body, 8, 200, "text/event-stream").unwrap().outcome else {
            panic!()
        };
        assert_eq!((error.status, error.in_stream, error.message.as_str()), (429, true, "too fast"));
    }

    #[test]
    fn sync_bodies() {
        let chat = r#"{"choices":[{"message":{"content":"yo","tool_calls":[{"id":"c","function":{"name":"f","arguments":"{}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":3,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":1}}}"#;
        let r = reply(run(Protocol::OpenaiChat, chat, 1000, 200, "application/json").unwrap());
        assert_eq!((r.text.as_deref(), r.tool_calls.len(), r.usage.cached_tokens), (Some("yo"), 1, 1));

        let msg = r#"{"content":[{"type":"thinking","thinking":"t","signature":"s"},{"type":"text","text":"a"},{"type":"text","text":"b"},{"type":"tool_use","id":"u","name":"n","input":{"x": 1}}],"stop_reason":"tool_use","usage":{"input_tokens":5,"output_tokens":6,"cache_read_input_tokens":2}}"#;
        let r = reply(run(Protocol::AnthropicMessages, msg, 1000, 200, "application/json").unwrap());
        assert_eq!(r.text.as_deref(), Some("ab"));
        assert_eq!(r.tool_calls[0].arguments, r#"{"x":1}"#);
        assert_eq!(
            (r.usage.prompt_tokens, r.usage.cached_tokens, r.reasoning_opaque.as_deref()),
            (7, 2, Some("s"))
        );

        let emb = r#"{"data":[{"embedding":[0.1]}],"usage":{"prompt_tokens":9,"total_tokens":9}}"#;
        let r = reply(run(Protocol::OpenaiEmbeddings, emb, 1000, 200, "application/json").unwrap());
        assert_eq!((r.text, r.usage.prompt_tokens), (None, 9));

        let resp = r#"{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"message","content":[{"type":"output_text","text":"cut"}]}]}"#;
        let out = run(Protocol::OpenaiResponses, resp, 1000, 200, "application/json").unwrap();
        assert!(out.response.is_some());
        assert_eq!(reply(out).finish, FinishReason::Length);

        assert!(run(Protocol::OpenaiChat, "<html>", 10, 200, "text/html").is_err());
    }

    #[test]
    fn error_bodies() {
        let mut h = headers("application/json");
        h.insert("retry-after", "7".parse().unwrap());
        h.insert("retry-after-ms", "7000".parse().unwrap());
        h.insert("x-should-retry", "true".parse().unwrap());
        let mut r = Reassembler::new(Protocol::OpenaiChat);
        r.push(br#"{"error":{"message":"slow","type":"requests","code":"rate_limit_exceeded","param":null}}"#);
        let CassetteOutcome::Error { error } = r.finish(429, &h).unwrap().outcome else {
            panic!()
        };
        assert_eq!(error.status, 429);
        assert_eq!(error.kind, "requests");
        assert_eq!(error.code.as_deref(), Some("rate_limit_exceeded"));
        assert_eq!(error.retry_after, Some(Duration::from_secs(7)));
        assert_eq!(error.headers.len(), 2);

        let e = error_from_body(
            Protocol::AnthropicMessages,
            529,
            br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            &http::HeaderMap::new(),
        );
        assert_eq!(
            (e.kind.as_str(), e.message.as_str(), e.code),
            ("overloaded_error", "Overloaded", None)
        );

        let e = error_from_body(Protocol::OpenaiChat, 502, b"<html>Bad Gateway</html>", &http::HeaderMap::new());
        assert_eq!((e.status, e.message.as_str()), (502, "<html>Bad Gateway</html>"));
    }
}
