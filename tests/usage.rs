//! Rule `usage` overrides, `last_tool_call_id`, 404 model echo, in-stream error headers, and hang
//! visibility in the request log.

use std::time::Duration;

use serde_json::{Value, json};

mod common;
use common::*;

fn msg_body(user: &str) -> Value {
    json!({"model": "claude-x", "max_tokens": 64, "messages": [{"role": "user", "content": user}]})
}

#[tokio::test]
async fn usage_override_on_chat_and_messages() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"reply": {"text": "hello world", "usage": {"input_tokens": 20236, "output_tokens": 64, "cached_tokens": 100}}}]),
    )
    .await;
    let r = chat(&s, &chat_body("gpt-4o", "hi")).await.json();
    assert_eq!(r["usage"]["prompt_tokens"], 20236);
    assert_eq!(r["usage"]["completion_tokens"], 64);
    assert_eq!(r["usage"]["prompt_tokens_details"]["cached_tokens"], 100);
    let m = post_json(&s, "/v1/messages", &msg_body("hi")).await.json();
    assert_eq!(m["usage"]["output_tokens"], 64);
    // Literal: never reduced by cached_tokens.
    assert_eq!(m["usage"]["input_tokens"], 20236);
    assert_eq!(m["usage"]["cache_read_input_tokens"], 100);
}

async fn stream_usage(s: &fakellm::Server) -> (Value, Value) {
    let mut body = msg_body("hi");
    body["stream"] = json!(true);
    let events = open_sse(s, "/v1/messages", &[], &body).await.events().await;
    let data = |name: &str| -> Value {
        let e = events.iter().find(|e| e.event.as_deref() == Some(name)).unwrap();
        serde_json::from_str::<Value>(&e.data).unwrap()
    };
    (
        data("message_start")["message"]["usage"].clone(),
        data("message_delta")["usage"].clone(),
    )
}

#[tokio::test]
async fn anthropic_input_tokens_in_start_or_delta() {
    let s = start().await;
    let usage = json!({"input_tokens": 500, "output_tokens": 9, "cache_read_tokens": 100, "cache_creation_tokens": 20});
    put_rules(&s, json!([{"reply": {"text": "hello", "usage": usage}}])).await;
    let (start_u, delta_u) = stream_usage(&s).await;
    assert_eq!(start_u["input_tokens"], 500);
    assert_eq!(start_u["cache_read_input_tokens"], 100);
    assert_eq!(start_u["cache_creation_input_tokens"], 20);
    assert!(delta_u.get("input_tokens").is_none(), "{delta_u}");
    assert_eq!(delta_u["output_tokens"], 9);

    let mut usage = usage;
    usage["input_tokens_in"] = json!("delta");
    put_rules(&s, json!([{"reply": {"text": "hello", "usage": usage}}])).await;
    let (start_u, delta_u) = stream_usage(&s).await;
    assert!(start_u.get("input_tokens").is_none(), "{start_u}");
    assert_eq!(delta_u["input_tokens"], 500);
    assert_eq!(delta_u["cache_read_input_tokens"], 100);
    assert_eq!(delta_u["output_tokens"], 9);

    // count_tokens returns the rule's value as-is, whatever the cache counters say.
    let r = post_json(&s, "/v1/messages/count_tokens", &msg_body("hi")).await.json();
    assert_eq!(r["input_tokens"], 500);
}

#[tokio::test]
async fn usage_override_keeps_computed_fields() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "hello world", "usage": {"prompt_tokens": 7}}}])).await;
    let r = chat(&s, &chat_body("gpt-4o", "hi")).await.json();
    assert_eq!(r["usage"]["prompt_tokens"], 7);
    assert!(r["usage"]["completion_tokens"].as_u64().unwrap() > 0);
    let bad = send(
        &s,
        "PUT",
        "/_fakellm/scripts",
        &[],
        r#"[{"reply":{"text":"x","usage":{"bogus":1}}}]"#,
    )
    .await;
    assert_eq!(bad.status, 400);
}

#[tokio::test]
async fn count_tokens_consults_rules_with_usage() {
    let s = start().await;
    let plain = post_json(&s, "/v1/messages/count_tokens", &msg_body("hello there")).await.json();
    assert!(plain["input_tokens"].as_u64().unwrap() > 0);
    put_rules(
        &s,
        json!([
            {"name": "plain", "reply": {"text": "x"}, "times": 1},
            {"name": "ctx", "match": {"last_user": "hello"}, "reply": {"text": "x", "usage": {"input_tokens": 777}}, "times": 1}
        ]),
    )
    .await;
    let r = post_json(&s, "/v1/messages/count_tokens", &msg_body("hello there")).await.json();
    assert_eq!(r["input_tokens"], 777);
    let rules = get(&s, "/_fakellm/scripts").await.json();
    assert_eq!(
        (rules["rules"][0]["hits"].clone(), rules["rules"][1]["hits"].clone()),
        (json!(0), json!(0))
    );
    // Counting does not spend the rule: it still answers the count, and the real request still finds it.
    let again = post_json(&s, "/v1/messages/count_tokens", &msg_body("hello there")).await.json();
    assert_eq!(again["input_tokens"], 777);
    let real = post_json_with(&s, "/v1/messages", &[("anthropic-version", "2023-06-01")], &msg_body("hello there")).await;
    assert_eq!(real.status, 200, "{}", real.body);
    let rules = get(&s, "/_fakellm/scripts").await.json();
    assert_eq!(rules["rules"][0]["hits"], 1);
}

