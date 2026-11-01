//! Anthropic Messages over raw HTTP: sync body, SSE event order, tool_use and thinking streams,
//! count_tokens and the error shapes.

use serde_json::{Value, json};

mod common;
use common::*;

const H: &[(&str, &str)] = &[("anthropic-version", "2023-06-01"), ("x-api-key", "k")];

fn body(user: &str) -> Value {
    json!({"model": "claude-sonnet-4-5", "max_tokens": 256, "messages": [{"role": "user", "content": user}]})
}

fn names(events: &[SseEvent]) -> Vec<&str> {
    events.iter().map(|e| e.event.as_deref().unwrap()).collect()
}

#[tokio::test]
async fn sync_text() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "hello there"}}])).await;
    let r = post_json_with(&s, "/v1/messages", H, &body("hi")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let j = r.json();
    assert_eq!(j["type"], "message");
    assert_eq!(j["role"], "assistant");
    assert_eq!(j["content"][0], json!({"type": "text", "text": "hello there"}));
    assert_eq!(j["stop_reason"], "end_turn");
    assert!(j["usage"]["input_tokens"].as_u64().unwrap() > 0);
    assert!(j["usage"]["output_tokens"].as_u64().unwrap() > 0);
    assert!(j["id"].as_str().unwrap().starts_with("msg_"));
    assert!(r.header("request-id").is_some());
}

#[tokio::test]
async fn sse_event_order_with_event_lines() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "one two three"}}])).await;
    let mut b = body("hi");
    b["stream"] = json!(true);
    let mut sse = open_sse(&s, "/v1/messages", H, &b).await;
    assert_eq!(sse.status, 200);
    let ev = sse.events().await;
    let n = names(&ev);
    assert_eq!(&n[..3], ["message_start", "content_block_start", "ping"]);
    let tail = &n[n.len() - 3..];
    assert_eq!(tail, ["content_block_stop", "message_delta", "message_stop"]);
    assert!(n[3..n.len() - 3].iter().all(|e| *e == "content_block_delta"));
    assert!(!ev.iter().any(|e| e.data == "[DONE]"));
    for e in &ev {
        assert_eq!(e.json()["type"], e.event.as_deref().unwrap());
    }
    let text: String = ev
        .iter()
        .filter(|e| e.event.as_deref() == Some("content_block_delta"))
        .map(|e| e.json()["delta"]["text"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(text, "one two three");
    let md = ev[n.len() - 2].json();
    assert_eq!(md["delta"]["stop_reason"], "end_turn");
    assert!(md["usage"]["output_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn tool_use_input_json_delta_reassembles() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"reply": {"tool_calls": [{"name": "get_weather", "arguments": {"city": "Paris", "unit": "celsius"}}]}}]),
    )
    .await;
    let mut b = body("weather?");
    b["stream"] = json!(true);
    b["tools"] = json!([{"name": "get_weather", "description": "d", "input_schema": {"type": "object"}}]);
    let ev = open_sse(&s, "/v1/messages", H, &b).await.events().await;
    let start_ev = ev
        .iter()
        .find(|e| e.event.as_deref() == Some("content_block_start"))
        .unwrap()
        .json();
    assert_eq!(start_ev["content_block"]["type"], "tool_use");
    assert_eq!(start_ev["content_block"]["name"], "get_weather");
    assert!(start_ev["content_block"]["id"].as_str().unwrap().starts_with("toolu_"));
    let frags: Vec<String> = ev
        .iter()
        .filter(|e| e.event.as_deref() == Some("content_block_delta"))
        .map(|e| {
            let d = e.json();
            assert_eq!(d["delta"]["type"], "input_json_delta");
            d["delta"]["partial_json"].as_str().unwrap().to_owned()
        })
        .collect();
    assert!(frags.len() > 1, "{frags:?}");
    let args: Value = serde_json::from_str(&frags.concat()).unwrap();
    assert_eq!(args, json!({"city": "Paris", "unit": "celsius"}));
    let md = ev.iter().find(|e| e.event.as_deref() == Some("message_delta")).unwrap().json();
    assert_eq!(md["delta"]["stop_reason"], "tool_use");
}

#[tokio::test]
async fn thinking_block_with_signature_delta() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"reasoning": "let me think", "text": "answer"}}])).await;
    let mut b = body("hi");
    b["stream"] = json!(true);
    b["max_tokens"] = json!(2048);
    b["thinking"] = json!({"type": "enabled", "budget_tokens": 1024});
    let ev = open_sse(&s, "/v1/messages", H, &b).await.events().await;
    let starts: Vec<Value> = ev
        .iter()
        .filter(|e| e.event.as_deref() == Some("content_block_start"))
        .map(|e| e.json()["content_block"].clone())
        .collect();
    assert_eq!(starts[0]["type"], "thinking");
    assert_eq!(starts[1]["type"], "text");
    let deltas: Vec<Value> = ev
        .iter()
        .filter(|e| e.event.as_deref() == Some("content_block_delta"))
        .map(|e| e.json()["delta"].clone())
        .collect();
    let kinds: Vec<&str> = deltas.iter().map(|d| d["type"].as_str().unwrap()).collect();
    let sig = kinds.iter().position(|k| *k == "signature_delta").expect("signature_delta");
    assert!(kinds[..sig].iter().all(|k| *k == "thinking_delta"));
    assert!(kinds[sig + 1..].iter().all(|k| *k == "text_delta"));
    assert!(!deltas[sig]["signature"].as_str().unwrap().is_empty());
    let thinking: String = deltas.iter().filter_map(|d| d["thinking"].as_str()).collect();
    assert_eq!(thinking, "let me think");
}

