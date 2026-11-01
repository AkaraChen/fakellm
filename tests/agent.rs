//! Fake agent over HTTP: trajectories are registered through the control plane and driven by hand-built conversations
//! on all three protocols, the way a real agent loop replays history with tool results appended.

use serde_json::{Value, json};

mod common;
use common::*;

async fn put_traj(s: &fakellm::Server, ctype: &str, body: &str) -> Reply {
    send(s, "PUT", "/_fakellm/trajectories", &[("content-type", ctype)], body).await
}

async fn put_json(s: &fakellm::Server, body: Value) {
    let r = put_traj(s, "application/json", &body.to_string()).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

async fn attach(s: &fakellm::Server, name: &str) {
    put_rules(s, json!([{"reply": {"trajectory": name}}])).await;
}

fn tools(names: &[&str]) -> Value {
    Value::Array(
        names
            .iter()
            .map(|n| json!({"type": "function", "function": {"name": n, "parameters": {"type": "object"}}}))
            .collect(),
    )
}

fn call(name: &str, args: Value) -> Value {
    json!({"name": name, "arguments": args})
}

fn calls_step(calls: Vec<Value>) -> Value {
    json!({"reply": {"tool_calls": calls}})
}

fn text_step(t: &str) -> Value {
    json!({"reply": {"text": t}})
}

/// A Chat conversation: user prompt, then each (assistant call, tool result) pair already played.
fn chat_history(prompt: &str, played: &[(&str, &str, Value, &str)], offered: &[&str]) -> Value {
    let mut messages = vec![json!({"role": "user", "content": prompt})];
    for (id, name, args, result) in played {
        messages.push(json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": id, "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}));
        messages.push(json!({"role": "tool", "tool_call_id": id, "content": result}));
    }
    json!({"model": "gpt-4o", "messages": messages, "tools": tools(offered)})
}

fn first_call(r: &Reply) -> (String, Value, String) {
    let v = r.json();
    let c = &v["choices"][0]["message"]["tool_calls"][0];
    (
        c["function"]["name"].as_str().unwrap().to_owned(),
        serde_json::from_str(c["function"]["arguments"].as_str().unwrap()).unwrap(),
        c["id"].as_str().unwrap().to_owned(),
    )
}

