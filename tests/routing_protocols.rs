//! Routing and matching details on all three protocols over raw HTTP: `stream.args_chunk_bytes`,
//! `last_user_raw` vs `last_user`, and path tolerance (gateway prefixes, `Config::path_prefix`).

use serde_json::{Value, json};

mod common;
use common::*;
#[path = "common/protocols.rs"]
mod protocols;
use protocols::*;

// ---- args_chunk_bytes ----------------------------------------------------------------------

const ARGS: &str = r#"{"path":"src/main.rs","text":"hello world"}"#; // 41 bytes, ASCII

fn tool_rule(stream: Value) -> Value {
    json!([{"reply": {"tool_calls": [{"name": "edit_file", "arguments": ARGS, "id": "call_1"}], "stream": stream}}])
}

/// The argument fragments a streaming client would see, in order.
async fn fragments(s: &fakellm::Server, p: P) -> Vec<String> {
    let mut body = p.body("go", &[], &["edit_file"]);
    body["stream"] = json!(true);
    let mut sse = open_sse(s, p.path(), p.headers(), &body).await;
    assert_eq!(sse.status, 200, "{p:?}");
    let mut out = vec![];
    for ev in sse.events().await {
        if ev.data == "[DONE]" {
            continue;
        }
        let v = ev.json();
        let frag = match p {
            P::Chat => v["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .map(str::to_owned),
            P::Responses if ev.event.as_deref() == Some("response.function_call_arguments.delta") => v["delta"].as_str().map(str::to_owned),
            P::Anthropic if v["delta"]["type"] == "input_json_delta" => v["delta"]["partial_json"].as_str().map(str::to_owned),
            _ => None,
        };
        out.extend(frag.filter(|f| !f.is_empty()));
    }
    out
}

#[tokio::test]
async fn args_chunk_bytes_sizes_the_fragments_on_every_protocol() {
    for p in ALL {
        let s = start().await;
        for n in [5usize, 16, 100] {
            put_rules(&s, tool_rule(json!({"args_chunk_bytes": n}))).await;
            let frags = fragments(&s, p).await;
            assert_eq!(frags.concat(), ARGS, "{p:?} n={n}");
            assert!(frags.iter().all(|f| f.len() <= n), "{p:?} n={n}: {frags:?}");
            assert_eq!(frags.len(), ARGS.len().div_ceil(n), "{p:?} n={n}: {frags:?}");
        }
    }
}

#[tokio::test]
async fn args_chunk_bytes_server_default_and_rule_override() {
    let s = start_with(fakellm::Config {
        delivery: fakellm::Delivery {
            args_chunk_bytes: 10,
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    for p in ALL {
        put_rules(&s, tool_rule(json!({}))).await;
        let frags = fragments(&s, p).await;
        assert_eq!(frags.len(), ARGS.len().div_ceil(10), "{p:?} default: {frags:?}");
        put_rules(&s, tool_rule(json!({"args_chunk_bytes": 20}))).await;
        let frags = fragments(&s, p).await;
        assert_eq!(frags.len(), ARGS.len().div_ceil(20), "{p:?} override: {frags:?}");
    }
}

#[tokio::test]
async fn args_chunk_bytes_zero_is_rejected() {
    let s = start().await;
    let r = send(
        &s,
        "PUT",
        "/_fakellm/scripts",
        &[("content-type", "application/json")],
        &tool_rule(json!({"args_chunk_bytes": 0})).to_string(),
    )
    .await;
    assert_eq!(r.status, 400, "{}", r.body);
}

#[tokio::test]
async fn args_chunk_bytes_does_not_split_sync_replies() {
    for p in ALL {
        let s = start().await;
        put_rules(&s, tool_rule(json!({"args_chunk_bytes": 3}))).await;
        let c = p.calls(&p.send(&s, "go", &[], &["edit_file"]).await);
        assert_eq!(c[0].1, serde_json::from_str::<Value>(ARGS).unwrap(), "{p:?}");
    }
}

// ---- last_user / last_user_raw -------------------------------------------------------------

const REMINDER: &str = "<system-reminder>\n# currentDate\nToday is 2026-01-01.\n</system-reminder>";

/// A user turn with a reminder block in front, in each protocol's shape.
fn with_reminder(p: P, prompt: &str) -> Value {
    let text = format!("{REMINDER}\n{prompt}");
    match p {
        P::Chat => json!({"model": "gpt-4o", "messages": [{"role": "user", "content": text}]}),
        P::Responses => json!({"model": "gpt-4o", "input": [
            {"role": "user", "content": [{"type": "input_text", "text": text}]}]}),
        P::Anthropic => json!({"model": "claude-sonnet-4", "max_tokens": 16, "messages": [
            {"role": "user", "content": [{"type": "text", "text": REMINDER}, {"type": "text", "text": prompt}]}]}),
    }
}

async fn ask(s: &fakellm::Server, p: P, body: &Value) -> String {
    let r = post_json_with(s, p.path(), p.headers(), body).await;
    p.text(&r)
}

#[tokio::test]
async fn last_user_strips_and_last_user_raw_keeps_reminders_on_every_protocol() {
    for p in ALL {
        let s = start().await;
        put_rules(
            &s,
            json!([
                {"match": {"last_user": "^fix the bug$"}, "reply": {"text": "stripped"}},
                {"reply": {"text": "other"}}
            ]),
        )
        .await;
        assert_eq!(ask(&s, p, &with_reminder(p, "fix the bug")).await, "stripped", "{p:?}");
        assert_eq!(ask(&s, p, &with_reminder(p, "fix the bug please")).await, "other", "{p:?}");

        put_rules(
            &s,
            json!([
                {"match": {"last_user_raw": "system-reminder"}, "reply": {"text": "raw"}},
                {"reply": {"text": "other"}}
            ]),
        )
        .await;
        // Anthropic keeps content blocks apart, so the reminder block is its own text; the match still sees it.
        assert_eq!(ask(&s, p, &with_reminder(p, "fix the bug")).await, "raw", "{p:?}");
        assert_eq!(ask(&s, p, &p.body("fix the bug", &[], &[])).await, "other", "{p:?}");

        // Both fields together: the raw one is a guard, the stripped one the real condition.
        put_rules(
            &s,
            json!([
                {"match": {"last_user_raw": "currentDate", "last_user": "^fix the bug$"}, "reply": {"text": "both"}},
                {"reply": {"text": "other"}}
            ]),
        )
        .await;
        assert_eq!(ask(&s, p, &with_reminder(p, "fix the bug")).await, "both", "{p:?}");
        assert_eq!(ask(&s, p, &p.body("fix the bug", &[], &[])).await, "other", "{p:?}");
    }
}

// ---- path tolerance ------------------------------------------------------------------------

fn standard_paths(prefix: &str) -> Vec<(P, String)> {
    ALL.iter().map(|p| (*p, format!("{prefix}{}", p.path()))).collect()
}

#[tokio::test]
async fn any_leading_segments_reach_every_generation_route() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "routed"}}])).await;
    for prefix in ["", "/anthropic", "/gateway/v2/proxy", "/team/a/b/c", "/openai"] {
        for (p, path) in standard_paths(prefix) {
            let r = post_json_with(&s, &path, p.headers(), &p.body("hi", &[], &[])).await;
            assert_eq!(p.text(&r), "routed", "{path}");
        }
    }
    // The log keeps the path the client used and the protocol it was served as.
    let all = log(&s, "").await;
    assert_eq!(all.len(), 15);
    assert!(
        all.iter()
            .any(|e| e["path"] == "/gateway/v2/proxy/v1/messages" && e["protocol"] == "anthropic.messages")
    );
    assert!(
        all.iter()
            .any(|e| e["path"] == "/team/a/b/c/v1/responses" && e["protocol"] == "openai.responses")
    );
}