#[tokio::test]
async fn count_tokens() {
    let s = start().await;
    let r = post_json_with(
        &s,
        "/v1/messages/count_tokens",
        H,
        &json!({"model": "claude-sonnet-4-5", "messages": [{"role": "user", "content": "hello world"}]}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let n = r.json()["input_tokens"].as_u64().unwrap();
    assert!(n > 0);
    let longer =
        json!({"model": "claude-sonnet-4-5", "messages": [{"role": "user", "content": "hello world, and a good deal more text to count"}]});
    let r2 = post_json_with(&s, "/v1/messages/count_tokens", H, &longer).await;
    assert!(r2.json()["input_tokens"].as_u64().unwrap() > n);
}

#[tokio::test]
async fn overloaded_529() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"error": {"status": 529}}}])).await;
    let r = post_json_with(&s, "/v1/messages", H, &body("hi")).await;
    assert_eq!(r.status, 529);
    let j = r.json();
    assert_eq!(j["type"], "error");
    assert_eq!(j["error"]["type"], "overloaded_error");
    assert!(j["request_id"].is_string() || j.get("request_id").is_none());
}

#[tokio::test]
async fn missing_max_tokens_is_400() {
    let s = start().await;
    let r = post_json_with(
        &s,
        "/v1/messages",
        H,
        &json!({"model": "claude-sonnet-4-5", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await;
    assert_eq!(r.status, 400);
    let j = r.json();
    assert_eq!(j["type"], "error");
    assert_eq!(j["error"]["type"], "invalid_request_error");
    assert!(j["error"]["message"].as_str().unwrap().contains("max_tokens"), "{j}");
}

#[tokio::test]
async fn in_stream_error_event() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"error": {"status": 529, "in_stream": true}}}])).await;
    let mut b = body("hi");
    b["stream"] = json!(true);
    let mut sse = open_sse(&s, "/v1/messages", H, &b).await;
    assert_eq!(sse.status, 200);
    let ev = sse.events().await;
    assert_eq!(names(&ev), ["message_start", "ping", "error"]);
    let e = ev[2].json();
    assert_eq!(e["type"], "error");
    assert_eq!(e["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn models_and_log_protocol() {
    let s = start().await;
    let r = send(&s, "GET", "/v1/models", H, "").await;
    assert_eq!(r.status, 200);
    post_json_with(&s, "/v1/messages", H, &body("hi")).await;
    let log = get(&s, "/_fakellm/requests?protocol=anthropic.messages").await.json();
    assert_eq!(log["requests"].as_array().map(|a| a.len()), Some(1), "{log}");
}

#[tokio::test]
async fn response_fidelity_fields() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"match": {"last_user": "sync"}, "reply": {"text": "ok"}},
               {"match": {"last_user": "noargs"}, "reply": {"tool_calls": [{"name": "ping", "arguments": {}}]}},
               {"reply": {"text": "ok"}}]),
    )
    .await;
    // Sync usage.
    let j = post_json_with(&s, "/v1/messages", H, &body("sync")).await.json();
    assert_eq!(j["usage"]["service_tier"], "standard");
    assert_eq!(j["usage"]["inference_geo"], "not_available");
    assert_eq!(j["usage"]["cache_creation"]["ephemeral_5m_input_tokens"], 0);
    assert_eq!(j["usage"]["cache_creation"]["ephemeral_1h_input_tokens"], 0);
    // Streamed: message_start usage, context_management only when asked.
    let mut b = body("hi");
    b["stream"] = json!(true);
    let ev = open_sse(&s, "/v1/messages", H, &b).await.events().await;
    let start_ev = ev.iter().find(|e| e.event.as_deref() == Some("message_start")).unwrap().json();
    assert_eq!(start_ev["message"]["usage"]["service_tier"], "standard");
    assert!(start_ev["message"]["usage"]["cache_creation"].is_object());
    let md = ev.iter().find(|e| e.event.as_deref() == Some("message_delta")).unwrap().json();
    assert!(md["context_management"].is_null());
    b["context_management"] = json!({"edits": []});
    let ev = open_sse(&s, "/v1/messages", H, &b).await.events().await;
    let md = ev.iter().find(|e| e.event.as_deref() == Some("message_delta")).unwrap().json();
    assert_eq!(md["context_management"], json!({"applied_edits": []}));
    // A tool without arguments streams one empty fragment, not `{}`.
    let mut b = body("noargs");
    b["stream"] = json!(true);
    b["tools"] = json!([{"name": "ping", "description": "d", "input_schema": {"type": "object"}}]);
    let ev = open_sse(&s, "/v1/messages", H, &b).await.events().await;
    let frags: Vec<String> = ev
        .iter()
        .filter(|e| e.event.as_deref() == Some("content_block_delta"))
        .map(|e| e.json()["delta"]["partial_json"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(frags, [""]);
}
