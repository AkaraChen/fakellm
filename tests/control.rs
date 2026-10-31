//! End-to-end tests of the control plane and the Rust API, including the virtual-clock pacing.

use std::time::Duration;

use serde_json::{Value, json};

mod common;
use common::*;

const REAL: Duration = Duration::from_millis(100);

fn stream_body(user: &str) -> Value {
    let mut b = chat_body("gpt-4o", user);
    b["stream"] = json!(true);
    b
}

#[tokio::test]
async fn frozen_clock_paces_stream_without_sleeping() {
    let s = start().await;
    s.clock().freeze(None).unwrap();
    put_rules(
        &s,
        json!([{"reply": {"text": "aa bb cc", "stream": {"ttft_ms": 300, "tokens_per_sec": 10, "chunk_tokens": 1}}}]),
    )
    .await;
    let mut sse = open_stream(&s, &stream_body("x")).await;
    assert_eq!(sse.status, 200);
    assert!(sse.next_within(REAL).await.is_none(), "frame arrived before the clock moved");

    let adv = |by: &'static str| {
        let s = &s;
        async move { assert_eq!(post_json(s, "/_fakellm/clock/advance", &json!({"by": by})).await.status, 200) }
    };
    adv("300ms").await;
    let role = sse.next_within(Duration::from_secs(5)).await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&role).unwrap()["choices"][0]["delta"]["role"],
        "assistant"
    );
    let first = sse.next_within(Duration::from_secs(5)).await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&first).unwrap()["choices"][0]["delta"]["content"],
        "aa"
    );
    assert!(sse.next_within(REAL).await.is_none());

    let mut got = vec![first];
    for _ in 0..2 {
        adv("100ms").await;
        got.push(sse.next_within(Duration::from_secs(5)).await.unwrap().unwrap());
        if got.len() < 3 {
            assert!(sse.next_within(REAL).await.is_none());
        }
    }
    let text: String = got
        .iter()
        .map(|f| {
            serde_json::from_str::<Value>(f).unwrap()["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap_or("")
                .to_owned()
        })
        .collect();
    assert_eq!(text, "aa bb cc");
    let mut rest = Vec::new();
    while let Some(Some(f)) = sse.next_within(Duration::from_secs(5)).await {
        rest.push(f);
    }
    assert_eq!(rest.last().unwrap(), "[DONE]");
    assert!(rest.iter().any(|f| f.contains("\"finish_reason\":\"stop\"")));
}

#[tokio::test]
async fn reset_clears_scripts_log_and_clock() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "x"}}])).await;
    chat(&s, &chat_body("gpt-4o", "hi")).await;
    s.clock().freeze(None).unwrap();
    let st = get(&s, "/_fakellm/").await.json();
    assert_eq!(st["scripts"]["count"], 1);
    assert_eq!(st["clock"]["frozen"], true);

    let r = post_json(&s, "/_fakellm/reset", &json!({})).await;
    assert_eq!((r.status, r.json()["reset"].clone()), (200, json!(true)));
    assert_eq!(get(&s, "/_fakellm/scripts").await.json()["rules"], json!([]));
    assert_eq!(get(&s, "/_fakellm/requests").await.json()["total"], 0);
    assert_eq!(get(&s, "/_fakellm/clock").await.json()["frozen"], false);
}

