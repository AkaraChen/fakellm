//! Cassette replay over HTTP: formats, routes, matching rules, cross-protocol rendering, precedence and the log
//! (docs/m4-spec.md section 3 and 11).

use fakellm::Config;
use serde_json::{Value, json};

mod common;
use common::*;

fn user_msg(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

/// A hand-written exchange with extra request fields merged in.
fn ex_with(user: &str, text: &str, extra: Value) -> Value {
    let mut e = exchange(user, text);
    for (k, v) in extra.as_object().unwrap() {
        e["request"][k] = v.clone();
    }
    e
}

fn is_default_reply(text: &str) -> bool {
    !text.is_empty() && text != "recorded"
}

// ---- formats and routes ----

#[tokio::test]
async fn all_document_forms_load() {
    let doc = cassette("one", vec![exchange("a", "A")]);
    let s = start().await;
    for (label, body, ct) in [
        ("object", doc.to_string(), "application/json"),
        ("array", json!([doc]).to_string(), "application/json"),
        ("wrapped", json!({"cassettes": [doc]}).to_string(), "application/json"),
        (
            "jsonl",
            format!(
                "{}\n{}\n",
                json!({"kind": "cassette", "name": "one"}),
                exchange("a", "A")
            ),
            "application/x-ndjson",
        ),
        (
            "yaml",
            "kind: cassette\nname: one\nexchanges:\n  - request:\n      model: gpt-4o\n      messages:\n        - role: user\n          content:\n            - {type: text, text: a}\n    outcome: {outcome: reply, reply: {text: A, finish: stop, usage: {prompt_tokens: 1, completion_tokens: 1}}}\n"
                .to_owned(),
            "application/yaml",
        ),
    ] {
        let r = send(&s, "PUT", "/_fakellm/cassettes", &[("content-type", ct)], &body).await;
        assert_eq!(r.status, 200, "{label}: {}", r.body);
        let list = get(&s, "/_fakellm/cassettes").await.json();
        assert_eq!(list["cassettes"][0]["name"], "one", "{label}");
        assert_eq!(list["cassettes"][0]["exchanges"], 1, "{label}");
        assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "a")).await), "A", "{label}");
    }
}

#[tokio::test]
async fn multi_cassette_jsonl_and_export_round_trip() {
    let s = start().await;
    let body = format!(
        "{}\n{}\n{}\n{}\n",
        json!({"kind": "cassette", "name": "a", "once": true}),
        exchange("q1", "A1"),
        json!({"kind": "cassette", "name": "b", "fuzzy": true}),
        exchange("q2", "B1"),
    );
    let r = send(&s, "PUT", "/_fakellm/cassettes", &[("content-type", "application/x-ndjson")], &body).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["count"], 2);
    let list = get(&s, "/_fakellm/cassettes").await.json();
    assert_eq!(list["cassettes"][0]["once"], true);
    assert_eq!(list["cassettes"][1]["fuzzy"], true);

    // Export as JSON and as JSONL, load both into a fresh server: same behavior.
    let json_doc = get(&s, "/_fakellm/cassettes/a").await.json();
    assert_eq!(json_doc["kind"], "cassette");
    assert_eq!(json_doc["exchanges"].as_array().unwrap().len(), 1);
    let jsonl = get(&s, "/_fakellm/cassettes/b?format=jsonl").await;
    assert_eq!(
        jsonl.header("content-type").map(|c| c.starts_with("application/x-ndjson")),
        Some(true)
    );
    let other = start().await;
    put_cassettes(&other, &json_doc).await;
    let r = send(
        &other,
        "POST",
        "/_fakellm/cassettes",
        &[("content-type", "application/x-ndjson")],
        &jsonl.body,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(chat_text(&chat(&other, &chat_body("gpt-4o", "q1")).await), "A1");
    assert_eq!(chat_text(&chat(&other, &chat_body("gpt-4o", "q2")).await), "B1");
}

