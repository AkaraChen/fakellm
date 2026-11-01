//! Custom endpoint rules over raw HTTP: `match.path` + `reply.body` answer paths no standard route
//! serves (xsai's `POST /v1/systemone`), matched on the raw body; everything else stays a 404.
//! `reply.raw` replays a recorded response verbatim (status, content type, SSE stream), on custom
//! paths and on the standard routes, with `match.nth`, `match.method` and `match.header`.

use serde_json::json;

mod common;
use common::*;

async fn rules(s: &fakellm::Server) {
    put_rules(
        s,
        json!([
            {"name": "angry", "match": {"path": "/v1/systemone", "body": "angry"},
             "reply": {"body": {"answers": {"damaged": {"type": "noul", "noul": 0.9}}, "usage": {"input_tokens": 5, "output_tokens": 1}}}},
            {"name": "happy", "match": {"path": "/v1/system*", "body": "/HAPPY/i"},
             "reply": {"status": 202, "headers": {"x-custom": "yes"}, "body": {"answers": {"damaged": {"type": "noul", "noul": 0.1}}}}},
        ]),
    )
    .await;
}

#[tokio::test]
async fn body_regexes_pick_the_rule() {
    let s = start().await;
    rules(&s).await;
    let a = post_json(&s, "/v1/systemone", &json!({"state": "an angry customer"})).await;
    assert_eq!(a.status, 200, "{}", a.body);
    assert_eq!(a.header("content-type"), Some("application/json"));
    assert_eq!(a.json()["answers"]["damaged"]["noul"], 0.9);
    assert_eq!(a.json()["usage"]["input_tokens"], 5);
    let h = post_json(&s, "/v1/systemone", &json!({"state": "so Happy"})).await;
    assert_eq!(h.json()["answers"]["damaged"]["noul"], 0.1);
}

#[tokio::test]
async fn status_and_headers() {
    let s = start().await;
    rules(&s).await;
    let h = post_json(&s, "/v1/systemone", &json!({"state": "happy"})).await;
    assert_eq!(h.status, 202);
    assert_eq!(h.header("x-custom"), Some("yes"));
}

#[tokio::test]
async fn unmatched_stays_404() {
    let s = start().await;
    rules(&s).await;
    let r = post_json(&s, "/v1/systemone", &json!({"state": "neutral"})).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["message"], "Invalid URL (POST /v1/systemone)");
    let r = post_json(&s, "/v1/other", &json!({"state": "angry"})).await;
    assert_eq!(r.status, 404);
    // Custom rules never touch the standard routes.
    let c = chat(&s, &chat_body("m", "angry")).await;
    assert_eq!(c.status, 200);
    assert!(!c.body.contains("noul"));
}

#[tokio::test]
async fn logged_as_custom() {
    let s = start().await;
    rules(&s).await;
    post_json(&s, "/v1/systemone", &json!({"state": "angry"})).await;
    let log = get(&s, "/_fakellm/requests?full=true&protocol=custom").await.json();
    let e = &log["requests"][0];
    assert_eq!(e["protocol"], "custom");
    assert_eq!(e["path"], "/v1/systemone");
    assert_eq!(e["status"], 200);
    assert_eq!(e["raw"]["state"], "angry");
    assert_eq!(e["rule"], "angry");
    assert_eq!(e["source"]["kind"], "script");
    // Hit counts show up on the rule.
    let rules = get(&s, "/_fakellm/scripts").await.json();
    assert_eq!(rules["rules"][0]["hits"], 1);
}

#[tokio::test]
async fn invalid_rules_are_rejected() {
    let s = start().await;
    for bad in [
        json!({"match": {"path": "/x"}, "reply": {"text": "hi"}}),
        json!({"match": {}, "reply": {"text": "hi", "status": 201}}),
        json!({"match": {"path": "/x"}, "reply": {"body": {}, "status": 99}}),
    ] {
        let r = send(
            &s,
            "PUT",
            "/_fakellm/scripts",
            &[("content-type", "application/json")],
            &json!([bad]).to_string(),
        )
        .await;
        assert_eq!(r.status, 400, "{}", r.body);
    }
}

// ---- raw replies: recorded responses replayed verbatim ----

/// The recorded Chat Completions stream of docs/research.md, one `data:` line per event.
fn fixture_events() -> Vec<String> {
    let doc = include_str!("../docs/research.md");
    let events: Vec<String> = doc
        .lines()
        .skip_while(|l| !l.starts_with("## 真实 OpenAI 流式样本"))
        .skip(1)
        .skip_while(|l| !l.starts_with("data: "))
        .take_while(|l| l.starts_with("data: "))
        .map(str::to_owned)
        .collect();
    assert_eq!(events.len(), 5, "the fixture in docs/research.md changed");
    events
}

