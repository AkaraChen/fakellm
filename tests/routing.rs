//! Path tolerance (gateway prefixes, `Config::path_prefix`) and `<system-reminder>` handling in `last_user` matching.

use serde_json::{Value, json};

mod common;
use common::*;

const H: [(&str, &str); 1] = [("anthropic-version", "2023-06-01")];

fn anthropic_body(user: Value) -> Value {
    json!({"model": "claude-sonnet-4", "max_tokens": 16, "messages": [{"role": "user", "content": user}]})
}

#[tokio::test]
async fn known_routes_behind_any_leading_segments() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "routed"}}])).await;

    let r = post_json_with(&s, "/anthropic/v1/messages", &H, &anthropic_body(json!("hi"))).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["content"][0]["text"], "routed");

    let r = post_json(&s, "/gateway/v2/proxy/v1/chat/completions", &chat_body("gpt-4o", "hi")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["choices"][0]["message"]["content"], "routed");

    let r = get(&s, "/x/v1/models").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.json()["data"].is_array());

    let r = post_json(
        &s,
        "/foo/embeddings",
        &json!({"model": "text-embedding-3-small", "input": "x", "dimensions": 4}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);

    let r = post_json(&s, "/team/a/v1/responses", &json!({"model": "gpt-4o", "input": "hi"})).await;
    assert_eq!(r.status, 200, "{}", r.body);

    let r = post_json_with(&s, "/anthropic/v1/messages/count_tokens", &H, &anthropic_body(json!("hi"))).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn unknown_paths_still_404_and_the_log_keeps_the_full_path() {
    let s = start().await;
    let r = post_json(&s, "/foo/bar", &json!({})).await;
    assert_eq!(r.status, 404, "{}", r.body);
    assert!(r.body.contains("/foo/bar"), "{}", r.body);

    post_json(&s, "/gateway/v1/chat/completions", &chat_body("gpt-4o", "hi")).await;
    let log = get(&s, "/_fakellm/requests?limit=1").await.json();
    assert_eq!(log["requests"][0]["path"], "/gateway/v1/chat/completions");
    assert_eq!(get(&s, "/_fakellm").await.status, 200, "the control plane is not rerouted");
}

#[tokio::test]
async fn configured_prefix_for_routes_without_v1() {
    let s = start_with(fakellm::Config {
        path_prefix: Some("/anthropic".into()),
        ..Default::default()
    })
    .await;
    for path in ["/anthropic/v1/messages", "/anthropic/messages", "/v1/messages"] {
        let r = post_json_with(&s, path, &H, &anthropic_body(json!("hi"))).await;
        assert_eq!(r.status, 200, "{path}: {}", r.body);
    }
    let r = post_json(&s, "/anthropic/chat/completions", &chat_body("gpt-4o", "hi")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = post_json(&s, "/anthropicx/nothing", &json!({})).await;
    assert_eq!(r.status, 404, "prefixes match on segment boundaries");
}

const REMINDER: &str = "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday is 2026-01-01.\n</system-reminder>";

async fn answer(s: &fakellm::Server, user: Value) -> String {
    let r = post_json_with(s, "/v1/messages", &H, &anthropic_body(user)).await;
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()["content"][0]["text"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn last_user_ignores_system_reminders() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"match": {"last_user": "^fix the bug$"}, "reply": {"text": "stripped match"}},
            {"reply": {"text": "other"}}
        ]),
    )
    .await;
    assert_eq!(answer(&s, json!(format!("{REMINDER}\nfix the bug"))).await, "stripped match");
    assert_eq!(
        answer(&s, json!(format!("{REMINDER}fix the bug{REMINDER}"))).await,
        "stripped match"
    );
    assert_eq!(
        answer(
            &s,
            json!([{"type": "text", "text": REMINDER}, {"type": "text", "text": "fix the bug"}])
        )
        .await,
        "stripped match"
    );
    assert_eq!(answer(&s, json!("fix the bug please")).await, "other");
}

#[tokio::test]
async fn reminder_only_message_falls_back_to_the_previous_user_text() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"match": {"last_user": "^fix the bug$"}, "reply": {"text": "found the prompt"}},
            {"reply": {"text": "other"}}
        ]),
    )
    .await;
    let body = json!({"model": "claude-sonnet-4", "max_tokens": 16, "messages": [
        {"role": "user", "content": "fix the bug"},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "data"}]},
        {"role": "user", "content": [{"type": "text", "text": REMINDER}]}
    ]});
    let r = post_json_with(&s, "/v1/messages", &H, &body).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["content"][0]["text"], "found the prompt");
}

#[tokio::test]
async fn last_user_raw_sees_the_reminder() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"match": {"last_user_raw": "system-reminder"}, "reply": {"text": "raw"}},
            {"reply": {"text": "other"}}
        ]),
    )
    .await;
    assert_eq!(answer(&s, json!(format!("{REMINDER}\nfix the bug"))).await, "raw");
    assert_eq!(answer(&s, json!("fix the bug")).await, "other");

    put_rules(
        &s,
        json!([{"match": {"last_user_raw": "^<system-reminder>"}, "reply": {"text": "anchored"}}]),
    )
    .await;
    assert_eq!(answer(&s, json!(format!("{REMINDER}\nfix the bug"))).await, "anchored");
}

#[tokio::test]
async fn any_message_and_system_stay_raw() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"match": {"any_message": "currentDate"}, "reply": {"text": "saw reminder"}}, {"reply": {"text": "other"}}]),
    )
    .await;
    assert_eq!(answer(&s, json!(format!("{REMINDER}\nhi"))).await, "saw reminder");
}
