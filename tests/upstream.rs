//! Upstream proxy mode over HTTP: a second fakellm instance is the "real" server (docs/m4-spec.md section 4 and 11).

use std::time::Duration;

use fakellm::{Config, Server};
use serde_json::{Value, json};

mod common;
use common::*;

fn proxy_config(upstream: &Server) -> Config {
    Config {
        upstream: Some(upstream.base_url().parse().unwrap()),
        ..Config::default()
    }
}

/// An upstream with `rules`, and a proxy pointing at it.
async fn pair(rules: Value) -> (Server, Server) {
    let up = start().await;
    put_rules(&up, rules).await;
    let proxy = start_with(proxy_config(&up)).await;
    (up, proxy)
}

/// Like [`pair`], with `--record-errors`.
async fn pair_recording_errors(rules: Value) -> (Server, Server) {
    let up = start().await;
    put_rules(&up, rules).await;
    let proxy = start_with(Config {
        record_errors: true,
        ..proxy_config(&up)
    })
    .await;
    (up, proxy)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// The recording of `s` once it holds `n` exchanges (the exchange is recorded when the relayed body ends).
async fn recording(s: &Server, n: usize) -> Value {
    eventually(5.0, || async {
        let r = get(s, "/_fakellm/cassettes/recording").await;
        (r.status == 200 && r.json()["exchanges"].as_array().is_some_and(|e| e.len() >= n)).then(|| r.json())
    })
    .await
    .unwrap_or_else(|| panic!("recording never reached {n} exchanges"))
}

fn header_of<'a>(record: &'a Value, name: &str) -> Option<&'a str> {
    record["headers"]
        .as_array()?
        .iter()
        .find(|h| h[0] == name)
        .and_then(|h| h[1].as_str())
}

// ---- the core round trip ----

#[tokio::test]
async fn relays_records_and_the_recording_replays_without_the_upstream() {
    let (up, proxy) = pair(json!([{"match": {"last_user": "weather"}, "reply": {
        "text": "Sunny in Paris", "usage": {"prompt_tokens": 21, "completion_tokens": 5}}}]))
    .await;
    let record_file = temp_path("record.jsonl");
    drop(proxy);
    let proxy = start_with(Config {
        record: Some(record_file.clone()),
        ..proxy_config(&up)
    })
    .await;

    let direct = chat(&up, &chat_body("gpt-4o", "weather")).await.json();
    let via = chat(&proxy, &chat_body("gpt-4o", "weather")).await;
    assert_eq!(via.status, 200, "{}", via.body);
    let via = via.json();
    assert_eq!(via["choices"][0]["message"]["content"], direct["choices"][0]["message"]["content"]);
    assert_eq!(via["choices"][0]["finish_reason"], direct["choices"][0]["finish_reason"]);
    assert_eq!(via["usage"], direct["usage"]);
    assert_eq!(via["model"], direct["model"]);

    // Proxy log: source upstream. Upstream log: the client's raw body, unchanged.
    let plog = log(&proxy, "").await;
    assert_eq!(plog.len(), 1);
    assert_eq!(plog[0]["source"]["kind"], "upstream");
    let ulog = log(&up, "").await;
    assert_eq!(ulog.len(), 2);
    assert_eq!(ulog[1]["raw"], chat_body("gpt-4o", "weather"));

    // The recording holds one exchange, in memory and in the file.
    let rec = recording(&proxy, 1).await;
    assert_eq!(rec["exchanges"].as_array().unwrap().len(), 1);
    let ex = &rec["exchanges"][0];
    assert_eq!(ex["outcome"]["reply"]["text"], "Sunny in Paris");
    assert_eq!(ex["outcome"]["reply"]["finish"], "stop");
    assert_eq!(ex["outcome"]["reply"]["usage"]["prompt_tokens"], 21);
    assert_eq!(ex["meta"]["upstream_status"], 200);
    assert!(ex["meta"]["duration_ms"].is_u64());
    assert_eq!(ex["meta"]["model"], "gpt-4o");

    let text = std::fs::read_to_string(&record_file).unwrap();
    let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2, "header + one exchange: {text}");
    assert_eq!(lines[0]["kind"], "cassette");
    assert!(lines[1].get("request").is_some());

    // Load the file as a cassette into a third server with no upstream.
    let third = start().await;
    let r = send(
        &third,
        "PUT",
        "/_fakellm/cassettes",
        &[("content-type", "application/x-ndjson")],
        &text,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let replay = chat(&third, &chat_body("gpt-4o", "weather")).await.json();
    assert_eq!(replay["choices"][0]["message"]["content"], "Sunny in Paris");
    assert_eq!(replay["usage"]["prompt_tokens"], 21);
    assert_eq!(log(&third, "source=cassette").await.len(), 1);

    // The same recording on the proxy with the upstream stopped.
    drop(up);
    let again = chat(&proxy, &chat_body("gpt-4o", "weather")).await;
    assert_eq!(again.status, 502, "no cassette registered yet, the upstream is gone");
    let mut replayed = rec.clone();
    replayed["name"] = json!("replayed");
    put_cassettes(&proxy, &replayed).await;
    assert_eq!(chat_text(&chat(&proxy, &chat_body("gpt-4o", "weather")).await), "Sunny in Paris");
}

