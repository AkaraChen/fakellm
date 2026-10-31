//! The Responses SSE event sequence. Wire rules are in docs/m1-spec.md ("OpenAI Responses").
//!
//! Every event carries `event: <type>` and a `sequence_number` that increases by one over the
//! whole stream; clients (codex, the official SDK) key on both. Item payloads are taken from the
//! same [`responses::output_items`] the sync body uses, so the two cannot drift apart.

use serde_json::{Map, Value, json};

use crate::ir::{ApiError, Reply, Request};
use crate::openai::responses::{self, ResponseMeta};
use crate::sse::{Frame, FrameKind};
use crate::tokenizer;

/// Numbers events and assigns pacing kinds: everything before the first delta is `Head`, the
/// rest that is not a delta is `Trailer`.
struct Builder {
    frames: Vec<Frame>,
    seen_delta: bool,
}

impl Builder {
    fn event(&mut self, kind: &str, fields: Value) {
        self.push(kind, fields, false);
    }

    fn delta(&mut self, kind: &str, fields: Value) {
        self.push(kind, fields, true);
        self.seen_delta = true;
    }

    fn push(&mut self, kind: &str, fields: Value, content: bool) {
        let mut data = Map::new();
        data.insert("type".into(), kind.into());
        data.insert("sequence_number".into(), self.frames.len().into());
        if let Value::Object(f) = fields {
            data.extend(f);
        }
        let pace = match (content, self.seen_delta) {
            (true, _) => FrameKind::Content,
            (false, true) => FrameKind::Trailer,
            (false, false) => FrameKind::Head,
        };
        self.frames.push(Frame::json(pace, Some(kind), &Value::Object(data)));
    }
}

fn pieces(text: &str, chunk_tokens: usize) -> Vec<String> {
    tokenizer::tokenize(text).chunks(chunk_tokens.max(1)).map(|c| c.concat()).collect()
}

/// Pieces of at most `max` bytes, split on char boundaries.
fn split_bytes(s: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let mut end = max.max(1).min(rest.len());
        while !rest.is_char_boundary(end) {
            end += 1;
        }
        let (a, b) = rest.split_at(end);
        out.push(a);
        rest = b;
    }
    out
}

/// The item as `response.output_item.added` shows it: final identity, empty payload.
fn added_item(item: &Value) -> Value {
    let mut v = item.clone();
    match item["type"].as_str() {
        Some("message") => {
            v["status"] = "in_progress".into();
            v["content"] = json!([]);
        }
        Some("function_call") => {
            v["status"] = "in_progress".into();
            v["arguments"] = "".into();
        }
        Some("custom_tool_call") => {
            v["status"] = "in_progress".into();
            v["input"] = "".into();
        }
        Some("reasoning") => v["summary"] = json!([]),
        _ => {}
    }
    v
}