#[tokio::test]
async fn crud_errors_and_reset() {
    let s = start().await;
    let put = |body: Value| {
        let s = &s;
        async move { ctl_json(s, "PUT", "/_fakellm/cassettes", &body).await }
    };
    let bad_regex = json!({"name": "x", "normalize": [{"pattern": "(", "replace": ""}], "exchanges": []});
    let bad_ignore = json!({"name": "x", "ignore": ["nonsense"], "exchanges": []});
    let reserved = json!({"name": "recording", "exchanges": []});
    let slash = json!({"name": "a/b", "exchanges": []});
    let traj = json!({"kind": "trajectory", "name": "t", "steps": [{"reply": {"text": "x"}}]});
    for (label, body) in [
        ("regex", bad_regex),
        ("ignore", bad_ignore),
        ("reserved", reserved),
        ("slash", slash),
        ("trajectory", traj),
    ] {
        let r = put(body).await;
        assert_eq!(r.status, 400, "{label}: {}", r.body);
    }
    // A 400 changes nothing.
    put_cassettes(&s, &cassette("keep", vec![exchange("a", "A")])).await;
    assert_eq!(put(json!({"name": "x", "ignore": ["nonsense"]})).await.status, 400);
    assert_eq!(get(&s, "/_fakellm/cassettes").await.json()["cassettes"][0]["name"], "keep");
    // POST replaces in place, DELETE removes, 404s.
    let r = ctl_json(&s, "POST", "/_fakellm/cassettes", &cassette("keep", vec![exchange("a", "A2")])).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "a")).await), "A2");
    assert_eq!(
        send(&s, "DELETE", "/_fakellm/cassettes/keep", &[], "").await.json(),
        json!({"deleted": true})
    );
    assert_eq!(send(&s, "DELETE", "/_fakellm/cassettes/keep", &[], "").await.status, 404);
    assert_eq!(get(&s, "/_fakellm/cassettes/keep").await.status, 404);
    // reset clears.
    put_cassettes(&s, &cassette("again", vec![exchange("a", "A")])).await;
    assert_eq!(send(&s, "POST", "/_fakellm/reset", &[], "").await.status, 200);
    assert_eq!(get(&s, "/_fakellm/cassettes").await.json(), json!({"cassettes": []}));
}

#[tokio::test]
async fn config_cassettes_are_registered_at_start() {
    let c: fakellm::Cassette = serde_json::from_value(cassette("boot", vec![exchange("a", "from boot")])).unwrap();
    let s = start_with(Config {
        cassettes: vec![c],
        ..Config::default()
    })
    .await;
    assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "a")).await), "from boot");
    assert_eq!(get(&s, "/_fakellm").await.json()["cassettes"]["count"], 1);
}

// ---- cross-protocol replay ----