#[tokio::test]
async fn every_protocol_sync_and_streaming() {
    let (up, proxy) = pair(json!([{"reply": {
        "text": "alpha beta gamma",
        "reasoning": "thinking it over",
        "usage": {"prompt_tokens": 30, "completion_tokens": 9, "cached_tokens": 12},
    }}]))
    .await;

    // Chat
    let r = chat(&proxy, &chat_body("gpt-4o", "go")).await;
    assert_eq!(r.json()["choices"][0]["message"]["content"], "alpha beta gamma");
    let mut b = chat_body("gpt-4o", "go");
    b["stream"] = json!(true);
    b["stream_options"] = json!({"include_usage": true});
    let frames = open_stream(&proxy, &b).await.all().await;
    assert_eq!(frames.last().map(String::as_str), Some("[DONE]"));
    let text: String = json_chunks(&frames)
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(text, "alpha beta gamma");

    // Responses
    let j = responses(&proxy, &responses_body("gpt-5.5", "go")).await.json();
    assert_eq!(j["status"], "completed");
    let mut b = responses_body("gpt-5.5", "go");
    b["stream"] = json!(true);
    let events = open_sse(&proxy, "/v1/responses", &[], &b).await.events().await;
    assert_eq!(events.last().unwrap().json()["type"], "response.completed");

    // Anthropic
    let j = anthropic(&proxy, &anthropic_body("claude-sonnet-4-5", "go")).await.json();
    assert_eq!(j["content"].as_array().unwrap().last().unwrap()["text"], "alpha beta gamma");
    let mut b = anthropic_body("claude-sonnet-4-5", "go");
    b["stream"] = json!(true);
    let events = open_sse(&proxy, "/v1/messages", ANTHROPIC_HEADERS, &b).await.events().await;
    assert_eq!(events.last().unwrap().event.as_deref(), Some("message_stop"));

    // Embeddings
    let r = post_json(
        &proxy,
        "/v1/embeddings",
        &json!({"model": "text-embedding-3-small", "input": "hello"}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["object"], "list");

    // 7 relayed calls: logged as upstream on the proxy, seen by the upstream, recorded once each.
    assert_eq!(log(&proxy, "source=upstream").await.len(), 7);
    assert_eq!(log(&up, "").await.len(), 7);
    let rec = recording(&proxy, 7).await;
    let exchanges = rec["exchanges"].as_array().unwrap();
    assert_eq!(exchanges.len(), 7);
    for e in &exchanges[..6] {
        assert_eq!(e["outcome"]["reply"]["text"], "alpha beta gamma", "{e}");
        assert_eq!(e["outcome"]["reply"]["reasoning"], "thinking it over", "{e}");
        assert_eq!(e["meta"]["upstream_status"], 200);
    }
    // Usage incl. cache tokens survives each wire format. The Anthropic IR keeps the total, and a rule's prompt count
    // is literal for an Anthropic request (m3-spec D5): `input_tokens` 30 plus 12 cache reads is 42.
    for e in &exchanges[..6] {
        let u = &e["outcome"]["reply"]["usage"];
        let prompt = if e["protocol"] == "anthropic.messages" { 42 } else { 30 };
        assert_eq!(u["prompt_tokens"], prompt, "{e}");
        assert_eq!(u["completion_tokens"], 9, "{e}");
        assert_eq!(u["cached_tokens"], 12, "{e}");
    }
}

#[tokio::test]
async fn streamed_tool_call_arguments_are_reassembled() {
    let (_up, proxy) = pair(json!([{"reply": {
        "tool_calls": [{"id": "call_w1", "name": "get_weather", "arguments": {"city": "Paris", "unit": "celsius"}}],
        "stream": {"chunk_tokens": 1},
    }}]))
    .await;
    let mut b = chat_body("gpt-4o", "weather");
    b["stream"] = json!(true);
    let frames = open_stream(&proxy, &b).await.all().await;
    assert!(frames.len() > 4);
    let rec = recording(&proxy, 1).await;
    let reply = &rec["exchanges"][0]["outcome"]["reply"];
    assert_eq!(reply["finish"], "tool_calls");
    assert_eq!(reply["tool_calls"][0]["id"], "call_w1");
    assert_eq!(reply["tool_calls"][0]["name"], "get_weather");
    let args: Value = serde_json::from_str(reply["tool_calls"][0]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args, json!({"city": "Paris", "unit": "celsius"}));
}

// ---- precedence ----

#[tokio::test]
async fn scripts_and_cassettes_on_the_proxy_beat_the_upstream() {
    let (up, proxy) = pair(json!([{"reply": {"text": "from upstream"}}])).await;
    put_rules(
        &proxy,
        json!([{"match": {"last_user": "local"}, "reply": {"text": "from proxy rule"}}]),
    )
    .await;
    assert_eq!(chat_text(&chat(&proxy, &chat_body("gpt-4o", "local")).await), "from proxy rule");
    assert!(log(&up, "").await.is_empty());

    put_cassettes(&proxy, &cassette("c", vec![exchange("taped", "from cassette")])).await;
    assert_eq!(chat_text(&chat(&proxy, &chat_body("gpt-4o", "taped")).await), "from cassette");
    assert!(log(&up, "").await.is_empty(), "a cassette hit never reaches the upstream");
    assert_eq!(chat_text(&chat(&proxy, &chat_body("gpt-4o", "other")).await), "from upstream");
    assert_eq!(log(&up, "").await.len(), 1);
    // A cassette hit is not recorded.
    let rec = recording(&proxy, 1).await;
    assert_eq!(rec["exchanges"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_fault_with_times_one_then_the_real_answer() {
    let (up, proxy) = pair(json!([{"reply": {"text": "real answer"}}])).await;
    put_rules(&proxy, json!([{"reply": {"error": {"status": 429, "retry_after": 1}}, "times": 1}])).await;
    let first = chat(&proxy, &chat_body("gpt-4o", "hi")).await;
    assert_eq!(first.status, 429);
    assert_eq!(chat_text(&chat(&proxy, &chat_body("gpt-4o", "hi")).await), "real answer");
    assert_eq!(log(&up, "").await.len(), 1);
}

#[tokio::test]
async fn strict_proxy_still_relays() {
    let up = start().await;
    put_rules(&up, json!([{"reply": {"text": "up"}}])).await;
    let proxy = start_with(Config {
        strict: true,
        ..proxy_config(&up)
    })
    .await;
    assert_eq!(chat_text(&chat(&proxy, &chat_body("gpt-4o", "hi")).await), "up");
}

// ---- errors ----

#[tokio::test]
async fn upstream_errors_are_relayed_and_recorded_with_record_errors() {
    let (_up, proxy) = pair_recording_errors(json!([
        {"match": {"last_user": "limited"}, "reply": {"error": {"status": 429, "retry_after": 3, "message": "slow down"}}},
        {"match": {"last_user": "broken"}, "reply": {"error": {"status": 500, "message": "boom"}}},
    ]))
    .await;
    let r = chat(&proxy, &chat_body("gpt-4o", "limited")).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.header("retry-after"), Some("3"));
    assert_eq!(r.json()["error"]["message"], "slow down");
    let r = chat(&proxy, &chat_body("gpt-4o", "broken")).await;
    assert_eq!(r.status, 500);
    let r = anthropic(&proxy, &anthropic_body("claude-sonnet-4-5", "broken")).await;
    assert_eq!(r.status, 500);
    assert_eq!(r.json()["type"], "error");

    let rec = recording(&proxy, 3).await;
    let ex = rec["exchanges"].as_array().unwrap();
    assert_eq!(ex[0]["outcome"]["outcome"], "error");
    assert_eq!(ex[0]["outcome"]["error"]["status"], 429);
    assert_eq!(ex[0]["meta"]["upstream_status"], 429);
    assert_eq!(ex[1]["outcome"]["error"]["status"], 500);
    assert_eq!(ex[2]["outcome"]["error"]["message"], "boom");
}

#[tokio::test]
async fn transient_upstream_errors_are_relayed_but_not_recorded_by_default() {
    let (_up, proxy) = pair(json!([
        {"match": {"last_user": "limited"}, "reply": {"error": {"status": 429, "message": "slow down"}}},
        {"match": {"last_user": "bad"}, "reply": {"error": {"status": 400, "message": "nope"}}},
    ]))
    .await;
    assert_eq!(chat(&proxy, &chat_body("gpt-4o", "limited")).await.status, 429);
    assert_eq!(chat(&proxy, &chat_body("gpt-4o", "bad")).await.status, 400);
    let rec = recording(&proxy, 1).await;
    let ex = rec["exchanges"].as_array().unwrap();
    assert_eq!(ex.len(), 1, "{rec}");
    assert_eq!(ex[0]["outcome"]["error"]["status"], 400);
}

#[tokio::test]
async fn in_stream_errors_are_relayed() {
    let (_up, proxy) = pair_recording_errors(
        json!([{"reply": {"error": {"status": 529, "type": "overloaded_error", "message": "busy", "in_stream": true}}}]),
    )
    .await;
    let mut b = anthropic_body("claude-sonnet-4-5", "hi");
    b["stream"] = json!(true);
    let events = open_sse(&proxy, "/v1/messages", ANTHROPIC_HEADERS, &b).await.events().await;
    assert!(events.iter().any(|e| e.event.as_deref() == Some("error")), "{events:?}");
    let rec = recording(&proxy, 1).await;
    assert_eq!(rec["exchanges"][0]["outcome"]["outcome"], "error");
    assert_eq!(rec["exchanges"][0]["outcome"]["error"]["in_stream"], true);
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_502_in_every_protocols_format() {
    let port = free_port();
    let proxy = start_with(Config {
        upstream: Some(format!("http://127.0.0.1:{port}").parse().unwrap()),
        ..Config::default()
    })
    .await;
    let r = chat(&proxy, &chat_body("gpt-4o", "hi")).await;
    assert_eq!(r.status, 502);
    assert_eq!(r.json()["error"]["code"], "fakellm_upstream_unreachable");
    let r = responses(&proxy, &responses_body("gpt-5.5", "hi")).await;
    assert_eq!(r.status, 502);
    assert_eq!(r.json()["error"]["code"], "fakellm_upstream_unreachable");
    let r = anthropic(&proxy, &anthropic_body("claude-sonnet-4-5", "hi")).await;
    assert_eq!(r.status, 502);
    assert_eq!(r.json()["type"], "error");
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("did not answer"));
    let logged = log(&proxy, "").await;
    assert_eq!(logged.len(), 3);
    assert!(logged.iter().all(|l| l["status"] == 502));
    let rec = get(&proxy, "/_fakellm/cassettes/recording").await;
    assert!(
        rec.status == 404 || rec.json()["exchanges"].as_array().is_some_and(Vec::is_empty),
        "{}",
        rec.body
    );
}

// ---- other routes ----

#[tokio::test]
async fn control_plane_models_and_count_tokens_stay_local() {
    let (up, proxy) = pair(json!([{"reply": {"text": "up"}}])).await;
    let status = get(&proxy, "/_fakellm").await.json();
    assert!(status["upstream"]["url"].as_str().is_some(), "{status}");
    assert_eq!(get(&proxy, "/v1/models").await.status, 200);
    let r = anthropic_count(&proxy).await;
    assert_eq!(r, 200);
    assert!(log(&up, "").await.is_empty());
}

async fn anthropic_count(s: &Server) -> u16 {
    post_json_with(
        s,
        "/v1/messages/count_tokens",
        ANTHROPIC_HEADERS,
        &anthropic_body("claude-sonnet-4-5", "hi"),
    )
    .await
    .status
}

#[tokio::test]
async fn responses_chain_through_the_proxy() {
    let (up, proxy) = pair(json!([{"match": {"last_user": "one"}, "reply": {"text": "first"}}, {"reply": {"text": "second"}}])).await;
    let first = responses(&proxy, &responses_body("gpt-5.5", "one")).await.json();
    let id = first["id"].as_str().unwrap().to_owned();
    let mut b = responses_body("gpt-5.5", "two");
    b["previous_response_id"] = json!(id);
    let second = responses(&proxy, &b).await;
    assert_eq!(second.status, 200, "{}", second.body);

    // The recorded IR of the second request has the history; the upstream saw the original bytes.
    let rec = recording(&proxy, 2).await;
    let msgs = rec["exchanges"][1]["request"]["messages"].as_array().unwrap();
    assert!(msgs.len() >= 3, "history missing: {msgs:?}");
    let ulog = log(&up, "").await;
    assert_eq!(ulog[1]["raw"]["previous_response_id"], json!(id));

    // A restarted proxy does not know the id: forwarded with partial IR, still answered.
    let fresh = start_with(proxy_config(&up)).await;
    let r = responses(&fresh, &b).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

async fn ask(s: &Server, path: &str, headers: &[(&str, &str)], body: &Value) {
    let mut h = vec![("content-type", "application/json")];
    h.extend_from_slice(headers);
    send(s, "POST", path, &h, &body.to_string()).await;
}

async fn last(s: &Server) -> Value {
    log(s, "").await.pop().unwrap()
}

// ---- credentials (table in spec 4.2) ----

#[tokio::test]
async fn credentials_follow_the_table() {
    let up = start().await;
    put_rules(&up, json!([{"reply": {"text": "ok"}}])).await;
    // No upstream key: client credentials pass through (both when both are present).
    let proxy = start_with(proxy_config(&up)).await;
    ask(
        &proxy,
        "/v1/chat/completions",
        &[("authorization", "Bearer client-key"), ("x-api-key", "client-x")],
        &chat_body("gpt-4o", "a"),
    )
    .await;
    let l = last(&up).await;
    assert_eq!(header_of(&l, "authorization"), Some("Bearer client-key"));
    assert_eq!(header_of(&l, "x-api-key"), Some("client-x"));
    // Neither sent: neither forwarded. An Anthropic request without a version gets one.
    ask(&proxy, "/v1/messages", &[], &anthropic_body("claude-sonnet-4-5", "b")).await;
    let l = last(&up).await;
    assert_eq!(header_of(&l, "authorization"), None);
    assert_eq!(header_of(&l, "x-api-key"), None);
    assert_eq!(header_of(&l, "anthropic-version"), Some("2023-06-01"));
    ask(
        &proxy,
        "/v1/messages",
        &[("anthropic-version", "2024-01-01"), ("anthropic-beta", "x-beta")],
        &anthropic_body("claude-sonnet-4-5", "c"),
    )
    .await;
    let l = last(&up).await;
    assert_eq!(header_of(&l, "anthropic-version"), Some("2024-01-01"));
    assert_eq!(header_of(&l, "anthropic-beta"), Some("x-beta"));

    // Upstream key: both client headers replaced; Anthropic also gets x-api-key.
    let keyed = start_with(Config {
        upstream_api_key: Some("real-key".into()),
        ..proxy_config(&up)
    })
    .await;
    ask(
        &keyed,
        "/v1/chat/completions",
        &[("authorization", "Bearer client-key"), ("x-api-key", "client-x")],
        &chat_body("gpt-4o", "d"),
    )
    .await;
    let l = last(&up).await;
    assert_eq!(header_of(&l, "authorization"), Some("Bearer real-key"));
    assert_eq!(header_of(&l, "x-api-key"), None);
    ask(
        &keyed,
        "/v1/messages",
        &[("x-api-key", "client-x")],
        &anthropic_body("claude-sonnet-4-5", "e"),
    )
    .await;
    let l = last(&up).await;
    assert_eq!(header_of(&l, "authorization"), Some("Bearer real-key"));
    assert_eq!(header_of(&l, "x-api-key"), Some("real-key"));

    // fakellm's own key is never forwarded.
    let own = start_with(Config {
        api_key: Some("own-key".into()),
        ..proxy_config(&up)
    })
    .await;
    ask(
        &own,
        "/v1/chat/completions",
        &[("authorization", "Bearer own-key")],
        &chat_body("gpt-4o", "f"),
    )
    .await;
    let l = last(&up).await;
    assert_eq!(header_of(&l, "authorization"), None);
    ask(
        &own,
        "/v1/messages",
        &[("x-api-key", "own-key")],
        &anthropic_body("claude-sonnet-4-5", "g"),
    )
    .await;
    let l = last(&up).await;
    assert_eq!(header_of(&l, "x-api-key"), None);
    let all = log(&up, "").await;
    assert!(all.iter().all(|r| !r["headers"].to_string().contains("own-key")));
}

// ---- loops ----

#[tokio::test]
async fn a_cycle_of_instances_ends_in_a_quick_508() {
    let (pa, pb) = (free_port(), free_port());
    let node = |port: u16, other: u16| Config {
        host: "127.0.0.1".into(),
        port,
        upstream: Some(format!("http://127.0.0.1:{other}").parse().unwrap()),
        ..Config::default()
    };
    let b = start_with(node(pb, pa)).await;
    let a = start_with(node(pa, pb)).await;
    let r = tokio::time::timeout(Duration::from_secs(5), chat(&a, &chat_body("gpt-4o", "hello")))
        .await
        .expect("a relay loop must end quickly");
    assert_eq!(r.status, 508);
    assert_eq!(r.json()["error"]["code"], "fakellm_upstream_loop");
    drop(b);
}

#[tokio::test]
async fn a_chain_of_instances_works_and_carries_the_hop_header() {
    let c = start().await;
    put_rules(&c, json!([{"reply": {"text": "from C"}}])).await;
    let b = start_with(proxy_config(&c)).await;
    let a = start_with(proxy_config(&b)).await;
    assert_eq!(chat_text(&chat(&a, &chat_body("gpt-4o", "hi")).await), "from C");
    let hop = header_of(&log(&c, "").await[0], "x-fakellm-hop").unwrap().to_owned();
    assert_eq!(hop.split(',').count(), 2, "A and B: {hop}");
    let a_id = hop.split(',').next().unwrap().trim().to_owned();

    // A request that already carries A's id is refused by A, before anything is sent upstream.
    let before = log(&b, "").await.len();
    let r = post_json_with(
        &a,
        "/v1/chat/completions",
        &[("x-fakellm-hop", &a_id)],
        &chat_body("gpt-4o", "again"),
    )
    .await;
    assert_eq!(r.status, 508, "{}", r.body);
    assert_eq!(r.json()["error"]["code"], "fakellm_upstream_loop");
    assert_eq!(log(&b, "").await.len(), before);
}

// ---- client disconnect ----

#[tokio::test]
async fn a_client_that_leaves_mid_stream_logs_incomplete_and_records_nothing() {
    let (_up, proxy) = pair(json!([{"reply": {
        "text": "one two three four five six seven eight nine ten",
        "stream": {"tokens_per_sec": 8},
    }}]))
    .await;
    let mut b = chat_body("gpt-4o", "hi");
    b["stream"] = json!(true);
    let mut st = open_stream(&proxy, &b).await;
    assert!(st.next().await.is_some());
    drop(st);
    let logged = eventually(5.0, || async {
        let l = log(&proxy, "").await;
        l.first()
            .filter(|r| r["reply"]["error"]["code"] == "fakellm_upstream_incomplete")
            .cloned()
    })
    .await;
    assert!(logged.is_some(), "no fakellm_upstream_incomplete in the log");
    let rec = get(&proxy, "/_fakellm/cassettes/recording").await;
    assert!(
        rec.status == 404 || rec.json()["exchanges"].as_array().is_some_and(Vec::is_empty),
        "{}",
        rec.body
    );
}

// ---- the real thing, when asked for ----

#[tokio::test]
async fn real_upstream_round_trip_when_configured() {
    let Ok(url) = std::env::var("FAKELLM_UPSTREAM_TEST") else {
        eprintln!("skip: FAKELLM_UPSTREAM_TEST is not set (an OpenAI-compatible URL; key in FAKELLM_UPSTREAM_TEST_KEY)");
        return;
    };
    let proxy = start_with(Config {
        upstream: Some(url.parse().unwrap()),
        upstream_api_key: std::env::var("FAKELLM_UPSTREAM_TEST_KEY").ok(),
        ..Config::default()
    })
    .await;
    let model = std::env::var("FAKELLM_UPSTREAM_TEST_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
    let mut b = chat_body(&model, "Say hi in one word.");
    b["max_tokens"] = json!(16);
    let r = chat(&proxy, &b).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let rec = recording(&proxy, 1).await;
    assert!(rec["exchanges"][0]["outcome"]["reply"]["text"].is_string());
}
