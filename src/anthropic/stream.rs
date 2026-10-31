//! The Messages SSE event sequence. Wire rules are in docs/m1-spec.md ("Anthropic Messages").
//!
//! The whole sequence is built up front as `Frame`s; `sse::respond` only paces them. Kinds follow
//! the shared rule: everything before the first delta is `Head`, deltas are `Content`, what comes
//! after is `Trailer`.

use serde_json::{Value, json};

use crate::anthropic::messages::{Block, MessageMeta, blocks, input_tokens, stop_reason};
use crate::ir::{ApiError, Reply, Request};
use crate::sse::{Frame, FrameKind};

fn message_start(meta: &MessageMeta, input: u32, cache_creation: u32, cache_read: u32) -> Value {
    json!({"type": "message_start", "message": {
        "id": meta.id,
        "type": "message",
        "role": "assistant",
        "model": meta.model,
        "content": [],
        "stop_reason": null,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input,
            "cache_creation_input_tokens": cache_creation,
            "cache_read_input_tokens": cache_read,
            "output_tokens": 1,
        },
    }})
}

/// The whole event sequence for a streamed reply, in order: `message_start`, per content
/// block `content_block_start` / `content_block_delta`* / `content_block_stop` (`ping` follows the first
/// `content_block_start`), `message_delta`,
/// `message_stop`. Every frame has `event: <type>` and `data: {"type": "<type>", ...}`.
/// Frame kinds: everything before the first delta is `Head`, deltas are `Content`, the rest is
/// `Trailer`. Text and reasoning deltas hold `chunk_tokens` tokens each; tool-argument
/// `input_json_delta` fragments hold `args_chunk_bytes` bytes each.
pub fn frames(req: &Request, reply: &Reply, meta: &MessageMeta, chunk_tokens: usize, args_chunk_bytes: usize) -> Vec<Frame> {
    let chunk_tokens = chunk_tokens.max(1);
    let args_chunk_bytes = args_chunk_bytes.max(1);
    let usage = &reply.usage;

    // (event body, is a token-carrying delta)
    let mut events: Vec<(Value, bool)> = vec![(
        message_start(meta, input_tokens(usage), usage.cache_creation_tokens, usage.cached_tokens),
        false,
    )];
    let delta = |index: usize, delta: Value| (json!({"type": "content_block_delta", "index": index, "delta": delta}), true);

    for (index, block) in blocks(req, reply).into_iter().enumerate() {
        match block {
            Block::Thinking { text, signature } => {
                events.push((
                    json!({"type": "content_block_start", "index": index, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
                    false,
                ));
                for group in crate::tokenizer::tokenize(text).chunks(chunk_tokens) {
                    events.push(delta(index, json!({"type": "thinking_delta", "thinking": group.concat()})));
                }
                events.push(delta(index, json!({"type": "signature_delta", "signature": signature})));
            }
            Block::Text(text) => {
                events.push((
                    json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}),
                    false,
                ));
                for group in crate::tokenizer::tokenize(&text).chunks(chunk_tokens) {
                    events.push(delta(index, json!({"type": "text_delta", "text": group.concat()})));
                }
            }
            Block::ToolUse(call) => {
                events.push((
                    json!({"type": "content_block_start", "index": index, "content_block": {
                        "type": "tool_use", "id": call.id, "name": call.name, "input": {}}}),
                    false,
                ));
                let frags = split_bytes(&call.arguments, args_chunk_bytes);
                if frags.is_empty() {
                    events.push(delta(index, json!({"type": "input_json_delta", "partial_json": ""})));
                }
                for frag in frags {
                    events.push(delta(index, json!({"type": "input_json_delta", "partial_json": frag})));
                }
            }
        }
        events.push((json!({"type": "content_block_stop", "index": index}), false));
    }

    events.push((
        json!({"type": "message_delta",
        "delta": {"stop_reason": stop_reason(reply.finish), "stop_sequence": null},
        "usage": {
            "input_tokens": input_tokens(usage),
            "cache_creation_input_tokens": usage.cache_creation_tokens,
            "cache_read_input_tokens": usage.cached_tokens,
            "output_tokens": usage.completion_tokens,
        }}),
        false,
    ));
    events.push((json!({"type": "message_stop"}), false));
    // The real API sends `ping` right after the first `content_block_start`.
    events.insert(2, (json!({"type": "ping"}), false));

    let mut seen_delta = false;
    events
        .into_iter()
        .map(|(value, is_delta)| {
            let kind = match (is_delta, seen_delta) {
                (true, _) => FrameKind::Content,
                (false, false) => FrameKind::Head,
                (false, true) => FrameKind::Trailer,
            };
            seen_delta |= is_delta;
            let event = value["type"].as_str().unwrap_or_default().to_owned();
            Frame::json(kind, Some(&event), &value)
        })
        .collect()
}