fn fixture_sse() -> String {
    fixture_events().iter().map(|e| format!("{e}\n\n")).collect()
}

fn stream_chat(user: &str) -> serde_json::Value {
    let mut b = chat_body("gpt-4o", user);
    b["stream"] = json!(true);
    b
}

#[tokio::test]
async fn raw_sse_replays_a_recorded_chat_stream_verbatim() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"name": "fixture", "match": {"path": "/v1/chat/completions"},
                "reply": {"raw": {"content_type": "text/event-stream", "body": fixture_sse()}}}]),
    )
    .await;
    let mut sse = open_stream(&s, &stream_chat("anything at all")).await;
    assert_eq!(sse.status, 200);
    assert_eq!(sse.headers["content-type"], "text/event-stream");
    assert!(sse.headers.get("content-length").is_none(), "{:?}", sse.headers);
    let frames = sse.all().await;
    let want: Vec<String> = fixture_events()
        .iter()
        .map(|e| e.strip_prefix("data: ").unwrap().to_owned())
        .collect();
    assert_eq!(frames, want);
    // Not regenerated: the recorded id and obfuscation field survive.
    assert_eq!(json_chunks(&frames)[0]["id"], "chatcmpl-D8Z5oo6uDh67AD85p73ksdT1KxhE0");
    assert_eq!(json_chunks(&frames)[1]["obfuscation"], "yhjoJbEF");

    // The request is parsed into the IR and logged with the rule as its source.
    let e = &log(&s, "protocol=openai.chat").await[0];
    assert_eq!(e["rule"], "fixture");
    assert_eq!(e["source"]["kind"], "script");
    assert_eq!(e["model"], "gpt-4o");
    assert_eq!(e["stream"], true);
    assert_eq!(e["raw"]["messages"][0]["content"], "anything at all");
    assert_eq!(e["status"], 200);
}

#[tokio::test]
async fn raw_sse_is_one_frame_per_event_paced_by_the_clock() {
    let s = start().await;
    s.clock().freeze(None).unwrap();
    put_rules(
        &s,
        json!([{"match": {"path": "/v1/chat/completions"},
                "reply": {"stream": {"ttft_ms": 200, "tokens_per_sec": 10},
                          "raw": {"content_type": "text/event-stream", "body": fixture_sse()}}}]),
    )
    .await;
    let real = std::time::Duration::from_millis(100);
    let long = std::time::Duration::from_secs(5);
    let mut sse = open_stream(&s, &stream_chat("x")).await;
    assert!(sse.next_within(real).await.is_none(), "a frame arrived before the clock moved");
    post_json(&s, "/_fakellm/clock/advance", &json!({"by": "200ms"})).await;
    assert!(sse.next_within(long).await.unwrap().is_some());
    assert!(sse.next_within(real).await.is_none(), "the second event came without its interval");
    post_json(&s, "/_fakellm/clock/advance", &json!({"by": "100ms"})).await;
    assert!(sse.next_within(long).await.unwrap().is_some());
    post_json(&s, "/_fakellm/clock/advance", &json!({"by": "1s"})).await;
    assert_eq!(sse.all().await.len(), 3);
}

#[tokio::test]
async fn raw_stream_can_be_cut_like_any_stream() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"match": {"path": "/v1/chat/completions"},
                "reply": {"stream": {"cut_after_chunks": 2},
                          "raw": {"content_type": "text/event-stream", "body": fixture_sse()}}}]),
    )
    .await;
    assert_eq!(open_stream(&s, &stream_chat("x")).await.all().await.len(), 2);
}

#[tokio::test]
async fn raw_json_with_status_and_headers() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"match": {"path": "/v1/chat/completions"},
                "reply": {"raw": {"status": 418, "body": "{\"error\": {\"message\":  \"I am a teapot\"}}",
                                  "headers": {"x-fixture": "teapot", "retry-after": "7"}}}}]),
    )
    .await;
    let r = chat(&s, &chat_body("gpt-4o", "tea?")).await;
    assert_eq!(r.status, 418);
    assert_eq!(r.header("x-fixture"), Some("teapot"));
    assert_eq!(r.header("retry-after"), Some("7"));
    assert_eq!(r.header("content-type"), Some("application/json"));
    // Verbatim, including the odd spacing; buffered with a length.
    assert_eq!(r.body, "{\"error\": {\"message\":  \"I am a teapot\"}}");
    assert_eq!(r.header("content-length"), Some(r.body.len().to_string().as_str()));
    assert_eq!(log(&s, "protocol=openai.chat").await[0]["status"], 418);
}