#[tokio::test]
async fn a_chat_recording_answers_chat_responses_and_anthropic() {
    let s = start().await;
    let mut e = exchange("hello", "recorded words here");
    e["outcome"]["reply"]["usage"] = json!({"prompt_tokens": 11, "completion_tokens": 4});
    put_cassettes(&s, &cassette("c", vec![e])).await;

    let r = chat(&s, &chat_body("gpt-4o", "hello")).await;
    let j = r.json();
    assert_eq!(j["choices"][0]["message"]["content"], "recorded words here");
    assert_eq!(j["choices"][0]["finish_reason"], "stop");
    assert_eq!(j["usage"]["prompt_tokens"], 11);
    assert_eq!(j["usage"]["completion_tokens"], 4);

    let j = responses(&s, &responses_body("gpt-5.5", "hello")).await.json();
    assert_eq!(j["output"][0]["content"][0]["text"], "recorded words here");
    assert_eq!(j["usage"]["input_tokens"], 11);

    let j = anthropic(&s, &anthropic_body("claude-sonnet-4-5", "hello")).await.json();
    assert_eq!(j["content"][0]["text"], "recorded words here");
    assert_eq!(j["stop_reason"], "end_turn");
    assert_eq!(j["usage"]["output_tokens"], 4);

    // Streaming renders in the request's protocol.
    let mut b = chat_body("gpt-4o", "hello");
    b["stream"] = json!(true);
    let frames = open_stream(&s, &b).await.all().await;
    let text: String = json_chunks(&frames)
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(text, "recorded words here");

    let mut b = anthropic_body("claude-sonnet-4-5", "hello");
    b["stream"] = json!(true);
    let events = open_sse(&s, "/v1/messages", ANTHROPIC_HEADERS, &b).await.events().await;
    let text: String = events
        .iter()
        .filter(|e| e.event.as_deref() == Some("content_block_delta"))
        .filter_map(|e| e.json()["delta"]["text"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(text, "recorded words here");

    let mut b = responses_body("gpt-5.5", "hello");
    b["stream"] = json!(true);
    let events = open_sse(&s, "/v1/responses", &[], &b).await.events().await;
    let text: String = events
        .iter()
        .map(SseEvent::json)
        .filter(|j| j["type"] == "response.output_text.delta")
        .map(|j| j["delta"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(text, "recorded words here");
}

#[tokio::test]
async fn a_tool_call_recording_replays_with_its_ids_in_anthropic() {
    let s = start().await;
    let mut e = exchange("weather?", "");
    e["outcome"]["reply"] = json!({
        "tool_calls": [{"id": "call_abc", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}],
        "finish": "tool_calls",
        "usage": {"prompt_tokens": 5, "completion_tokens": 2},
    });
    put_cassettes(&s, &cassette("tools", vec![e])).await;
    let j = anthropic(&s, &anthropic_body("claude-sonnet-4-5", "weather?")).await.json();
    assert_eq!(j["stop_reason"], "tool_use");
    assert_eq!(j["content"][0]["type"], "tool_use");
    assert_eq!(j["content"][0]["id"], "call_abc");
    assert_eq!(j["content"][0]["input"], json!({"city": "Paris"}));
}

// ---- matching ----

#[tokio::test]
async fn default_ignore_list_and_changed_messages() {
    let s = start().await;
    put_cassettes(&s, &cassette("c", vec![exchange("hello", "recorded")])).await;
    let mut b = chat_body("gpt-4o", "hello");
    b["temperature"] = json!(0.2);
    b["top_p"] = json!(0.5);
    b["max_tokens"] = json!(50);
    b["seed"] = json!(7);
    b["user"] = json!("someone");
    b["metadata"] = json!({"k": "v"});
    b["store"] = json!(false);
    b["foo_extra"] = json!(1);
    assert_eq!(chat_text(&chat(&s, &b).await), "recorded");
    let mut streaming = b.clone();
    streaming["stream"] = json!(true);
    streaming["stream_options"] = json!({"include_usage": true});
    let frames = open_stream(&s, &streaming).await.all().await;
    let text: String = json_chunks(&frames)
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(text, "recorded");
    // A different message does not match.
    assert!(is_default_reply(&chat_text(&chat(&s, &chat_body("gpt-4o", "hello there")).await)));
}

#[tokio::test]
async fn bang_max_tokens_makes_it_significant() {
    let s = start().await;
    let mut c = cassette("c", vec![ex_with("hello", "recorded", json!({"max_tokens": 100}))]);
    c["ignore"] = json!(["!max_tokens"]);
    put_cassettes(&s, &c).await;
    let mut b = chat_body("gpt-4o", "hello");
    b["max_tokens"] = json!(50);
    assert!(is_default_reply(&chat_text(&chat(&s, &b).await)));
    b["max_tokens"] = json!(100);
    assert_eq!(chat_text(&chat(&s, &b).await), "recorded");
}

#[tokio::test]
async fn ignore_system_and_per_exchange_ignore() {
    let s = start().await;
    let mut e = exchange("hello", "recorded");
    e["request"]["messages"] = json!([{"role": "system", "content": [{"type": "text", "text": "You are A"}]}, user_msg("hello")]);
    let mut c = cassette("c", vec![e.clone()]);
    put_cassettes(&s, &c).await;
    let body = json!({"model": "gpt-4o", "messages": [{"role": "system", "content": "You are B"}, {"role": "user", "content": "hello"}]});
    assert!(is_default_reply(&chat_text(&chat(&s, &body).await)));
    c["ignore"] = json!(["system"]);
    put_cassettes(&s, &c).await;
    assert_eq!(chat_text(&chat(&s, &body).await), "recorded");
    // Per-exchange ignore, no cassette-level list.
    e["ignore"] = json!(["system"]);
    put_cassettes(&s, &cassette("c", vec![e])).await;
    assert_eq!(chat_text(&chat(&s, &body).await), "recorded");
}

#[tokio::test]
async fn default_normalizers_cover_timestamps_uuids_and_req_ids() {
    let s = start().await;
    let mut e = exchange("x", "recorded");
    e["request"]["messages"] = json!([
        {"role": "system", "content": [{"type": "text", "text": "Now 2025-03-14T09:26:53Z, session 11111111-2222-3333-4444-555555555555"}]},
        user_msg("trace req_01ABCdef23456789 please"),
    ]);
    put_cassettes(&s, &cassette("c", vec![e])).await;
    let body = json!({"model": "gpt-4o", "messages": [
        {"role": "system", "content": "Now 2026-11-01T10:00:00.123+02:00, session aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"},
        {"role": "user", "content": "trace req_99ZZyy8877aaBBcc please"},
    ]});
    assert_eq!(chat_text(&chat(&s, &body).await), "recorded");
}

#[tokio::test]
async fn custom_normalize_and_defaults_off() {
    let s = start().await;
    let mut c = cassette("c", vec![exchange("Today is Monday", "recorded")]);
    c["normalize"] = json!([{"pattern": "Today is \\w+", "replace": "Today is <day>"}]);
    put_cassettes(&s, &c).await;
    assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "Today is Friday")).await), "recorded");

    let mut c = cassette("c", vec![exchange("at 2025-03-14T09:26:53Z", "recorded")]);
    c["normalize_defaults"] = json!(false);
    put_cassettes(&s, &c).await;
    assert!(is_default_reply(&chat_text(
        &chat(&s, &chat_body("gpt-4o", "at 2026-01-01T00:00:00Z")).await
    )));
    assert_eq!(
        chat_text(&chat(&s, &chat_body("gpt-4o", "at 2025-03-14T09:26:53Z")).await),
        "recorded"
    );
}