#[tokio::test]
async fn prefixed_streams_and_side_routes_on_every_protocol() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "streamed words arrive"}}])).await;
    for (p, path) in standard_paths("/gw") {
        let mut body = p.body("hi", &[], &[]);
        body["stream"] = json!(true);
        let mut sse = open_sse(&s, &path, p.headers(), &body).await;
        assert_eq!(sse.status, 200, "{path}");
        assert!(
            sse.headers["content-type"].to_str().unwrap().starts_with("text/event-stream"),
            "{path}"
        );
        let frames = sse.all().await.join("\n");
        assert!(frames.contains("words"), "{path}: {frames}");
    }
    // The routes next to generation.
    assert_eq!(get(&s, "/gw/v1/models").await.status, 200);
    assert_eq!(get(&s, "/gw/v1/models/gpt-4o").await.status, 200);
    let r = post_json_with(
        &s,
        "/gw/v1/messages/count_tokens",
        &[("anthropic-version", "2023-06-01")],
        &P::Anthropic.body("hi", &[], &[]),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.json()["input_tokens"].as_u64().unwrap() > 0);
    let r = post_json(
        &s,
        "/gw/v1/embeddings",
        &json!({"model": "text-embedding-3-small", "input": "x", "dimensions": 4}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);

    let r = post_json(&s, "/gw/v1/responses", &json!({"model": "gpt-4o", "input": "hi"})).await;
    let id = r.json()["id"].as_str().unwrap().to_owned();
    assert_eq!(get(&s, &format!("/gw/v1/responses/{id}")).await.status, 200);
    assert_eq!(send(&s, "DELETE", &format!("/gw/v1/responses/{id}"), &[], "").await.status, 200);
}