fn content(r: &Reply) -> String {
    r.json()["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_owned()
}

fn err_code(r: &Reply) -> String {
    r.json()["error"]["code"].as_str().unwrap_or_default().to_owned()
}

fn tool_text() -> Value {
    json!({
        "name": "fix",
        "steps": [
            calls_step(vec![call("read_file", json!({"path": "a.rs"}))]),
            calls_step(vec![call("edit_file", json!({"path": "a.rs", "text": "x"}))]),
            text_step("All fixed.")
        ]
    })
}

#[tokio::test]
async fn two_step_loop_over_chat() {
    let s = start().await;
    put_json(&s, tool_text()).await;
    attach(&s, "fix").await;
    let offered = ["read_file", "edit_file"];

    let r = chat(&s, &chat_history("fix it", &[], &offered)).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let (name, args, id1) = first_call(&r);
    assert_eq!((name.as_str(), args), ("read_file", json!({"path": "a.rs"})));
    assert_eq!(r.json()["choices"][0]["finish_reason"], "tool_calls");

    let r = chat(
        &s,
        &chat_history("fix it", &[(&id1, "read_file", json!({"path": "a.rs"}), "fn main() {}")], &offered),
    )
    .await;
    let (name, _, id2) = first_call(&r);
    assert_eq!(name, "edit_file");

    let played = [
        (id1.as_str(), "read_file", json!({"path": "a.rs"}), "fn main() {}"),
        (id2.as_str(), "edit_file", json!({"path": "a.rs", "text": "x"}), "ok"),
    ];
    let r = chat(&s, &chat_history("fix it", &played, &offered)).await;
    assert_eq!(content(&r), "All fixed.");
    assert_eq!(r.json()["choices"][0]["finish_reason"], "stop");

    let list = get(&s, "/_fakellm/trajectories").await.json();
    assert_eq!(list["trajectories"][0]["name"], "fix");
    assert_eq!(list["trajectories"][0]["steps"], 3);
    assert_eq!(list["trajectories"][0]["hits"], 3);
}

#[tokio::test]
async fn same_trajectory_over_responses_and_anthropic() {
    let s = start().await;
    put_json(&s, tool_text()).await;
    attach(&s, "fix").await;

    let resp_tools = json!([{"type": "function", "name": "read_file", "parameters": {"type": "object"}},
                            {"type": "function", "name": "edit_file", "parameters": {"type": "object"}}]);
    let r = post_json(
        &s,
        "/v1/responses",
        &json!({"model": "gpt-4o", "input": "fix it", "tools": resp_tools}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let item = r.json()["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["type"] == "function_call")
        .cloned()
        .unwrap();
    assert_eq!(item["name"], "read_file");
    let call_id = item["call_id"].as_str().unwrap().to_owned();
    let input = json!([
        {"role": "user", "content": "fix it"},
        {"type": "function_call", "call_id": call_id, "name": "read_file", "arguments": "{\"path\":\"a.rs\"}"},
        {"type": "function_call_output", "call_id": call_id, "output": "fn main() {}"}
    ]);
    let r = post_json(
        &s,
        "/v1/responses",
        &json!({"model": "gpt-4o", "input": input, "tools": resp_tools}),
    )
    .await;
    let out = r.json()["output"].clone();
    assert!(
        out.as_array()
            .unwrap()
            .iter()
            .any(|o| o["type"] == "function_call" && o["name"] == "edit_file"),
        "{out}"
    );

    let a_tools =
        json!([{"name": "read_file", "input_schema": {"type": "object"}}, {"name": "edit_file", "input_schema": {"type": "object"}}]);
    let h = [("anthropic-version", "2023-06-01")];
    let r = post_json_with(
        &s,
        "/v1/messages",
        &h,
        &json!({"model": "claude-sonnet-4", "max_tokens": 64, "tools": a_tools,
        "messages": [{"role": "user", "content": "fix it"}]}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let block = r.json()["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .cloned()
        .unwrap();
    assert_eq!(block["name"], "read_file");
    assert_eq!(block["input"], json!({"path": "a.rs"}));
    let id = block["id"].as_str().unwrap();
    let messages = json!([
        {"role": "user", "content": "fix it"},
        {"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": "read_file", "input": {"path": "a.rs"}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": "fn main() {}"}]}
    ]);
    let r = post_json_with(
        &s,
        "/v1/messages",
        &h,
        &json!({"model": "claude-sonnet-4", "max_tokens": 64, "tools": a_tools, "messages": messages}),
    )
    .await;
    let block = r.json()["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .cloned()
        .unwrap();
    assert_eq!(block["name"], "edit_file");
}

#[tokio::test]
async fn parallel_conversations_and_retries_do_not_interfere() {
    let s = start().await;
    put_json(&s, tool_text()).await;
    attach(&s, "fix").await;
    let offered = ["read_file", "edit_file"];
    let played = [("call_a", "read_file", json!({"path": "a.rs"}), "src")];

    let fresh_b = chat(&s, &chat_history("other task", &[], &offered)).await;
    let advanced_a = chat(&s, &chat_history("fix it", &played, &offered)).await;
    let retry_a = chat(&s, &chat_history("fix it", &played, &offered)).await;
    let fresh_c = chat(&s, &chat_history("third", &[], &offered)).await;
    assert_eq!(first_call(&fresh_b).0, "read_file");
    assert_eq!(first_call(&fresh_c).0, "read_file");
    assert_eq!(first_call(&advanced_a).0, "edit_file");
    assert_eq!(first_call(&retry_a).0, "edit_file");
}

#[tokio::test]
async fn by_hits_cursor_counts_requests() {
    let s = start().await;
    let mut t = tool_text();
    t["cursor"] = json!("by_hits");
    put_json(&s, t).await;
    attach(&s, "fix").await;
    let body = chat_history("same history every time", &[], &["read_file", "edit_file"]);
    assert_eq!(first_call(&chat(&s, &body).await).0, "read_file");
    assert_eq!(first_call(&chat(&s, &body).await).0, "edit_file");
    assert_eq!(content(&chat(&s, &body).await), "All fixed.");
}

#[tokio::test]
async fn registration_formats() {
    let s = start().await;
    let one = json!({"name": "a", "steps": [text_step("A")]});
    let two = json!({"name": "b", "steps": [text_step("B")]});

    put_json(&s, one.clone()).await;
    put_json(&s, json!([one.clone(), two.clone()])).await;
    put_json(&s, json!({"trajectories": [one.clone(), two.clone()]})).await;
    assert_eq!(
        get(&s, "/_fakellm/trajectories").await.json()["trajectories"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let yaml = "name: y\nsteps:\n  - reply:\n      text: from yaml\n";
    let r = put_traj(&s, "application/yaml", yaml).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json(), json!({"added": 1, "count": 1}));

    let jsonl = "{\"kind\":\"trajectory\",\"name\":\"j\"}\n{\"reply\":{\"text\":\"one\"}}\n\n{\"reply\":{\"text\":\"two\"}}\n";
    let r = put_traj(&s, "application/x-ndjson", jsonl).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let j = get(&s, "/_fakellm/trajectories/j").await.json();
    assert_eq!(j["steps"].as_array().unwrap().len(), 2);

    let multi = "{\"kind\":\"trajectory\",\"name\":\"m1\"}\n{\"reply\":{\"text\":\"x\"}}\n{\"kind\":\"trajectory\",\"name\":\"m2\"}\n{\"reply\":{\"text\":\"y\"}}\n{\"reply\":{\"text\":\"z\"}}\n";
    let r = put_traj(&s, "application/x-ndjson", multi).await;
    assert_eq!(r.json(), json!({"added": 2, "count": 2}));
    let names: Vec<_> = get(&s, "/_fakellm/trajectories").await.json()["trajectories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["m1", "m2"]);

    let whole = "{\"name\":\"w1\",\"steps\":[{\"reply\":{\"text\":\"x\"}}]}\n{\"name\":\"w2\",\"steps\":[{\"reply\":{\"text\":\"y\"}}]}\n";
    assert_eq!(put_traj(&s, "application/x-ndjson", whole).await.json()["count"], 2);
    assert_eq!(put_traj(&s, "text/plain", jsonl).await.json()["count"], 1, "Auto detects JSONL");
}

#[tokio::test]
async fn export_round_trips_json_and_jsonl() {
    let s = start().await;
    let mut t = tool_text();
    t["on_divergence"] = json!("lenient");
    t["tool_map"] = json!({"read_file": {"name": "Read", "args": {"file_path": "$path"}}});
    t["meta"] = json!({"source": "test"});
    put_json(&s, t).await;
    let exported = get(&s, "/_fakellm/trajectories/fix").await;
    assert_eq!(exported.status, 200);
    let doc = exported.json();
    assert_eq!(doc["kind"], "trajectory");
    assert_eq!(doc["on_divergence"], "lenient");
    assert_eq!(doc["cursor"], "by_turn");
    assert_eq!(doc["unknown_tool"], "error");

    let jsonl = get(&s, "/_fakellm/trajectories/fix?format=jsonl").await;
    assert!(jsonl.header("content-type").unwrap().contains("ndjson"));
    assert_eq!(jsonl.body.lines().count(), 4);

    let r = send(&s, "DELETE", "/_fakellm/trajectories", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(put_traj(&s, "application/x-ndjson", &jsonl.body).await.status, 200);
    assert_eq!(get(&s, "/_fakellm/trajectories/fix").await.json(), doc);
    put_json(&s, doc.clone()).await;
    assert_eq!(get(&s, "/_fakellm/trajectories/fix").await.json(), doc);
}

#[tokio::test]
async fn crud_and_registration_errors() {
    let s = start().await;
    put_json(
        &s,
        json!([{"name": "a", "steps": [text_step("A")]}, {"name": "b", "steps": [text_step("B")]}]),
    )
    .await;

    let r = post_json(&s, "/_fakellm/trajectories", &json!({"name": "a", "steps": [text_step("A2")]})).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["count"], 2, "same name replaced in place");
    let r = post_json(&s, "/_fakellm/trajectories", &json!({"name": "c", "steps": [text_step("C")]})).await;
    assert_eq!(r.json()["count"], 3);

    assert_eq!(get(&s, "/_fakellm/trajectories/nope").await.status, 404);
    assert_eq!(send(&s, "DELETE", "/_fakellm/trajectories/nope", &[], "").await.status, 404);
    let r = send(&s, "DELETE", "/_fakellm/trajectories/b", &[], "").await;
    assert_eq!((r.status, r.json()), (200, json!({"deleted": true})));
    assert_eq!(get(&s, "/_fakellm").await.json()["trajectories"]["count"], 2);

    let bad = [
        json!({"name": "", "steps": [text_step("x")]}),
        json!({"name": "a/b", "steps": [text_step("x")]}),
        json!({"name": "e", "steps": []}),
        json!({"name": "dup", "steps": [text_step("x")]}),
        json!({"name": "r", "steps": [{"reply": {"text": "x"}, "expect": {"any_user": "("}}]}),
        json!({"name": "p", "steps": [{"reply": {"trajectory": "other"}}]}),
        json!({"name": "u", "bogus": 1, "steps": [text_step("x")]}),
    ];
    for b in &bad[..1] {
        assert_eq!(post_json(&s, "/_fakellm/trajectories", b).await.status, 400, "{b}");
    }
    for b in &bad[1..] {
        let r = if b["name"] == "dup" {
            put_traj(&s, "application/json", &json!([b, b]).to_string()).await
        } else {
            post_json(&s, "/_fakellm/trajectories", b).await
        };
        assert_eq!(r.status, 400, "{b}: {}", r.body);
    }
    let r = put_traj(
        &s,
        "application/json",
        &json!([{"name": "ok", "steps": [text_step("x")]}, bad[2]]).to_string(),
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(
        get(&s, "/_fakellm").await.json()["trajectories"]["count"],
        2,
        "a 400 changes nothing"
    );
    assert_eq!(put_traj(&s, "application/json", "{not json").await.status, 400);
}

#[tokio::test]
async fn reset_clears_trajectories() {
    let s = start().await;
    put_json(&s, tool_text()).await;
    assert_eq!(post_json(&s, "/_fakellm/reset", &json!({})).await.status, 200);
    assert_eq!(get(&s, "/_fakellm/trajectories").await.json()["trajectories"], json!([]));
}

#[tokio::test]
async fn config_trajectories_are_registered_at_start() {
    let t = fakellm::Trajectory::parse_many(&tool_text().to_string(), fakellm::TrajectoryFormat::Json)
        .unwrap()
        .remove(0);
    let s = start_with(fakellm::Config {
        trajectories: vec![t],
        ..Default::default()
    })
    .await;
    assert_eq!(get(&s, "/_fakellm").await.json()["trajectories"]["count"], 1);
    attach(&s, "fix").await;
    assert_eq!(
        first_call(&chat(&s, &chat_history("go", &[], &["read_file", "edit_file"])).await).0,
        "read_file"
    );
}

#[tokio::test]
async fn missing_trajectory_is_a_per_request_400() {
    let s = start().await;
    attach(&s, "ghost").await;
    let r = chat(&s, &chat_body("gpt-4o", "hi")).await;
    assert_eq!(r.status, 400);
    assert_eq!(err_code(&r), "fakellm_trajectory_not_found");
    put_json(&s, json!({"name": "ghost", "steps": [text_step("boo")]})).await;
    assert_eq!(content(&chat(&s, &chat_body("gpt-4o", "hi")).await), "boo");
}

fn expecting(expect: Value, mode: &str) -> Value {
    json!({
        "name": "e",
        "on_divergence": mode,
        "stop_text": "halted",
        "steps": [
            calls_step(vec![call("read_file", json!({"path": "a.rs"}))]),
            {"reply": {"text": "second"}, "expect": expect}
        ]
    })
}

async fn second_step(s: &fakellm::Server, result: &str) -> Reply {
    chat(
        s,
        &chat_history("task", &[("call_1", "read_file", json!({"path": "a.rs"}), result)], &["read_file"]),
    )
    .await
}

#[tokio::test]
async fn expect_checks_tool_results() {
    let s = start().await;
    attach(&s, "e").await;
    let cases: [(Value, &str, bool); 7] = [
        (
            json!({"tool_results": [{"name": "read_file", "contains": "^fn m"}]}),
            "fn main",
            true,
        ),
        (json!({"tool_results": [{"name": "read_file", "equals": "exact"}]}), "exact", true),
        (json!({"tool_results": [{"name": "read_file", "equals": "exact"}]}), "exact!", false),
        (json!({"tool_results": [{"id": "call_1"}]}), "x", true),
        (json!({"tool_results": [{"id": "call_9"}]}), "x", false),
        (json!({"tool_results": [{"contains": "needle"}]}), "haystack", false),
        (json!({"any_user": "/TASK/i"}), "x", true),
    ];
    for (expect, result, ok) in cases {
        put_json(&s, expecting(expect.clone(), "strict")).await;
        let r = second_step(&s, result).await;
        if ok {
            assert_eq!(content(&r), "second", "{expect} vs {result}: {}", r.body);
        } else {
            assert_eq!(r.status, 400, "{expect} vs {result}");
            assert_eq!(err_code(&r), "fakellm_trajectory_diverged");
        }
    }
}

#[tokio::test]
async fn strict_divergence_message_lists_expected_and_got() {
    let s = start().await;
    attach(&s, "e").await;
    put_json(
        &s,
        expecting(json!({"tool_results": [{"name": "read_file", "contains": "needle"}]}), "strict"),
    )
    .await;
    let r = second_step(&s, "just hay").await;
    assert_eq!(r.status, 400);
    let e = r.json()["error"].clone();
    assert_eq!(e["type"], "fakellm_error");
    let msg = e["message"].as_str().unwrap();
    assert!(msg.contains("expected") && msg.contains("got"), "{msg}");
    assert!(msg.contains("read_file") && msg.contains("just hay"), "{msg}");
    assert!(msg.contains('e'), "{msg}");
}

#[tokio::test]
async fn lenient_ignores_and_stop_halts() {
    let s = start().await;
    attach(&s, "e").await;
    let expect = json!({"tool_results": [{"contains": "needle"}]});
    put_json(&s, expecting(expect.clone(), "lenient")).await;
    assert_eq!(content(&second_step(&s, "hay").await), "second");
    put_json(&s, expecting(expect, "stop")).await;
    let r = second_step(&s, "hay").await;
    assert_eq!(r.status, 200);
    assert_eq!(content(&r), "halted");
    assert_eq!(r.json()["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn tool_error_expectation() {
    let s = start().await;
    attach(&s, "e").await;
    put_json(
        &s,
        expecting(json!({"tool_results": [{"name": "read_file", "is_error": true}]}), "strict"),
    )
    .await;
    assert_eq!(second_step(&s, "boom").await.status, 400, "a Chat tool message is never an error");
    let a_msgs = json!([
        {"role": "user", "content": "task"},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "read_file", "input": {}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "boom", "is_error": true}]}
    ]);
    let r = post_json_with(
        &s,
        "/v1/messages",
        &[("anthropic-version", "2023-06-01")],
        &json!({"model": "claude-sonnet-4", "max_tokens": 16, "messages": a_msgs,
                "tools": [{"name": "read_file", "input_schema": {"type": "object"}}]}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["content"][0]["text"], "second");
}

#[tokio::test]
async fn exhaustion_per_mode() {
    let s = start().await;
    attach(&s, "x").await;
    let history = |n: usize| {
        let played: Vec<_> = (0..n).map(|i| (format!("c{i}"), "read_file", json!({}), "r")).collect();
        let played: Vec<_> = played.iter().map(|(a, b, c, d)| (a.as_str(), *b, c.clone(), *d)).collect();
        chat_history("go", &played, &["read_file"])
    };
    for (mode, strict) in [("strict", true), ("lenient", false), ("stop", false)] {
        put_json(
            &s,
            json!({"name": "x", "on_divergence": mode, "stop_text": "no more", "steps": [calls_step(vec![call("read_file", json!({}))])]}),
        )
        .await;
        assert_eq!(chat(&s, &history(0)).await.status, 200);
        let r = chat(&s, &history(1)).await;
        if strict {
            assert_eq!(r.status, 400, "{mode}");
            assert_eq!(err_code(&r), "fakellm_trajectory_exhausted");
        } else {
            assert_eq!(content(&r), "no more", "{mode}");
            assert_eq!(r.json()["choices"][0]["finish_reason"], "stop");
        }
    }
}

#[tokio::test]
async fn check_pairing_flags_orphaned_calls() {
    let s = start().await;
    attach(&s, "p").await;
    put_json(
        &s,
        json!({"name": "p", "check_pairing": true, "steps": [
        calls_step(vec![call("read_file", json!({}))]), text_step("done"), text_step("again")]}),
    )
    .await;
    let mut req = chat_history("go", &[("c1", "read_file", json!({}), "r")], &["read_file"]);
    assert_eq!(content(&chat(&s, &req).await), "done");
    req["messages"].as_array_mut().unwrap().pop();
    let r = chat(&s, &req).await;
    assert_eq!(r.status, 400);
    assert_eq!(err_code(&r), "fakellm_trajectory_diverged");
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("c1"));

    put_json(
        &s,
        json!({"name": "p", "steps": [calls_step(vec![call("read_file", json!({}))]), text_step("done")]}),
    )
    .await;
    assert_eq!(content(&chat(&s, &req).await), "done", "pairing is not checked by default");
}

fn mapped() -> Value {
    json!({
        "name": "m",
        "tool_map": {
            "bash": {"name": "run_shell", "args": {
                "cmd": "$command", "dir": "${cwd}/src", "deep": "$opts.depth", "note": "price $$5", "lit": 7, "flag": true
            }},
            "view": {"name": "Read", "args": {"file_path": "$path"}, "keep_rest": true},
            "plain": {"name": "plain_renamed"}
        },
        "steps": [calls_step(vec![
            call("bash", json!({"command": "ls -la", "cwd": "/repo", "opts": {"depth": 3}, "extra": 1})),
            call("view", json!({"path": "/a", "offset": 10})),
            call("plain", json!({"x": 1})),
            call("untouched", json!({"y": 2}))
        ])]
    })
}

#[tokio::test]
async fn tool_map_rewrites_names_and_arguments() {
    let s = start().await;
    let mut t = mapped();
    t["unknown_tool"] = json!("pass");
    put_json(&s, t).await;
    attach(&s, "m").await;
    let r = chat(&s, &chat_history("go", &[], &["run_shell", "Read", "plain_renamed"])).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let calls = r.json()["choices"][0]["message"]["tool_calls"].clone();
    let got: Vec<(String, Value)> = calls
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["function"]["name"].as_str().unwrap().to_owned(),
                serde_json::from_str(c["function"]["arguments"].as_str().unwrap()).unwrap(),
            )
        })
        .collect();
    assert_eq!(got[0].0, "run_shell");
    assert_eq!(
        got[0].1,
        json!({"cmd": "ls -la", "dir": "/repo/src", "deep": 3, "note": "price $5", "lit": 7, "flag": true})
    );
    assert_eq!(got[1], ("Read".to_owned(), json!({"file_path": "/a", "offset": 10})));
    assert_eq!(got[2], ("plain_renamed".to_owned(), json!({"x": 1})));
    assert_eq!(got[3], ("untouched".to_owned(), json!({"y": 2})));
}

#[tokio::test]
async fn tool_map_string_arguments() {
    let s = start().await;
    put_json(
        &s,
        json!({"name": "m", "tool_map": {"bash": {"name": "run", "args": {"cmd": "$command"}}},
        "steps": [{"reply": {"tool_calls": [{"name": "bash", "arguments": "{\"command\":\"ls\"}"}]}}]}),
    )
    .await;
    attach(&s, "m").await;
    let (name, args, _) = first_call(&chat(&s, &chat_history("go", &[], &["run"])).await);
    assert_eq!((name.as_str(), args), ("run", json!({"cmd": "ls"})));
}

#[tokio::test]
async fn unknown_tool_modes() {
    let s = start().await;
    attach(&s, "m").await;
    let offered = ["run_shell", "Read"];

    put_json(&s, mapped()).await;
    let r = chat(&s, &chat_history("go", &[], &offered)).await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(err_code(&r), "fakellm_unknown_tool");
    let msg = r.json()["error"]["message"].as_str().unwrap().to_owned();
    assert!(
        msg.contains("plain_renamed") && msg.contains("run_shell") && msg.contains("Read"),
        "{msg}"
    );

    let mut t = mapped();
    t["unknown_tool"] = json!("stop");
    t["stop_text"] = json!("tool missing");
    put_json(&s, t).await;
    let r = chat(&s, &chat_history("go", &[], &offered)).await;
    assert_eq!(content(&r), "tool missing");
    assert_eq!(r.json()["choices"][0]["finish_reason"], "stop");

    let mut t = mapped();
    t["unknown_tool"] = json!("pass");
    put_json(&s, t).await;
    assert_eq!(chat(&s, &chat_history("go", &[], &offered)).await.status, 200);
    let no_tools = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "go"}]});
    assert_eq!(chat(&s, &no_tools).await.status, 200);
    put_json(&s, mapped()).await;
    assert_eq!(chat(&s, &no_tools).await.status, 400, "an empty tool list offers nothing");
}

#[tokio::test]
async fn error_and_hang_steps_inject_faults() {
    let s = start().await;
    attach(&s, "f").await;
    put_json(
        &s,
        json!({"name": "f", "steps": [
        {"reply": {"error": {"status": 429, "message": "slow down"}}},
        {"reply": {"text": "recovered"}}]}),
    )
    .await;
    let r = chat(&s, &chat_body("gpt-4o", "go")).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.json()["error"]["message"], "slow down");

    let mut retry = chat_history("go", &[("c1", "t", json!({}), "r")], &["t"]);
    retry["messages"].as_array_mut().unwrap().truncate(1);
    retry["messages"]
        .as_array_mut()
        .unwrap()
        .insert(1, json!({"role": "assistant", "content": "partial"}));
    assert_eq!(content(&chat(&s, &retry).await), "recovered");
}

#[tokio::test]
async fn step_and_rule_delivery_overrides() {
    let s = start().await;
    put_json(
        &s,
        json!({"name": "d", "steps": [
        {"reply": {"text": "a b c d", "usage": {"prompt_tokens": 11, "completion_tokens": 4}}},
        {"reply": {"text": "w x y z", "stream": {"chunk_tokens": 2}}}]}),
    )
    .await;
    put_rules(&s, json!([{"reply": {"trajectory": "d", "stream": {"chunk_tokens": 1}}}])).await;

    let r = chat(&s, &chat_body("gpt-4o", "go")).await;
    let u = r.json()["usage"].clone();
    assert_eq!((u["prompt_tokens"].as_u64(), u["completion_tokens"].as_u64()), (Some(11), Some(4)));

    let mut body = json!({"model": "gpt-4o", "stream": true, "messages": [
        {"role": "user", "content": "go"}, {"role": "assistant", "content": "x"}, {"role": "user", "content": "more"}]});
    let mut sse = open_stream(&s, &body).await;
    let chunks = json_chunks(&sse.all().await);
    let deltas = chunks
        .iter()
        .filter(|c| c["choices"][0]["delta"]["content"].as_str().is_some_and(|t| !t.is_empty()))
        .count();
    assert_eq!(deltas, 2, "step chunk_tokens 2 beats the rule's 1");

    body["messages"] = json!([{"role": "user", "content": "go"}]);
    let mut sse = open_stream(&s, &body).await;
    let chunks = json_chunks(&sse.all().await);
    let deltas = chunks
        .iter()
        .filter(|c| c["choices"][0]["delta"]["content"].as_str().is_some_and(|t| !t.is_empty()))
        .count();
    assert_eq!(deltas, 4, "step 0 inherits the rule's chunk_tokens 1");
}

#[tokio::test]
async fn request_log_records_trajectory_source() {
    let s = start().await;
    put_json(&s, tool_text()).await;
    put_rules(&s, json!([{"name": "agent", "reply": {"trajectory": "fix"}}])).await;
    chat(&s, &chat_history("go", &[], &["read_file", "edit_file"])).await;
    chat(
        &s,
        &chat_history("go", &[("c1", "read_file", json!({}), "r")], &["read_file", "edit_file"]),
    )
    .await;

    let log = get(&s, "/_fakellm/requests?source=script").await.json();
    let reqs = log["requests"].as_array().unwrap();
    assert_eq!(reqs.len(), 2);
    let src = &reqs[1]["source"];
    assert_eq!(src["kind"], "trajectory");
    assert_eq!(src["trajectory"], "fix");
    assert_eq!(src["step"], 1);
    assert_eq!(src["name"], "agent");
    let by_rule = get(&s, "/_fakellm/requests?rule=agent").await.json();
    assert_eq!(by_rule["requests"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn step_respects_anthropic_system_reminder_user_turns() {
    let s = start().await;
    put_json(
        &s,
        json!({"name": "c", "steps": [calls_step(vec![call("Read", json!({}))]), text_step("ok")]}),
    )
    .await;
    attach(&s, "c").await;
    let messages = json!([
        {"role": "user", "content": "task"},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {}}]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "data"},
            {"type": "text", "text": "<system-reminder>be nice</system-reminder>"}]}
    ]);
    let r = post_json_with(
        &s,
        "/v1/messages",
        &[("anthropic-version", "2023-06-01")],
        &json!({"model": "claude-sonnet-4", "max_tokens": 16, "messages": messages, "tools": [{"name": "Read", "input_schema": {"type": "object"}}]}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["content"][0]["text"], "ok");
}