#[tokio::test]
async fn last_tool_call_id_matches_the_trailing_result() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"match": {"last_tool_call_id": "call_b"}, "reply": {"text": "B"}},
            {"match": {"last_tool_call_id": "call_a"}, "reply": {"text": "A"}},
            {"reply": {"text": "none"}}
        ]),
    )
    .await;
    let body = |id: &str| {
        json!({"model": "m", "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": null, "tool_calls": [{"id": id, "type": "function", "function": {"name": "f", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": id, "content": "ok"}
        ]})
    };
    for (id, want) in [("call_a", "A"), ("call_b", "B"), ("call_c", "none")] {
        assert_eq!(chat(&s, &body(id)).await.json()["choices"][0]["message"]["content"], want);
    }
    assert_eq!(
        chat(&s, &chat_body("m", "go")).await.json()["choices"][0]["message"]["content"],
        "none"
    );
}

#[tokio::test]
async fn bare_404_echoes_the_request_model() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"error": {"status": 404}}}])).await;
    let r = chat(&s, &chat_body("my-model", "hi")).await;
    assert_eq!(r.status, 404);
    let msg = r.json()["error"]["message"].as_str().unwrap().to_owned();
    assert!(msg.contains("`my-model`"), "{msg}");
    let r = post_json(&s, "/v1/responses", &json!({"model": "resp-model", "input": "hi"})).await;
    assert!(
        r.json()["error"]["message"].as_str().unwrap().contains("`resp-model`"),
        "{}",
        r.body
    );
    let r = post_json(&s, "/v1/messages", &msg_body("hi")).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["message"], "model: claude-x");
    assert_eq!(r.json()["error"]["type"], "not_found_error");
}

#[tokio::test]
async fn in_stream_error_headers_reach_the_http_response() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"reply": {"error": {"status": 529, "in_stream": true, "headers": {"x-should-retry": "true", "retry-after-ms": "1500"}}}}]),
    )
    .await;
    let mut chat_req = chat_body("gpt-4o", "hi");
    chat_req["stream"] = json!(true);
    let mut msg_req = msg_body("hi");
    msg_req["stream"] = json!(true);
    let resp_req = json!({"model": "m", "input": "hi", "stream": true});
    for (path, body) in [
        ("/v1/chat/completions", chat_req),
        ("/v1/messages", msg_req),
        ("/v1/responses", resp_req),
    ] {
        let mut sse = open_sse(&s, path, &[], &body).await;
        assert_eq!(sse.status, 200, "{path}");
        assert_eq!(
            sse.headers.get("x-should-retry").map(|v| v.to_str().unwrap()),
            Some("true"),
            "{path}"
        );
        assert_eq!(
            sse.headers.get("retry-after-ms").map(|v| v.to_str().unwrap()),
            Some("1500"),
            "{path}"
        );
        sse.all().await;
    }
}

async fn first_request(s: &fakellm::Server) -> Value {
    for _ in 0..100 {
        let log = get(s, "/_fakellm/requests").await.json();
        if let Some(r) = log["requests"].as_array().and_then(|a| a.first()) {
            return r.clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("request never logged");
}

#[tokio::test]
async fn hung_request_is_visible_while_parked_and_updated_on_release() {
    let s = start().await;
    s.clock().freeze(None).unwrap();
    put_rules(&s, json!([{"reply": {"text": "late", "hang_ms": 1000}}])).await;
    let body = chat_body("gpt-4o", "hi");
    let observe = async {
        let rec = first_request(&s).await;
        assert_eq!((rec["status"].clone(), rec["hung"].clone()), (json!(0), json!(true)), "{rec}");
        post_json(&s, "/_fakellm/clock/advance", &json!({"by": "1s"})).await;
    };
    let (r, ()) = tokio::join!(chat(&s, &body), observe);
    assert_eq!(r.status, 200);
    let rec = first_request(&s).await;
    assert_eq!((rec["status"].clone(), rec["hung"].clone()), (json!(200), json!(false)), "{rec}");
    assert_eq!(get(&s, "/_fakellm/requests").await.json()["requests"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn hung_request_is_updated_on_disconnect() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"hang": true}}])).await;
    let timed_out = tokio::time::timeout(Duration::from_millis(300), chat(&s, &chat_body("gpt-4o", "hi"))).await;
    assert!(timed_out.is_err());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let rec = first_request(&s).await;
    assert_eq!((rec["status"].clone(), rec["hung"].clone()), (json!(0), json!(false)), "{rec}");
    assert!(rec["latency_us"].as_u64().unwrap() >= 250_000, "{rec}");
}

#[tokio::test]
async fn reset_releases_hung_requests() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"match": {"last_user": "forever"}, "reply": {"hang": true}},
            {"reply": {"hang_ms": 3600000, "text": "released"}}
        ]),
    )
    .await;
    let reset = async {
        let _ = first_request(&s).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        post_json(&s, "/_fakellm/reset", &json!({})).await;
    };
    let body = chat_body("gpt-4o", "later");
    let (r, ()) = tokio::join!(chat(&s, &body), reset);
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["choices"][0]["message"]["content"], "released");

    put_rules(&s, json!([{"reply": {"hang": true}}])).await;
    let reset = async {
        let _ = first_request(&s).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        post_json(&s, "/_fakellm/reset", &json!({})).await;
    };
    let body = chat_body("gpt-4o", "forever");
    let (r, ()) = tokio::join!(chat(&s, &body), reset);
    assert_eq!(r.status, 503);
}
