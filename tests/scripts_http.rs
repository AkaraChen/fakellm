//! HTTP-level tests of the script docs examples, the request log controls, `cut_after_chunks` and `n > 1` streaming.

use serde_json::{Value, json};

mod common;
use common::*;

fn text_of(r: &Reply) -> String {
    r.json()["choices"][0]["message"]["content"].as_str().unwrap().to_owned()
}

fn tool_turn(result: &str) -> Value {
    json!({"model": "gpt-4o", "messages": [
        {"role": "user", "content": "read it"},
        {"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}]},
        {"role": "tool", "tool_call_id": "call_1", "content": result}
    ]})
}

#[tokio::test]
async fn tool_result_contains_and_last_tool_result() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"match": {"has_tool_result": true, "tool_result_contains": "ENOENT"}, "reply": {"text": "missing"}},
            {"match": {"last_tool_result": "read_file"}, "reply": {"text": "read ok"}},
            {"reply": {"text": "other"}}
        ]),
    )
    .await;
    assert_eq!(text_of(&chat(&s, &tool_turn("ENOENT: no such file")).await), "missing");
    assert_eq!(text_of(&chat(&s, &tool_turn("contents")).await), "read ok");
    assert_eq!(text_of(&chat(&s, &chat_body("gpt-4o", "hi")).await), "other");
}

#[tokio::test]
async fn any_message_and_turn() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"match": {"any_message": "TODO\\(alice\\)"}, "reply": {"text": "found alice's todo"}},
            {"match": {"turn": 0}, "reply": {"text": "first"}},
            {"reply": {"text": "later"}}
        ]),
    )
    .await;
    let body = json!({"model": "gpt-4o", "messages": [
        {"role": "system", "content": "notes"},
        {"role": "user", "content": "see TODO(alice) here"},
        {"role": "assistant", "content": "ok"},
        {"role": "user", "content": "and?"}
    ]});
    assert_eq!(text_of(&chat(&s, &body).await), "found alice's todo");
    assert_eq!(text_of(&chat(&s, &chat_body("gpt-4o", "plain")).await), "first");
    let second = json!({"model": "gpt-4o", "messages": [
        {"role": "user", "content": "a"}, {"role": "assistant", "content": "b"}, {"role": "user", "content": "c"}]});
    assert_eq!(text_of(&chat(&s, &second).await), "later");
}

#[tokio::test]
async fn title_side_rule_needs_system_and_no_tools() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"name": "title", "match": {"system": "/generate a title/i", "no_tools": true}, "reply": {"text": "A Title"}},
            {"name": "main", "reply": {"text": "main answer"}}
        ]),
    )
    .await;
    let mut with_tools = json!({"model": "gpt-4o", "messages": [
        {"role": "system", "content": "Generate a title for this chat"}, {"role": "user", "content": "x"}]});
    assert_eq!(text_of(&chat(&s, &with_tools).await), "A Title");
    with_tools["tools"] = json!([{"type": "function", "function": {"name": "t", "parameters": {"type": "object"}}}]);
    assert_eq!(text_of(&chat(&s, &with_tools).await), "main answer");
    let no_system = chat_body("gpt-4o", "Generate a title");
    assert_eq!(text_of(&chat(&s, &no_system).await), "main answer");
}

#[tokio::test]
async fn cut_after_chunks_ends_without_finish_or_done() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"reply": {"text": "one two three four five six", "stream": {"chunk_tokens": 1, "cut_after_chunks": 2}}}]),
    )
    .await;
    let mut body = chat_body("gpt-4o", "go");
    body["stream"] = json!(true);
    let mut sse = open_stream(&s, &body).await;
    assert_eq!(sse.status, 200);
    let frames = sse.all().await;
    assert!(!frames.iter().any(|f| f == "[DONE]"), "{frames:?}");
    let chunks = json_chunks(&frames);
    assert!(chunks.iter().all(|c| c["choices"][0]["finish_reason"].is_null()), "{chunks:?}");
    let contents = chunks
        .iter()
        .filter(|c| c["choices"][0]["delta"]["content"].as_str().is_some_and(|t| !t.is_empty()))
        .count();
    assert_eq!(contents, 2);
}

#[tokio::test]
async fn n_greater_than_one_streams_all_choices() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "alpha beta gamma"}}])).await;
    let mut body = chat_body("gpt-4o", "go");
    body["stream"] = json!(true);
    body["n"] = json!(2);
    let mut sse = open_stream(&s, &body).await;
    let frames = sse.all().await;
    assert_eq!(frames.last().unwrap(), "[DONE]");
    let chunks = json_chunks(&frames);
    for i in 0..2 {
        let mine: Vec<&Value> = chunks.iter().filter(|c| c["choices"][0]["index"] == i).collect();
        let text: String = mine.iter().filter_map(|c| c["choices"][0]["delta"]["content"].as_str()).collect();
        assert_eq!(text, "alpha beta gamma", "choice {i}");
        let finishes = mine.iter().filter(|c| c["choices"][0]["finish_reason"].is_string()).count();
        assert_eq!(finishes, 1, "choice {i}");
        assert_eq!(mine.iter().filter(|c| c["choices"][0]["delta"]["role"].is_string()).count(), 1);
    }
}