/// The whole event sequence for a streamed reply, in order, ending in `response.completed` (or
/// `response.incomplete` when `finish == Length`). Every frame has `event: <type>` and
/// `data: {"type": "<type>", "sequence_number": n, ...}` with `n` counting from 0.
/// Frame kinds: everything before the first delta is `Head`, deltas are `Content`, the rest is
/// `Trailer`.
pub fn frames(req: &Request, reply: &Reply, meta: &ResponseMeta, chunk_tokens: usize, args_chunk_bytes: usize) -> Vec<Frame> {
    let mut b = Builder {
        frames: Vec::new(),
        seen_delta: false,
    };
    let created = responses::response_object(req, None, meta, "in_progress", &[], None);
    b.event("response.created", json!({"response": created}));
    b.event("response.in_progress", json!({"response": created}));

    let items = responses::output_items(req, reply, meta);
    for (output_index, item) in items.iter().enumerate() {
        let item_id = item["id"].clone();
        b.event(
            "response.output_item.added",
            json!({"output_index": output_index, "item": added_item(item)}),
        );
        match item["type"].as_str() {
            Some("reasoning") => {
                let text = item["summary"][0]["text"].as_str().unwrap_or_default();
                let at = json!({"item_id": item_id, "output_index": output_index, "summary_index": 0});
                let with = |extra: Value| {
                    let mut v = at.clone();
                    v.as_object_mut()
                        .expect("object")
                        .extend(extra.as_object().cloned().unwrap_or_default());
                    v
                };
                b.event(
                    "response.reasoning_summary_part.added",
                    with(json!({"part": {"type": "summary_text", "text": ""}})),
                );
                for piece in pieces(text, chunk_tokens) {
                    b.delta("response.reasoning_summary_text.delta", with(json!({"delta": piece})));
                }
                b.event("response.reasoning_summary_text.done", with(json!({"text": text})));
                b.event(
                    "response.reasoning_summary_part.done",
                    with(json!({"part": {"type": "summary_text", "text": text}})),
                );
            }
            Some("message") => {
                for (content_index, part) in item["content"].as_array().into_iter().flatten().enumerate() {
                    let at = json!({"item_id": item_id, "output_index": output_index, "content_index": content_index});
                    let with = |extra: Value| {
                        let mut v = at.clone();
                        v.as_object_mut()
                            .expect("object")
                            .extend(extra.as_object().cloned().unwrap_or_default());
                        v
                    };
                    if part["type"] == "refusal" {
                        let text = part["refusal"].as_str().unwrap_or_default();
                        b.event(
                            "response.content_part.added",
                            with(json!({"part": {"type": "refusal", "refusal": ""}})),
                        );
                        for piece in pieces(text, chunk_tokens) {
                            b.delta("response.refusal.delta", with(json!({"delta": piece})));
                        }
                        b.event("response.refusal.done", with(json!({"refusal": text})));
                    } else {
                        let text = part["text"].as_str().unwrap_or_default();
                        b.event(
                            "response.content_part.added",
                            with(json!({"part": {"type": "output_text", "text": "", "annotations": [], "logprobs": []}})),
                        );
                        for piece in pieces(text, chunk_tokens) {
                            b.delta("response.output_text.delta", with(json!({"delta": piece, "logprobs": []})));
                        }
                        b.event("response.output_text.done", with(json!({"text": text, "logprobs": []})));
                    }
                    b.event("response.content_part.done", with(json!({"part": part})));
                }
            }
            Some("function_call") => {
                let args = item["arguments"].as_str().unwrap_or_default();
                let at = json!({"item_id": item_id, "output_index": output_index});
                let fragments = if args.is_empty() {
                    vec![""]
                } else {
                    split_bytes(args, args_chunk_bytes)
                };
                for f in fragments {
                    let mut v = at.clone();
                    v["delta"] = f.into();
                    b.delta("response.function_call_arguments.delta", v);
                }
                let mut v = at.clone();
                v["name"] = item["name"].clone();
                v["arguments"] = args.into();
                b.event("response.function_call_arguments.done", v);
            }
            Some("custom_tool_call") => {
                let input = item["input"].as_str().unwrap_or_default();
                let at = json!({"item_id": item_id, "output_index": output_index});
                let fragments = if input.is_empty() {
                    vec![""]
                } else {
                    split_bytes(input, args_chunk_bytes)
                };
                for f in fragments {
                    let mut v = at.clone();
                    v["delta"] = f.into();
                    b.delta("response.custom_tool_call_input.delta", v);
                }
                let mut v = at.clone();
                v["input"] = input.into();
                b.event("response.custom_tool_call_input.done", v);
            }
            _ => {}
        }
        b.event("response.output_item.done", json!({"output_index": output_index, "item": item}));
    }

    let status = responses::reply_status(reply);
    let done = responses::response_object(req, Some(reply), meta, status, &items, None);
    let kind = if status == "completed" {
        "response.completed"
    } else {
        "response.incomplete"
    };
    b.event(kind, json!({"response": done}));
    b.frames
}

/// An in-stream error: `response.created`, then `response.failed` with `response.status ==
/// "failed"` and `response.error {code, message}`. All `Head`.
pub fn error_frames(req: &Request, err: &ApiError, meta: &ResponseMeta) -> Vec<Frame> {
    let mut b = Builder {
        frames: Vec::new(),
        seen_delta: false,
    };
    b.event(
        "response.created",
        json!({"response": responses::response_object(req, None, meta, "in_progress", &[], None)}),
    );
    let mut error = json!({"code": error_code(err), "message": err.message});
    let extra = crate::error_headers(err);
    if !extra.is_empty() {
        let headers: Map<String, Value> = extra.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v.into())).collect();
        error["headers"] = Value::Object(headers);
    }
    b.event(
        "response.failed",
        json!({"response": responses::response_object(req, None, meta, "failed", &[], Some(error))}),
    );
    b.frames
}

