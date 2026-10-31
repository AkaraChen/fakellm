//! End-to-end tests of the Chat Completions endpoint against a real in-process server.

use fakellm::Config;
use serde_json::{Value, json};

mod common;
use common::*;

fn content(chunks: &[Value]) -> String {
    chunks.iter().filter_map(|c| c["choices"][0]["delta"]["content"].as_str()).collect()
}

#[tokio::test]
async fn sync_fallback_reply_shape() {
    let s = start().await;
    let body = chat_body("gpt-4o", "hello there");
    let r = chat(&s, &body).await;
    assert_eq!(r.status, 200);
    assert!(r.header("content-type").unwrap().starts_with("application/json"));
    let j = r.json();
    assert!(j["id"].as_str().unwrap().starts_with("chatcmpl-"));
    assert_eq!(j["object"], "chat.completion");
    assert_eq!(j["model"], "gpt-4o");
    let text = j["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(!text.is_empty());
    assert_eq!(j["choices"][0]["finish_reason"], "stop");
    let u = &j["usage"];
    assert!(u["prompt_tokens"].as_u64().unwrap() > 0);
    assert!(u["completion_tokens"].as_u64().unwrap() > 0);
    assert_eq!(
        u["total_tokens"].as_u64().unwrap(),
        u["prompt_tokens"].as_u64().unwrap() + u["completion_tokens"].as_u64().unwrap()
    );

    let again = chat(&s, &body).await.json();
    assert_eq!(again["choices"][0]["message"]["content"], j["choices"][0]["message"]["content"]);
}

#[tokio::test]
async fn rule_matches_model_glob_and_last_user() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"name": "greet", "match": {"model": "gpt-*", "last_user": "/^hi\\b/i"}, "reply": {"text": "scripted hello"}}]),
    )
    .await;
    let r = chat(&s, &chat_body("gpt-4o-mini", "Hi there")).await.json();
    assert_eq!(r["choices"][0]["message"]["content"], "scripted hello");
    let other = chat(&s, &chat_body("claude-3", "Hi there")).await.json();
    assert_ne!(other["choices"][0]["message"]["content"], "scripted hello");
    let other = chat(&s, &chat_body("gpt-4o", "yo")).await.json();
    assert_ne!(other["choices"][0]["message"]["content"], "scripted hello");
}

#[tokio::test]
async fn error_rule_renders_openai_error() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"reply": {"error": {"status": 429, "message": "slow down", "retry_after": 2}}}]),
    )
    .await;
    let r = chat(&s, &chat_body("gpt-4o", "x")).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.header("retry-after"), Some("2"));
    let e = &r.json()["error"];
    assert_eq!(e["message"], "slow down");
    for k in ["message", "type", "code", "param"] {
        assert!(e.get(k).is_some(), "missing {k}: {e}");
    }
}

#[tokio::test]
async fn strict_mode_rejects_unmatched() {
    let s = start_with(Config {
        strict: true,
        ..Config::default()
    })
    .await;
    let r = chat(&s, &chat_body("gpt-4o", "x")).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], "fakellm_no_matching_rule");
}

#[tokio::test]
async fn streaming_text() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "The quick brown fox jumps."}}])).await;
    let mut body = chat_body("gpt-4o", "x");
    body["stream"] = json!(true);
    let mut sse = open_stream(&s, &body).await;
    assert_eq!(sse.status, 200);
    assert!(sse.headers["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let frames = sse.all().await;
    assert_eq!(frames.last().unwrap(), "[DONE]");
    let chunks = json_chunks(&frames);
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(content(&chunks), "The quick brown fox jumps.");
    let finishes: Vec<_> = chunks.iter().filter(|c| c["choices"][0]["finish_reason"] == "stop").collect();
    assert_eq!(finishes.len(), 1);
    assert!(chunks.iter().all(|c| c.get("usage").is_none()));
}

#[tokio::test]
async fn streaming_include_usage() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "one two three"}}])).await;
    let mut body = chat_body("gpt-4o", "x");
    body["stream"] = json!(true);
    body["stream_options"] = json!({"include_usage": true});
    let frames = open_stream(&s, &body).await.all().await;
    assert_eq!(frames.last().unwrap(), "[DONE]");
    let chunks = json_chunks(&frames);
    let last = chunks.last().unwrap();
    assert_eq!(last["choices"], json!([]));
    assert!(last["usage"]["total_tokens"].as_u64().unwrap() > 0);
    assert!(chunks[..chunks.len() - 1].iter().all(|c| c["usage"].is_null()));
    assert_eq!(content(&chunks), "one two three");
}