#[tokio::test]
async fn request_log_shows_parsed_ir_and_filters() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"name": "boom", "match": {"last_user": "boom"}, "reply": {"error": {"status": 500}}},
            {"name": "ok", "match": {"last_user": "ok"}, "reply": {"text": "fine"}}
        ]),
    )
    .await;
    let mut with_tool = chat_body("gpt-4o-mini", "ok please");
    with_tool["tools"] = json!([{"type": "function", "function": {"name": "t1", "parameters": {"type": "object"}}}]);
    chat(&s, &with_tool).await;
    chat(&s, &chat_body("gpt-4o", "boom")).await;
    chat(&s, &chat_body("gpt-4o", "something else")).await;

    let all = get(&s, "/_fakellm/requests").await.json();
    assert_eq!(all["total"], 3);
    let first = &all["requests"][0];
    assert_eq!(first["request"]["messages"][0]["content"][0]["text"], "ok please");
    assert_eq!(first["model"], "gpt-4o-mini");
    assert_eq!(first["source"]["kind"], "script");
    assert_eq!(first["source"]["name"], "ok");
    assert!(first.get("headers").is_none() && first.get("raw").is_none());

    let total = |q: &str| {
        let s = &s;
        let path = format!("/_fakellm/requests?{q}");
        async move { get(s, &path).await.json()["total"].as_u64().unwrap() }
    };
    assert_eq!(total("model=gpt-4o-mini").await, 1);
    assert_eq!(total("has_tool=t1").await, 1);
    assert_eq!(total("status=500").await, 1);
    assert_eq!(total("status=2xx").await, 2);
    assert_eq!(total("source=fallback").await, 1);
    assert_eq!(total("source=script").await, 2);
    let limited = get(&s, "/_fakellm/requests?limit=1").await.json();
    assert_eq!(limited["requests"].as_array().unwrap().len(), 1);
    assert_eq!(limited["total"], 3);
    assert_eq!(get(&s, "/_fakellm/requests?bogus=1").await.status, 400);

    let full = get(&s, "/_fakellm/requests?full=true&model=gpt-4o-mini").await.json();
    let rec = &full["requests"][0];
    assert!(rec["headers"].is_array());
    assert_eq!(rec["raw"]["model"], "gpt-4o-mini");

    assert_eq!(send(&s, "DELETE", "/_fakellm/requests", &[], "").await.status, 200);
    assert_eq!(total("").await, 0);
}