#[tokio::test]
async fn configured_prefix_covers_routes_without_v1_on_every_protocol() {
    let s = start_with(fakellm::Config {
        path_prefix: Some("/proxy/".into()),
        ..Default::default()
    })
    .await;
    put_rules(&s, json!([{"reply": {"text": "routed"}}])).await;
    let bare = |p: P| match p {
        P::Chat => "chat/completions",
        P::Responses => "responses",
        P::Anthropic => "messages",
    };
    for p in ALL {
        for path in [
            format!("/proxy/{}", bare(p)),
            format!("/proxy/v1/{}", bare(p)),
            format!("/v1/{}", bare(p)),
        ] {
            let r = post_json_with(&s, &path, p.headers(), &p.body("hi", &[], &[])).await;
            assert_eq!(p.text(&r), "routed", "{path}");
        }
    }
    assert_eq!(get(&s, "/proxy/models").await.status, 200);
    let r = post_json(
        &s,
        "/proxy/embeddings",
        &json!({"model": "text-embedding-3-small", "input": "x", "dimensions": 4}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // Segment boundaries: `proxyx` is not the prefix, and an unknown path is not guessed.
    assert_eq!(post_json(&s, "/proxyx/nothing", &json!({})).await.status, 404);
    assert_eq!(
        post_json(&s, "/other/chat/completions", &chat_body("gpt-4o", "hi")).await.status,
        200,
        "known suffixes still tolerate any prefix"
    );
}

#[tokio::test]
async fn unknown_paths_are_404_in_each_protocol_envelope() {
    let s = start().await;
    let r = post_json(&s, "/foo/bar", &json!({})).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["message"], "Invalid URL (POST /foo/bar)");
    // An Anthropic client gets Anthropic's error shape.
    let r = post_json_with(&s, "/foo/bar", &[("anthropic-version", "2023-06-01")], &json!({})).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["type"], "error", "{}", r.body);
    // Near misses do not route.
    for path in ["/v1/message", "/v1/messages2", "/chat/completionsx", "/responses_x"] {
        assert_eq!(post_json(&s, path, &json!({})).await.status, 404, "{path}");
    }
    assert_eq!(get(&s, "/_fakellm").await.status, 200, "the control plane is never rerouted");
}