#[tokio::test]
async fn tool_names_versus_schemas_and_tool_call_ids() {
    let s = start().await;
    let tool = |desc: &str| json!({"type": "function", "function": {"name": "f", "parameters": {"type": "object", "properties": {"a": {"type": "string", "description": desc}}}}});
    let mut e = exchange("go", "recorded");
    e["request"]["tools"] =
        json!([{"name": "f", "parameters": {"type": "object", "properties": {"a": {"type": "string", "description": "one"}}}}]);
    let mut c = cassette("c", vec![e]);
    put_cassettes(&s, &c).await;
    let mut b = chat_body("gpt-4o", "go");
    b["tools"] = json!([tool("two")]);
    assert_eq!(chat_text(&chat(&s, &b).await), "recorded", "names only by default");
    c["match_tool_schemas"] = json!(true);
    put_cassettes(&s, &c).await;
    assert!(is_default_reply(&chat_text(&chat(&s, &b).await)), "schemas compared");
    b["tools"] = json!([tool("one")]);
    assert_eq!(chat_text(&chat(&s, &b).await), "recorded");

    // Tool call ids are not compared; the arguments' key order does not matter.
    let mut e = exchange("x", "recorded");
    e["request"]["messages"] = json!([
        user_msg("run it"),
        {"role": "assistant", "content": [], "tool_calls": [{"id": "call_OLD", "name": "f", "arguments": "{\"a\": 1, \"b\": 2}"}]},
        {"role": "tool", "content": [{"type": "text", "text": "done"}], "tool_call_id": "call_OLD"},
    ]);
    put_cassettes(&s, &cassette("ids", vec![e])).await;
    let body = json!({"model": "gpt-4o", "messages": [
        {"role": "user", "content": "run it"},
        {"role": "assistant", "content": null, "tool_calls": [{"id": "call_NEW", "type": "function", "function": {"name": "f", "arguments": "{\"b\":2,\"a\":1}"}}]},
        {"role": "tool", "tool_call_id": "call_NEW", "content": "done"},
    ]});
    assert_eq!(chat_text(&chat(&s, &body).await), "recorded");
}

#[tokio::test]
async fn model_glob_pins_the_cassette() {
    let s = start().await;
    let mut c = cassette("c", vec![exchange("hello", "recorded")]);
    c["model_glob"] = json!("gpt-4o*");
    put_cassettes(&s, &c).await;
    assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o-mini", "hello")).await), "recorded");
    assert!(is_default_reply(&chat_text(&chat(&s, &chat_body("gpt-3.5", "hello")).await)));
    // Unpinned: replays for any model, including another protocol's.
    put_cassettes(&s, &cassette("c", vec![exchange("hello", "recorded")])).await;
    assert_eq!(chat_text(&chat(&s, &chat_body("whatever", "hello")).await), "recorded");
}