#[tokio::test]
async fn request_log_config() {
    let s = start().await;
    let c = get(&s, "/_fakellm/requests/config").await.json();
    assert_eq!(c["enabled"], true);
    assert_eq!(c["capacity"], fakellm::DEFAULT_REQUEST_LOG_CAPACITY);
    let r = send(&s, "PUT", "/_fakellm/requests/config", &[], r#"{"capacity": 2}"#).await;
    assert_eq!(r.json()["capacity"], 2);
    for i in 0..4 {
        chat(&s, &chat_body("gpt-4o", &format!("m{i}"))).await;
    }
    let l = get(&s, "/_fakellm/requests").await.json();
    assert_eq!((l["total"].clone(), l["dropped"].clone()), (json!(2), json!(2)));
    send(&s, "PUT", "/_fakellm/requests/config", &[], r#"{"enabled": false}"#).await;
    chat(&s, &chat_body("gpt-4o", "unlogged")).await;
    assert_eq!(get(&s, "/_fakellm/requests").await.json()["total"], 2);
}

#[tokio::test]
async fn scripts_crud_and_formats() {
    let s = start().await;
    let r = send(
        &s,
        "PUT",
        "/_fakellm/scripts",
        &[("content-type", "application/yaml")],
        "- name: y\n  match:\n    last_user: yaml\n  reply:\n    text: from yaml\n",
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        chat(&s, &chat_body("m", "yaml")).await.json()["choices"][0]["message"]["content"],
        "from yaml"
    );

    post_json(
        &s,
        "/_fakellm/scripts",
        &json!({"rules": [{"name": "j", "reply": {"text": "from json"}}]}),
    )
    .await;
    let list = get(&s, "/_fakellm/scripts").await.json();
    assert_eq!(list["rules"].as_array().unwrap().len(), 2);
    assert_eq!(list["rules"][1]["name"], "j");
    assert_eq!(list["rules"][0]["hits"], 1);

    let bad = post_json(
        &s,
        "/_fakellm/scripts",
        &json!([{"match": {"last_user": "("}, "reply": {"text": "x"}}]),
    )
    .await;
    assert_eq!(bad.status, 400);
    assert!(bad.json()["error"].is_object() || bad.json()["Error"].is_object() || !bad.body.is_empty());
    assert_eq!(s.scripts().len(), 2, "a rejected document must not change the rules");

    assert_eq!(send(&s, "DELETE", "/_fakellm/scripts", &[], "").await.status, 200);
    assert_eq!(s.scripts().len(), 0);
}

#[tokio::test]
async fn origin_header_is_refused() {
    let s = start().await;
    let r = send(&s, "POST", "/_fakellm/reset", &[("origin", "http://evil.example")], "").await;
    assert_eq!(r.status, 403);
}

#[tokio::test]
async fn rust_api_surface() {
    let s = start().await;
    let rules = fakellm::Rule::parse_many(r#"[{"name": "r", "reply": {"text": "hi"}}]"#, fakellm::ScriptFormat::Json).unwrap();
    s.scripts().replace(rules).unwrap();
    let r = chat(&s, &chat_body("gpt-4o", "yo")).await.json();
    assert_eq!(r["choices"][0]["message"]["content"], "hi");

    let (rows, total) = s.requests(&fakellm::RequestFilter::default());
    assert_eq!((rows.len(), total), (1, 1));
    assert_eq!(rows[0].model(), Some("gpt-4o"));
    assert_eq!(rows[0].status, 200);
    assert_eq!(s.scripts().list()[0].hits, 1);

    s.clock().freeze(None).unwrap();
    assert!(s.clock().is_frozen());
    s.reset();
    assert!(!s.clock().is_frozen());
    assert_eq!(s.scripts().len(), 0);
    assert_eq!(s.requests(&fakellm::RequestFilter::default()).1, 0);
    assert!(s.base_url().ends_with("/v1"));
}

#[tokio::test]
async fn responses_routes_and_status() {
    let s = start().await;
    assert_eq!(get(&s, "/_fakellm").await.json()["responses"]["count"], 0);
    let r = get(&s, "/_fakellm/responses").await.json();
    assert_eq!((r["responses"].clone(), r["count"].clone()), (json!([]), json!(0)));
    assert_eq!(send(&s, "DELETE", "/_fakellm/responses", &[], "").await.json()["deleted"], 0);
    assert_eq!(send(&s, "POST", "/_fakellm/responses", &[], "").await.status, 405);
    assert!(s.response_ids().is_empty());
    assert_eq!(post_json(&s, "/_fakellm/reset?responses=false", &json!({})).await.status, 200);
}

#[tokio::test]
async fn put_keep_hits_and_protocol_filter() {
    let s = start().await;
    put_rules(&s, json!([{"name": "a", "reply": {"text": "x"}}])).await;
    chat(&s, &chat_body("gpt-4o", "hi")).await;
    assert_eq!(s.scripts().list()[0].hits, 1);
    let body = r#"[{"name": "a", "reply": {"text": "y"}}]"#;
    assert_eq!(send(&s, "PUT", "/_fakellm/scripts?keep_hits=true", &[], body).await.status, 200);
    assert_eq!(s.scripts().list()[0].hits, 1);
    send(&s, "PUT", "/_fakellm/scripts", &[], body).await;
    assert_eq!(s.scripts().list()[0].hits, 0);

    let total = |q: &str| {
        let s = &s;
        let q = q.to_owned();
        async move { get(s, &format!("/_fakellm/requests?{q}")).await.json()["total"].clone() }
    };
    assert_eq!(total("protocol=openai.chat").await, 1);
    assert_eq!(total("protocol=anthropic.messages").await, 0);
    assert_eq!(get(&s, "/_fakellm/requests?protocol=grpc").await.status, 400);
}

#[tokio::test]
async fn redact_auth_hides_credentials_in_the_log() {
    let s = start_with(fakellm::Config {
        redact_auth: true,
        ..Default::default()
    })
    .await;
    send(
        &s,
        "POST",
        "/v1/chat/completions",
        &[
            ("authorization", "Bearer sk-secret"),
            ("x-api-key", "sk-other"),
            ("content-type", "application/json"),
        ],
        &chat_body("gpt-4o", "hi").to_string(),
    )
    .await;
    let log = get(&s, "/_fakellm/requests?full=true").await;
    assert!(!log.body.contains("sk-secret") && !log.body.contains("sk-other"), "{}", log.body);
    assert!(log.body.contains("<redacted>"));
}

#[tokio::test]
async fn error_replies_expose_headers_and_in_stream() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"reply": {"error": {"status": 529, "headers": {"x-should-retry": "true"}}}}]),
    )
    .await;
    chat(&s, &chat_body("gpt-4o", "hi")).await;
    let r = &get(&s, "/_fakellm/requests").await.json()["requests"][0]["reply"];
    assert_eq!(r["status"], 529);
    assert_eq!(r["in_stream"], false);
    assert_eq!(r["headers"], json!([["x-should-retry", "true"]]));
}