#[tokio::test]
async fn raw_content_type_and_stream_flag_are_independent() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"match": {"path": "/text"}, "reply": {"raw": {"content_type": "text/plain; charset=utf-8", "body": "plain\n\ntext"}}},
            {"match": {"path": "/chunked"}, "reply": {"raw": {"stream": true, "body": "one chunk"}}},
            {"match": {"path": "/buffered"}, "reply": {"raw": {"content_type": "text/event-stream", "stream": false, "body": "data: 1\n\ndata: 2\n\n"}}},
        ]),
    )
    .await;
    let r = post_json(&s, "/text", &json!({})).await;
    assert_eq!(
        (r.header("content-type"), r.body.as_str()),
        (Some("text/plain; charset=utf-8"), "plain\n\ntext")
    );
    assert!(r.header("content-length").is_some());
    let r = post_json(&s, "/chunked", &json!({})).await;
    assert_eq!(r.body, "one chunk");
    assert!(r.header("content-length").is_none());
    let r = post_json(&s, "/buffered", &json!({})).await;
    assert_eq!(r.header("content-length"), Some("18"));
    assert_eq!(r.header("content-type"), Some("text/event-stream"));
}

#[tokio::test]
async fn raw_replays_responses_events() {
    let s = start().await;
    let events = "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
    put_rules(
        &s,
        json!([{"match": {"path": "/v1/responses", "last_user": "replay"},
                "reply": {"raw": {"content_type": "text/event-stream", "body": events}}}]),
    )
    .await;
    let mut sse = open_sse(
        &s,
        "/v1/responses",
        &[],
        &json!({"model": "m", "input": "replay me", "stream": true}),
    )
    .await;
    let got = sse.events().await;
    assert_eq!(
        got.iter().map(|e| e.event.as_deref().unwrap()).collect::<Vec<_>>(),
        ["response.created", "response.completed"]
    );
    // A request the rule does not match is handled by the Responses pipeline.
    let r = responses(&s, &responses_body("m", "something else")).await;
    assert_eq!(r.json()["object"], "response");
    let entries = log(&s, "protocol=openai.responses").await;
    assert_eq!(entries[0]["source"]["kind"], "script");
    assert_eq!(entries[0]["model"], "m");
    assert_eq!(entries[1]["source"]["kind"], "fallback");
}

#[tokio::test]
async fn nth_matches_only_the_nth_request_that_reached_the_rule() {
    let s = start().await;
    // Each rule counts the requests that reached it, and a rule only sees what the rules before it did not
    // answer: the later position of a sequence comes first.
    put_rules(
        &s,
        json!([
            {"name": "second", "match": {"path": "/v1/chat/completions", "last_user": "go", "nth": 2}, "reply": {"raw": {"body": "{\"n\":2}"}}},
            {"name": "first", "match": {"path": "/v1/chat/completions", "last_user": "go", "nth": 1}, "reply": {"raw": {"status": 429, "body": "{\"n\":1}"}}},
            {"reply": {"text": "generated"}},
        ]),
    )
    .await;
    // Requests that fail another condition do not advance the sequence.
    for _ in 0..3 {
        assert!(chat(&s, &chat_body("m", "unrelated")).await.body.contains("generated"));
    }
    let a = chat(&s, &chat_body("m", "go")).await;
    assert_eq!((a.status, a.body.as_str()), (429, "{\"n\":1}"));
    let b = chat(&s, &chat_body("m", "go")).await;
    assert_eq!((b.status, b.body.as_str()), (200, "{\"n\":2}"));
    // The sequence is over: the third "go" falls through to the generated reply, and keeps doing so.
    for _ in 0..2 {
        assert!(chat(&s, &chat_body("m", "go")).await.body.contains("generated"));
    }
    let rules = get(&s, "/_fakellm/scripts").await.json();
    assert_eq!(rules["rules"][0]["hits"], 1);
    assert_eq!(rules["rules"][0]["reached"], 4);
    assert_eq!(rules["rules"][1]["hits"], 1);
    // `first` never saw the request `second` answered.
    assert_eq!(rules["rules"][1]["reached"], 3);
}