#[tokio::test]
async fn fuzzy_matches_nearest_and_strict_does_not() {
    let two = |first: &str, last: &str, text: &str| {
        let mut e = exchange(last, text);
        e["request"]["messages"] =
            json!([user_msg(first), {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}, user_msg(last)]);
        e
    };
    let live = json!({"model": "gpt-4o", "messages": [
        {"role": "user", "content": "first, changed"},
        {"role": "assistant", "content": "ok"},
        {"role": "user", "content": "the question"},
    ]});
    let s = start().await;
    put_cassettes(&s, &cassette("exact", vec![two("first", "the question", "recorded")])).await;
    assert!(
        is_default_reply(&chat_text(&chat(&s, &live).await)),
        "no fuzzy: falls to the default reply"
    );
    let mut c = cassette("fz", vec![two("first", "the question", "recorded")]);
    c["fuzzy"] = json!(true);
    put_cassettes(&s, &c).await;
    assert_eq!(chat_text(&chat(&s, &live).await), "recorded");
    // A different last user text never fuzzy-matches.
    let other = json!({"model": "gpt-4o", "messages": [
        {"role": "user", "content": "first"}, {"role": "assistant", "content": "ok"}, {"role": "user", "content": "another"},
    ]});
    assert!(is_default_reply(&chat_text(&chat(&s, &other).await)));
}

#[tokio::test]
async fn once_replays_in_order_then_falls_through() {
    let s = start().await;
    let mut c = cassette("c", vec![exchange("hello", "first"), exchange("hello", "second")]);
    c["once"] = json!(true);
    put_cassettes(&s, &c).await;
    let b = chat_body("gpt-4o", "hello");
    assert_eq!(chat_text(&chat(&s, &b).await), "first");
    assert_eq!(chat_text(&chat(&s, &b).await), "second");
    let third = chat_text(&chat(&s, &b).await);
    assert!(third != "first" && third != "second", "{third}");
    let info = &get(&s, "/_fakellm/cassettes").await.json()["cassettes"][0];
    assert_eq!(info["consumed"], 2);
    assert_eq!(info["hits"], 2);
    // Without `once` the first of equal exchanges always wins.
    put_cassettes(&s, &cassette("c", vec![exchange("hello", "first"), exchange("hello", "second")])).await;
    for _ in 0..3 {
        assert_eq!(chat_text(&chat(&s, &b).await), "first");
    }
}

// ---- precedence, errors, log ----

#[tokio::test]
async fn scripts_beat_cassettes_and_strict_errors_only_after_a_miss() {
    let s = start_with(Config {
        strict: true,
        ..Config::default()
    })
    .await;
    put_cassettes(&s, &cassette("c", vec![exchange("hello", "recorded")])).await;
    assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "hello")).await), "recorded");
    let miss = chat(&s, &chat_body("gpt-4o", "unknown")).await;
    assert_eq!(miss.status, 400);
    assert_eq!(miss.json()["error"]["code"], "fakellm_no_matching_rule");
    put_rules(&s, json!([{"match": {"last_user": "hello"}, "reply": {"text": "scripted"}}])).await;
    assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "hello")).await), "scripted");
}

#[tokio::test]
async fn recorded_errors_replay_with_status_headers_and_in_stream() {
    let s = start().await;
    let mut e = exchange("limited", "");
    e["outcome"] = json!({"outcome": "error", "error": {
        "status": 429, "kind": "rate_limit_error", "code": "rate_limit_exceeded", "message": "slow down",
        "headers": [["x-should-retry", "true"]],
    }});
    let mut s_in = exchange("streamed", "");
    s_in["outcome"] = json!({"outcome": "error", "error": {
        "status": 500, "kind": "server_error", "message": "boom", "in_stream": true,
    }});
    put_cassettes(&s, &cassette("errs", vec![e, s_in])).await;
    let r = chat(&s, &chat_body("gpt-4o", "limited")).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.json()["error"]["message"], "slow down");
    assert_eq!(r.header("x-should-retry"), Some("true"));
    let mut b = chat_body("gpt-4o", "streamed");
    b["stream"] = json!(true);
    let mut st = open_stream(&s, &b).await;
    assert_eq!(st.status, 200);
    let frames = st.all().await;
    assert!(frames.iter().any(|f| f.contains("boom")), "{frames:?}");
}