#[tokio::test]
async fn streaming_tool_calls() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"reply": {"tool_calls": [{"name": "get_weather", "arguments": {"city": "Paris", "unit": "celsius"}}]}}]),
    )
    .await;
    let mut body = chat_body("gpt-4o", "weather?");
    body["stream"] = json!(true);
    let frames = open_stream(&s, &body).await.all().await;
    let chunks = json_chunks(&frames);
    let frags: Vec<&Value> = chunks
        .iter()
        .flat_map(|c| c["choices"][0]["delta"]["tool_calls"].as_array().into_iter().flatten())
        .collect();
    assert!(frags.len() >= 2);
    assert!(frags[0]["id"].as_str().unwrap().starts_with("call_"));
    assert_eq!(frags[0]["type"], "function");
    assert_eq!(frags[0]["function"]["name"], "get_weather");
    assert!(frags[1..].iter().all(|f| f["function"]["name"].is_null() && f["id"].is_null()));
    let args: String = frags.iter().filter_map(|f| f["function"]["arguments"].as_str()).collect();
    assert_eq!(args, r#"{"city":"Paris","unit":"celsius"}"#);
    let finishes: Vec<_> = chunks.iter().filter_map(|c| c["choices"][0]["finish_reason"].as_str()).collect();
    assert_eq!(finishes, ["tool_calls"]);
}

#[tokio::test]
async fn stateful_two_step_script() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"name": "call", "match": {"has_tool_result": false}, "reply": {"tool_calls": [{"name": "lookup", "arguments": {"q": "x"}, "id": "call_1"}]}},
            {"name": "answer", "match": {"last_tool_result": "lookup"}, "reply": {"text": "the answer is 42"}}
        ]),
    )
    .await;
    let tools = json!([{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}]);
    let mut body = chat_body("gpt-4o", "find it");
    body["tools"] = tools;
    let first = chat(&s, &body).await.json();
    let call = &first["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(first["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(call["function"]["name"], "lookup");
    assert_eq!(call["id"], "call_1");

    body["messages"] = json!([
        {"role": "user", "content": "find it"},
        {"role": "assistant", "content": null, "tool_calls": [call]},
        {"role": "tool", "tool_call_id": "call_1", "content": "42"}
    ]);
    let second = chat(&s, &body).await.json();
    assert_eq!(second["choices"][0]["message"]["content"], "the answer is 42");
    let rules = get(&s, "/_fakellm/scripts").await.json();
    assert_eq!(rules["rules"][0]["hits"], 1);
    assert_eq!(rules["rules"][1]["hits"], 1);
}

#[tokio::test]
async fn times_consumes_once() {
    let s = start().await;
    put_rules(&s, json!([{"times": 1, "reply": {"text": "once"}}])).await;
    let body = chat_body("gpt-4o", "x");
    assert_eq!(chat(&s, &body).await.json()["choices"][0]["message"]["content"], "once");
    assert_ne!(chat(&s, &body).await.json()["choices"][0]["message"]["content"], "once");
    assert_eq!(get(&s, "/_fakellm/scripts").await.json()["rules"][0]["exhausted"], true);
}

#[tokio::test]
async fn max_tokens_truncates() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "alpha beta gamma delta epsilon zeta eta theta"}}])).await;
    let mut body = chat_body("gpt-4o", "x");
    body["max_tokens"] = json!(3);
    let j = chat(&s, &body).await.json();
    assert_eq!(j["choices"][0]["finish_reason"], "length");
    assert_eq!(j["choices"][0]["message"]["content"], "alpha beta gamma");
    assert_eq!(j["usage"]["completion_tokens"], 3);
}

#[tokio::test]
async fn bad_requests() {
    let s = start().await;
    let r = send(&s, "POST", "/v1/chat/completions", &[("content-type", "application/json")], "{nope").await;
    assert_eq!(r.status, 400);
    assert!(r.json()["error"]["message"].is_string());
    let r = chat(&s, &json!({"messages": [{"role": "user", "content": "x"}]})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["message"], "you must provide a model parameter");
}

#[tokio::test]
async fn models_endpoints() {
    let s = start().await;
    let j = get(&s, "/v1/models").await.json();
    assert_eq!(j["object"], "list");
    let ids: Vec<&str> = j["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids, fakellm::DEFAULT_MODELS);
    let one = get(&s, "/v1/models/gpt-4o").await;
    assert_eq!(one.status, 200);
    assert_eq!(one.json()["id"], "gpt-4o");
    let r = get(&s, "/v1/models/nope").await;
    assert_eq!(r.status, 404);
    assert!(r.json()["error"].is_object());
}

#[tokio::test]
async fn api_key_is_enforced() {
    let s = start_with(Config {
        api_key: Some("sk-right".into()),
        ..Config::default()
    })
    .await;
    let body = chat_body("gpt-4o", "x").to_string();
    let send_with = |h: Vec<(&'static str, &'static str)>| {
        let (s, body) = (&s, body.clone());
        async move { send(s, "POST", "/v1/chat/completions", &h, &body).await }
    };
    let r = send_with(vec![("authorization", "Bearer sk-wrong")]).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.json()["error"]["code"], "invalid_api_key");
    assert_eq!(send_with(vec![]).await.status, 401);
    assert_eq!(send_with(vec![("authorization", "Bearer sk-right")]).await.status, 200);
    assert_eq!(send_with(vec![("x-api-key", "sk-right")]).await.status, 200);
}