#[tokio::test]
async fn nth_works_on_generated_rules() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"match": {"nth": 3}, "reply": {"text": "third"}}, {"reply": {"text": "other"}}]),
    )
    .await;
    let mut got = Vec::new();
    for _ in 0..5 {
        got.push(
            chat(&s, &chat_body("m", "x")).await.json()["choices"][0]["message"]["content"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    assert_eq!(got, ["other", "other", "third", "other", "other"]);
}

#[tokio::test]
async fn method_and_header_matching() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"name": "ping", "match": {"path": "/v1/ping", "method": "get", "header": {"X-Env": "^stag(e|ing)$"}},
             "reply": {"raw": {"body": "{\"pong\":true}"}}},
            {"name": "any-method", "match": {"path": "/v1/ping", "method": "P*", "header": {"x-env": "prod", "x-team": "/^A/i"}},
             "reply": {"raw": {"status": 202, "body": "{}"}}},
        ]),
    )
    .await;
    let r = send(&s, "GET", "/v1/ping", &[("x-env", "staging")], "").await;
    assert_eq!((r.status, r.body.as_str()), (200, "{\"pong\":true}"));
    // Wrong method, wrong or missing header.
    assert_eq!(send(&s, "POST", "/v1/ping", &[("x-env", "staging")], "").await.status, 404);
    assert_eq!(send(&s, "GET", "/v1/ping", &[("x-env", "production")], "").await.status, 404);
    assert_eq!(send(&s, "GET", "/v1/ping", &[], "").await.status, 404);
    // Every header named has to match.
    assert_eq!(send(&s, "POST", "/v1/ping", &[("x-env", "prod")], "").await.status, 404);
    assert_eq!(
        send(&s, "POST", "/v1/ping", &[("x-env", "prod"), ("x-team", "alpha")], "")
            .await
            .status,
        202
    );
    assert_eq!(
        send(&s, "PUT", "/v1/ping", &[("x-env", "prod"), ("X-Team", "Apps")], "")
            .await
            .status,
        202
    );
    assert_eq!(
        send(&s, "DELETE", "/v1/ping", &[("x-env", "prod"), ("x-team", "Apps")], "")
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn method_and_header_also_gate_generated_replies() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"match": {"header": {"x-team": "^blue$"}}, "reply": {"text": "blue team"}},
            {"match": {"method": "POST"}, "reply": {"text": "posted"}},
        ]),
    )
    .await;
    let blue = post_json_with(&s, "/v1/chat/completions", &[("x-team", "blue")], &chat_body("m", "x")).await;
    assert!(blue.body.contains("blue team"), "{}", blue.body);
    let other = post_json_with(&s, "/v1/chat/completions", &[("x-team", "red")], &chat_body("m", "x")).await;
    assert!(other.body.contains("posted"), "{}", other.body);
    let emb = post_json(&s, "/v1/embeddings", &json!({"model": "e", "input": "x"})).await;
    assert_eq!(emb.status, 200);
}

#[tokio::test]
async fn raw_rules_validate() {
    let s = start().await;
    for bad in [
        json!({"match": {"path": "/x"}, "reply": {"raw": {"body": "x"}, "text": "hi"}}),
        json!({"match": {"path": "/x"}, "reply": {"raw": {"body": "x"}, "body": {}}}),
        json!({"match": {"path": "/x"}, "reply": {"raw": {"body": "x"}, "tool_calls": [{"name": "f"}]}}),
        json!({"match": {"path": "/x"}, "reply": {"raw": {"body": "x"}, "error": {"status": 500}}}),
        json!({"match": {"path": "/x"}, "reply": {"raw": {"body": "x"}, "trajectory": "t"}}),
        json!({"match": {"path": "/x"}, "reply": {"raw": {"body": "x"}, "policy": "p"}}),
        json!({"match": {"path": "/x"}, "reply": {"raw": {"body": "x"}, "status": 201}}),
        json!({"match": {"path": "/x"}, "reply": {"raw": {"body": "x", "status": 99}}}),
        json!({"match": {"path": "/x"}, "reply": {"raw": {"body": "x", "headers": {"bad name": "v"}}}}),
        json!({"match": {"path": "/x"}, "reply": {"raw": {"body": "x", "content_type": "a\nb"}}}),
        json!({"match": {"path": "/x"}, "reply": {"raw": {"body": "x", "bogus": 1}}}),
        json!({"match": {"path": "/x"}, "reply": {"raw": {}}}),
        json!({"match": {}, "reply": {"raw": {"body": "x"}}}),
        json!({"match": {"nth": 0}, "reply": {"text": "x"}}),
        json!({"match": {"header": {"x": "("}}, "reply": {"text": "x"}}),
        json!({"match": {"header": {"bad name": "x"}}, "reply": {"text": "x"}}),
        json!({"match": {"method": ""}, "reply": {"text": "x"}}),
    ] {
        let r = send(
            &s,
            "PUT",
            "/_fakellm/scripts",
            &[("content-type", "application/json")],
            &json!([bad]).to_string(),
        )
        .await;
        assert_eq!(r.status, 400, "{bad}: {}", r.body);
    }
}