#[tokio::test]
async fn log_source_hits_and_consumed() {
    let s = start().await;
    let mut c = cassette("tape", vec![exchange("a", "A"), exchange("b", "B")]);
    c["once"] = json!(true);
    put_cassettes(&s, &c).await;
    chat(&s, &chat_body("gpt-4o", "b")).await;
    chat(&s, &chat_body("gpt-4o", "nothing")).await;
    let hits = log(&s, "source=cassette").await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["source"], json!({"kind": "cassette", "name": "tape", "index": 1}));
    assert_eq!(hits[0]["rule"], Value::Null);
    assert_eq!(log(&s, "source=fallback").await.len(), 1);
    let info = &get(&s, "/_fakellm/cassettes").await.json()["cassettes"][0];
    assert_eq!((info["hits"].as_u64(), info["consumed"].as_u64()), (Some(1), Some(1)));
}

#[tokio::test]
async fn embeddings_never_match_a_cassette() {
    let s = start().await;
    put_cassettes(&s, &cassette("c", vec![exchange("hello", "recorded")])).await;
    let r = post_json(&s, "/v1/embeddings", &json!({"model": "text-embedding-3-small", "input": "hello"})).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["object"], "list");
    assert!(log(&s, "source=cassette").await.is_empty());
}

// ---- cassette to trajectory ----

fn agent_cassette() -> Value {
    let mut first = exchange("fix a.rs", "");
    first["outcome"]["reply"] = json!({"tool_calls": [{"id": "call_1", "name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}], "finish": "tool_calls",
        "usage": {"prompt_tokens": 5, "completion_tokens": 2}});
    let mut side = exchange("write a title for this chat", "A title");
    side["request"]["model"] = json!("gpt-4o-mini");
    let mut second = exchange("fix a.rs", "Fixed.");
    second["request"]["messages"] = json!([
        user_msg("fix a.rs"),
        {"role": "assistant", "content": [], "tool_calls": [{"id": "call_1", "name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}]},
        {"role": "tool", "content": [{"type": "text", "text": "fn main() {}"}], "tool_call_id": "call_1"},
    ]);
    let mut failed = exchange("fix a.rs", "");
    failed["outcome"] = json!({"outcome": "error", "error": {"status": 500, "kind": "server_error", "message": "boom"}});
    cassette("session", vec![failed, first, side, second])
}

#[test]
fn to_trajectory_drops_side_requests_and_errors_and_adds_expectations() {
    let c: fakellm::Cassette = serde_json::from_value(agent_cassette()).unwrap();
    let t = c
        .to_trajectory(&fakellm::ToTrajectory {
            with_expect: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(t.name, "session");
    assert_eq!(t.steps.len(), 2, "the side request and the error drop out");
    assert_eq!(t.steps[0].reply.tool_calls.as_ref().unwrap()[0].name, "read_file");
    assert_eq!(t.steps[1].reply.text.as_deref(), Some("Fixed."));
    assert!(t.steps[0].expect.is_none());
    let expect = t.steps[1].expect.as_ref().unwrap();
    assert_eq!(expect.tool_results.len(), 1);
    t.validate().unwrap();

    let with_errors = c
        .to_trajectory(&fakellm::ToTrajectory {
            keep_errors: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        with_errors.steps.len(),
        2,
        "the error is step 0; the retry that follows it drops out"
    );
    assert!(with_errors.steps[0].reply.error.is_some());

    let none = c.to_trajectory(&fakellm::ToTrajectory {
        model: Some("claude-*".into()),
        ..Default::default()
    });
    assert!(none.is_err());
}

#[test]
fn cli_cassette_to_trajectory() {
    let input = temp_path("session.json");
    std::fs::write(&input, agent_cassette().to_string()).unwrap();
    let out = temp_path("traj.jsonl");
    let res = std::process::Command::new(env!("CARGO_BIN_EXE_fakellm"))
        .args([
            "cassette",
            "to-trajectory",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--with-expect",
            "--name",
            "fix",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(res.status.success(), "{stderr}");
    assert!(stderr.contains("2 of 4"), "{stderr}");
    let all = fakellm::Trajectory::parse_many(&std::fs::read_to_string(&out).unwrap(), fakellm::TrajectoryFormat::Auto).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!((all[0].name.as_str(), all[0].steps.len()), ("fix", 2));

    // A model filter that keeps nothing fails.
    let res = std::process::Command::new(env!("CARGO_BIN_EXE_fakellm"))
        .args([
            "cassette",
            "to-trajectory",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--model",
            "claude-*",
        ])
        .output()
        .unwrap();
    assert!(!res.status.success());
}