async fn requests(s: &fakellm::Server, query: &str) -> Value {
    let r = get(s, &format!("/_fakellm/requests?{query}")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()
}

fn seqs(v: &Value) -> Vec<u64> {
    v["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["seq"].as_u64().unwrap())
        .collect()
}

async fn put_log_config(s: &fakellm::Server, body: Value) -> Reply {
    send(
        s,
        "PUT",
        "/_fakellm/requests/config",
        &[("content-type", "application/json")],
        &body.to_string(),
    )
    .await
}

#[tokio::test]
async fn request_log_capacity_shrink_drops_oldest() {
    let s = start().await;
    for i in 0..5 {
        chat(&s, &chat_body("gpt-4o", &format!("m{i}"))).await;
    }
    assert_eq!(seqs(&requests(&s, "").await).len(), 5);
    let r = put_log_config(&s, json!({"capacity": 2})).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["dropped"], 3);
    let v = requests(&s, "").await;
    assert_eq!(v["dropped"], 3);
    let kept = seqs(&v);
    assert_eq!(kept.len(), 2);
    assert!(kept[0] < kept[1]);
    assert_eq!(get(&s, "/_fakellm/requests/config").await.json()["capacity"], 2);
}

#[tokio::test]
async fn request_log_disable_enable_keeps_counting() {
    let s = start().await;
    chat(&s, &chat_body("gpt-4o", "a")).await;
    let first = seqs(&requests(&s, "").await);
    assert_eq!(put_log_config(&s, json!({"enabled": false})).await.status, 200);
    chat(&s, &chat_body("gpt-4o", "b")).await;
    assert_eq!(seqs(&requests(&s, "").await), first);
    assert_eq!(put_log_config(&s, json!({"enabled": true})).await.status, 200);
    chat(&s, &chat_body("gpt-4o", "c")).await;
    let all = seqs(&requests(&s, "").await);
    assert_eq!(all.len(), 2);
    assert!(all[1] > first[0], "{all:?}");
}

#[tokio::test]
async fn request_log_since_after_limit_model_combine() {
    let s = start().await;
    for (m, t) in [
        ("gpt-4o", "1"),
        ("gpt-4o-mini", "2"),
        ("gpt-4o", "3"),
        ("gpt-4o", "4"),
        ("gpt-4o", "5"),
    ] {
        chat(&s, &chat_body(m, t)).await;
    }
    let all = seqs(&requests(&s, "").await);
    assert_eq!(all.len(), 5);
    let after = all[0];
    let v = requests(&s, &format!("after={after}&model=gpt-4o&limit=2")).await;
    let got = seqs(&v);
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|q| *q > after));
    assert!(!got.contains(&all[1]), "mini request must be filtered out");
    assert_eq!(
        seqs(&requests(&s, &format!("after={after}&model=gpt-4o")).await),
        vec![all[2], all[3], all[4]]
    );
    // `limit` keeps the newest matches.
    assert_eq!(got, vec![all[3], all[4]]);
    assert_eq!(
        seqs(&requests(&s, "since=2000-01-01T00:00:00Z&after=0&model=gpt-4o-mini").await),
        vec![all[1]]
    );
    assert!(seqs(&requests(&s, "since=2999-01-01T00:00:00Z").await).is_empty());
}

#[tokio::test]
async fn scripts_keep_hits() {
    let s = start().await;
    let rules = json!([{"name": "r", "match": {"last_user": "ping"}, "reply": {"text": "pong"}}]);
    put_rules(&s, rules.clone()).await;
    chat(&s, &chat_body("gpt-4o", "ping")).await;
    chat(&s, &chat_body("gpt-4o", "ping")).await;
    let hits = |v: Value| v["rules"][0]["hits"].as_u64().unwrap();
    let put = |q: &'static str| {
        let s = &s;
        let rules = rules.clone();
        async move {
            let r = send(
                s,
                "PUT",
                &format!("/_fakellm/scripts{q}"),
                &[("content-type", "application/json")],
                &rules.to_string(),
            )
            .await;
            assert_eq!(r.status, 200, "{}", r.body);
        }
    };
    put("?keep_hits=true").await;
    assert_eq!(hits(get(&s, "/_fakellm/scripts").await.json()), 2);
    put("").await;
    assert_eq!(hits(get(&s, "/_fakellm/scripts").await.json()), 0);
}