/// The `response.error.code` for an IR error: `err.code` when set, otherwise by `err.kind` and
/// status (see the spec's table).
pub fn error_code(err: &ApiError) -> String {
    if let Some(c) = &err.code {
        return c.clone();
    }
    match err.status {
        _ if err.kind == "insufficient_quota" => "insufficient_quota",
        429 => "rate_limit_exceeded",
        503 | 529 => "server_is_overloaded",
        s if s >= 500 => "server_error",
        _ => return err.kind.clone(),
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ir::{FinishReason, ToolCall, Usage};

    fn meta() -> ResponseMeta {
        ResponseMeta {
            id: "resp_1".into(),
            created: 1_700_000_000,
            model: "gpt-5.5".into(),
            seed: 3,
        }
    }

    fn req(body: Value) -> Request {
        responses::parse(&body, &Default::default()).unwrap()
    }

    fn reply() -> Reply {
        Reply {
            usage: Usage {
                prompt_tokens: 8,
                completion_tokens: 4,
                ..Usage::default()
            },
            ..Reply::default()
        }
    }

    fn types(frames: &[Frame]) -> Vec<String> {
        frames.iter().map(|f| f.event.clone().unwrap()).collect()
    }

    fn data(f: &Frame) -> Value {
        serde_json::from_str(&f.data).unwrap()
    }

    #[test]
    fn text_sequence() {
        let r = Reply {
            text: Some("Hello there".into()),
            ..reply()
        };
        let fr = frames(&req(json!({"model": "m", "input": "hi"})), &r, &meta(), 1, 16);
        let t = types(&fr);
        assert_eq!(
            &t[..6],
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta"
            ]
        );
        assert_eq!(
            &t[6..],
            [
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed"
            ]
        );
        for (i, f) in fr.iter().enumerate() {
            let d = data(f);
            assert_eq!(d["sequence_number"], i);
            assert_eq!(d["type"], t[i]);
        }
        assert_eq!(data(&fr[0])["response"]["status"], "in_progress");
        assert_eq!(data(&fr[0])["response"]["id"], "resp_1");
        assert_eq!(data(&fr[0])["response"]["output"], json!([]));
        assert!(data(&fr[0])["response"]["usage"].is_null());
        assert_eq!(data(&fr[2])["item"]["status"], "in_progress");
        assert_eq!(data(&fr[4])["delta"], "Hello");
        assert_eq!(data(&fr[6])["text"], "Hello there");
        let kinds: Vec<FrameKind> = fr.iter().map(|f| f.kind).collect();
        assert!(kinds[..4].iter().all(|k| *k == FrameKind::Head));
        assert_eq!(kinds[4], FrameKind::Content);
        assert_eq!(kinds[5], FrameKind::Content);
        assert!(kinds[6..].iter().all(|k| *k == FrameKind::Trailer));
        let last = data(fr.last().unwrap());
        assert_eq!(last["response"]["status"], "completed");
        assert_eq!(last["response"]["model"], "gpt-5.5");
        assert!(last["response"]["created_at"].is_number());
        assert_eq!(last["response"]["usage"]["input_tokens"], 8);
        assert_eq!(last["response"]["usage"]["output_tokens"], 4);
        assert_eq!(last["response"]["usage"]["total_tokens"], 12);
        assert!(last["response"]["usage"]["input_tokens_details"].is_object());
        assert!(last["response"]["usage"]["output_tokens_details"].is_object());
        assert_eq!(last["response"]["output"][0]["content"][0]["text"], "Hello there");
    }

    #[test]
    fn function_call_sequence() {
        let r = Reply {
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "shell".into(),
                arguments: r#"{"command":["ls","-la"]}"#.into(),
            }],
            finish: FinishReason::ToolCalls,
            ..reply()
        };
        let fr = frames(&req(json!({"model": "m", "input": "hi"})), &r, &meta(), 1, 10);
        let t = types(&fr);
        assert_eq!(t[2], "response.output_item.added");
        assert!(t[3..t.len() - 3].iter().all(|x| x == "response.function_call_arguments.delta"));
        assert_eq!(t[t.len() - 3], "response.function_call_arguments.done");
        assert_eq!(data(&fr[2])["item"]["arguments"], "");
        assert_eq!(data(&fr[2])["item"]["call_id"], "call_1");
        let joined: String = fr
            .iter()
            .filter(|f| f.event.as_deref() == Some("response.function_call_arguments.delta"))
            .map(|f| data(f)["delta"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(joined, r#"{"command":["ls","-la"]}"#);
        let done = data(&fr[fr.len() - 3]);
        assert_eq!(done["name"], "shell");
        assert_eq!(done["arguments"], joined);
        let item_done = data(&fr[fr.len() - 2]);
        assert_eq!(item_done["item"]["status"], "completed");
        assert_eq!(item_done["item"]["name"], "shell");
        assert_eq!(item_done["item"]["arguments"], joined);
    }

    #[test]
    fn custom_tool_call_sequence() {
        let r = Reply {
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "apply_patch".into(),
                arguments: "0123456789abcdef".into(),
            }],
            finish: FinishReason::ToolCalls,
            ..reply()
        };
        let fr = frames(
            &req(json!({"model": "m", "input": "hi", "tools": [{"type": "custom", "name": "apply_patch"}]})),
            &r,
            &meta(),
            1,
            10,
        );
        let t = types(&fr);
        assert_eq!(t.iter().filter(|x| *x == "response.custom_tool_call_input.delta").count(), 2);
        assert!(t.iter().any(|x| x == "response.custom_tool_call_input.done"));
        let added = fr
            .iter()
            .find(|f| f.event.as_deref() == Some("response.output_item.added") && data(f)["item"]["type"] == "custom_tool_call");
        assert_eq!(data(added.unwrap())["item"]["input"], "");
        let done = fr
            .iter()
            .find(|f| f.event.as_deref() == Some("response.custom_tool_call_input.done"))
            .unwrap();
        assert_eq!(data(done)["input"], "0123456789abcdef");
    }

    #[test]
    fn reasoning_then_text() {
        let r = Reply {
            reasoning: Some("think hard".into()),
            text: Some("done".into()),
            ..reply()
        };
        let rq = req(json!({"model": "m", "input": "hi", "include": ["reasoning.encrypted_content"]}));
        let fr = frames(&rq, &r, &meta(), 1, 16);
        let t = types(&fr);
        assert_eq!(t[2], "response.output_item.added");
        assert_eq!(t[3], "response.reasoning_summary_part.added");
        assert_eq!(t[4], "response.reasoning_summary_text.delta");
        assert!(t.contains(&"response.reasoning_summary_text.done".to_owned()));
        assert!(t.contains(&"response.reasoning_summary_part.done".to_owned()));
        let item = data(&fr[2]);
        assert_eq!(item["item"]["type"], "reasoning");
        assert_eq!(item["item"]["summary"], json!([]));
        let first_text = t.iter().position(|x| x == "response.output_text.delta").unwrap();
        assert_eq!(data(&fr[first_text])["output_index"], 1);
        let last = data(fr.last().unwrap());
        assert_eq!(last["response"]["output"][0]["type"], "reasoning");
        assert_eq!(last["response"]["output"][0]["summary"][0]["text"], "think hard");
        assert!(last["response"]["output"][0]["encrypted_content"].is_string());
        assert_eq!(last["response"]["output"][1]["type"], "message");
    }

    #[test]
    fn incomplete_on_length() {
        let r = Reply {
            text: Some("cut off".into()),
            finish: FinishReason::Length,
            ..reply()
        };
        let fr = frames(&req(json!({"model": "m", "input": "hi"})), &r, &meta(), 1, 16);
        assert_eq!(fr.last().unwrap().event.as_deref(), Some("response.incomplete"));
        let d = data(fr.last().unwrap());
        assert_eq!(d["response"]["status"], "incomplete");
        assert_eq!(d["response"]["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn failed_stream_and_codes() {
        let err = ApiError::new(429, "requests", "slow down")
            .retry_after(Duration::from_millis(1500))
            .header("X-Codex-Active-Limit", "x");
        let fr = error_frames(&req(json!({"model": "m", "input": "hi"})), &err, &meta());
        assert_eq!(types(&fr), ["response.created", "response.failed"]);
        assert!(fr.iter().all(|f| f.kind == FrameKind::Head));
        let d = data(&fr[1]);
        assert_eq!(d["sequence_number"], 1);
        assert_eq!(d["response"]["status"], "failed");
        assert_eq!(d["response"]["error"]["code"], "rate_limit_exceeded");
        assert_eq!(d["response"]["error"]["message"], "slow down");
        assert_eq!(d["response"]["error"]["headers"]["retry-after"], "2");
        assert_eq!(d["response"]["error"]["headers"]["x-codex-active-limit"], "x");
        assert_eq!(d["response"]["output"], json!([]));

        assert_eq!(error_code(&ApiError::insufficient_quota().with_status(429)), "insufficient_quota");
        assert_eq!(error_code(&ApiError::new(429, "insufficient_quota", "q")), "insufficient_quota");
        assert_eq!(error_code(&ApiError::overloaded()), "server_is_overloaded");
        assert_eq!(error_code(&ApiError::new(529, "overloaded_error", "o")), "server_is_overloaded");
        assert_eq!(error_code(&ApiError::new(500, "api_error", "x")), "server_error");
        assert_eq!(
            error_code(&ApiError::new(400, "invalid_request_error", "x")),
            "invalid_request_error"
        );
        assert_eq!(error_code(&ApiError::context_length_exceeded(10, 20)), "context_length_exceeded");
    }
}