/// An in-stream error: `message_start` (with `input_tokens` in its usage), `ping`, then
/// `event: error` with `data: {"type": "error", "error": {"type", "message"}}`. All `Head`.
pub fn error_frames(err: &ApiError, meta: &MessageMeta, input_tokens: u32) -> Vec<Frame> {
    vec![
        Frame::json(FrameKind::Head, Some("message_start"), &message_start(meta, input_tokens, 0, 0)),
        Frame::json(FrameKind::Head, Some("ping"), &json!({"type": "ping"})),
        Frame::json(FrameKind::Head, Some("error"), &super::error::body(err)),
    ]
}

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
    use crate::ir::{FinishReason, ToolCall, Usage};

    fn meta() -> MessageMeta {
        MessageMeta {
            id: "msg_1".into(),
            model: "claude-sonnet-4-5".into(),
        }
    }

    fn req() -> Request {
        Request::new("m", vec![])
    }

    fn shape(frames: &[Frame]) -> Vec<(String, FrameKind)> {
        frames.iter().map(|f| (f.event.clone().unwrap(), f.kind)).collect()
    }

    fn data(f: &Frame) -> Value {
        serde_json::from_str(&f.data).unwrap()
    }

    fn names(frames: &[Frame]) -> Vec<String> {
        frames.iter().map(|f| f.event.clone().unwrap()).collect()
    }

    #[test]
    fn text_sequence() {
        let reply = Reply {
            text: Some("one two".into()),
            usage: Usage {
                prompt_tokens: 9,
                completion_tokens: 2,
                ..Usage::default()
            },
            ..Reply::default()
        };
        let fs = frames(&req(), &reply, &meta(), 1, 16);
        assert_eq!(
            shape(&fs),
            [
                ("message_start", FrameKind::Head),
                ("content_block_start", FrameKind::Head),
                ("ping", FrameKind::Head),
                ("content_block_delta", FrameKind::Content),
                ("content_block_delta", FrameKind::Content),
                ("content_block_stop", FrameKind::Trailer),
                ("message_delta", FrameKind::Trailer),
                ("message_stop", FrameKind::Trailer),
            ]
            .map(|(e, k)| (e.to_owned(), k))
        );
        for f in &fs {
            assert_eq!(data(f)["type"], f.event.as_deref().unwrap());
        }
        let start = data(&fs[0]);
        assert_eq!(start["message"]["usage"]["input_tokens"], 9);
        assert_eq!(start["message"]["usage"]["output_tokens"], 1);
        assert_eq!(start["message"]["content"], json!([]));
        assert_eq!(data(&fs[1])["content_block"], json!({"type": "text", "text": ""}));
        let text: String = [3, 4]
            .iter()
            .map(|&i| data(&fs[i])["delta"]["text"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(text, "one two");
        assert_eq!(data(&fs[3])["delta"]["type"], "text_delta");
        let md = data(&fs[6]);
        assert_eq!(md["delta"], json!({"stop_reason": "end_turn", "stop_sequence": null}));
        assert_eq!(md["usage"]["output_tokens"], 2);
        assert_eq!(md["usage"]["input_tokens"], 9);
    }

    #[test]
    fn tool_use_sequence() {
        let reply = Reply {
            tool_calls: vec![
                ToolCall {
                    id: "toolu_1".into(),
                    name: "f".into(),
                    arguments: r#"{"city":"Paris"}"#.into(),
                },
                ToolCall {
                    id: "toolu_2".into(),
                    name: "g".into(),
                    arguments: String::new(),
                },
            ],
            finish: FinishReason::ToolCalls,
            ..Reply::default()
        };
        let fs = frames(&req(), &reply, &meta(), 1, 8);
        assert_eq!(
            names(&fs),
            [
                "message_start",
                "content_block_start",
                "ping",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(
            data(&fs[1])["content_block"],
            json!({"type": "tool_use", "id": "toolu_1", "name": "f", "input": {}})
        );
        assert_eq!(fs[1].kind, FrameKind::Head);
        assert_eq!(fs[6].kind, FrameKind::Trailer);
        let joined: String = [3, 4]
            .iter()
            .map(|&i| data(&fs[i])["delta"]["partial_json"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(joined, r#"{"city":"Paris"}"#);
        assert_eq!(data(&fs[3])["delta"]["type"], "input_json_delta");
        assert_eq!(data(&fs[7])["index"], 1);
        assert_eq!(data(&fs[7])["delta"]["partial_json"], "");
        assert_eq!(data(&fs[9])["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn thinking_then_text() {
        let reply = Reply {
            reasoning: Some("ab cd".into()),
            reasoning_opaque: Some("SIG".into()),
            text: Some("hi".into()),
            ..Reply::default()
        };
        let fs = frames(&req(), &reply, &meta(), 100, 16);
        assert_eq!(
            names(&fs),
            [
                "message_start",
                "content_block_start",
                "ping",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(
            data(&fs[1])["content_block"],
            json!({"type": "thinking", "thinking": "", "signature": ""})
        );
        assert_eq!(data(&fs[3])["delta"], json!({"type": "thinking_delta", "thinking": "ab cd"}));
        assert_eq!(data(&fs[4])["delta"], json!({"type": "signature_delta", "signature": "SIG"}));
        assert_eq!(fs[4].kind, FrameKind::Content);
        assert_eq!(data(&fs[6])["index"], 1);
        assert_eq!(fs[6].kind, FrameKind::Trailer);
    }

    #[test]
    fn stop_reason_in_message_delta() {
        for (finish, want) in [(FinishReason::Length, "max_tokens"), (FinishReason::ContentFilter, "refusal")] {
            let reply = Reply {
                text: Some("x".into()),
                finish,
                ..Reply::default()
            };
            let fs = frames(&req(), &reply, &meta(), 1, 16);
            assert_eq!(data(&fs[fs.len() - 2])["delta"]["stop_reason"], want);
        }
    }

    #[test]
    fn in_stream_error() {
        let err = ApiError::for_status_in(crate::ir::Protocol::AnthropicMessages, 529);
        let fs = frames_err(&err);
        assert_eq!(names(&fs), ["message_start", "ping", "error"]);
        assert!(fs.iter().all(|f| f.kind == FrameKind::Head));
        assert_eq!(data(&fs[0])["message"]["usage"]["input_tokens"], 12);
        assert_eq!(
            data(&fs[2]),
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})
        );
    }

    fn frames_err(err: &ApiError) -> Vec<Frame> {
        error_frames(err, &meta(), 12)
    }

    #[test]
    fn split_bytes_respects_char_boundaries() {
        assert_eq!(split_bytes("aé€b", 2), ["aé", "€", "b"]);
        assert!(split_bytes("", 4).is_empty());
    }
}
