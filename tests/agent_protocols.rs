//! Trajectories on every wire format: the loop, divergence checks, tool mapping, fault steps, parallel
//! conversations and streaming, each run on Chat Completions, Responses and Anthropic Messages.

use serde_json::{Value, json};

mod common;
use common::*;
#[path = "common/protocols.rs"]
mod protocols;
use protocols::*;

async fn put_traj(s: &fakellm::Server, body: Value) {
    let r = send(
        s,
        "PUT",
        "/_fakellm/trajectories",
        &[("content-type", "application/json")],
        &body.to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
}

async fn attach(s: &fakellm::Server, name: &str) {
    put_rules(s, json!([{"reply": {"trajectory": name}}])).await;
}

fn calls_step(calls: Vec<Value>) -> Value {
    json!({"reply": {"tool_calls": calls}})
}

fn fix() -> Value {
    json!({"name": "fix", "steps": [
        calls_step(vec![json!({"name": "read_file", "arguments": {"path": "a.rs"}})]),
        calls_step(vec![json!({"name": "edit_file", "arguments": {"path": "a.rs", "text": "x"}})]),
        {"reply": {"text": "All fixed."}}
    ]})
}

#[tokio::test]
async fn full_loop_on_every_protocol() {
    for p in ALL {
        let s = start().await;
        put_traj(&s, fix()).await;
        attach(&s, "fix").await;
        let offered = ["read_file", "edit_file"];

        let r = p.send(&s, "fix it", &[], &offered).await;
        let c = p.calls(&r);
        assert_eq!(c.len(), 1, "{p:?}");
        assert_eq!((c[0].0.as_str(), c[0].1.clone()), ("read_file", json!({"path": "a.rs"})), "{p:?}");
        let t1 = call_turn(&c[0].2, "read_file", json!({"path": "a.rs"}), "fn main() {}");

        let r = p.send(&s, "fix it", std::slice::from_ref(&t1), &offered).await;
        let c = p.calls(&r);
        assert_eq!(c[0].0, "edit_file", "{p:?}");
        assert_eq!(c[0].1, json!({"path": "a.rs", "text": "x"}), "{p:?}");
        let t2 = call_turn(&c[0].2, "edit_file", c[0].1.clone(), "ok");

        let r = p.send(&s, "fix it", &[t1, t2], &offered).await;
        assert_eq!(p.text(&r), "All fixed.", "{p:?}");
        assert!(p.calls(&r).is_empty(), "{p:?}");

        let list = get(&s, "/_fakellm/trajectories").await.json();
        assert_eq!(list["trajectories"][0]["hits"], 3, "{p:?}");
        assert_eq!(list["trajectories"][0]["last_step"], 2, "{p:?}");

        // Past the end: strict says so, with the protocol's own error envelope.
        let past = [
            call_turn("c1", "read_file", json!({}), "r"),
            call_turn("c2", "edit_file", json!({}), "r"),
            call_turn("c3", "read_file", json!({}), "r"),
        ];
        let r = p.send(&s, "fix it", &past, &offered).await;
        assert_eq!(r.status, 400, "{p:?}: {}", r.body);
        assert!(p.error_message(&r).contains("exhausted"), "{p:?}: {}", r.body);
    }
}

fn expecting(expect: Value, mode: &str) -> Value {
    json!({"name": "e", "on_divergence": mode, "stop_text": "halted", "steps": [
        calls_step(vec![json!({"name": "read_file", "arguments": {"path": "a.rs"}, "id": "c1"})]),
        {"reply": {"text": "second"}, "expect": expect}
    ]})
}

#[tokio::test]
async fn expect_and_divergence_on_every_protocol() {
    for p in ALL {
        let s = start().await;
        attach(&s, "e").await;
        let second = |result: &'static str| {
            let s = &s;
            async move {
                p.send(
                    s,
                    "task",
                    &[call_turn("c1", "read_file", json!({"path": "a.rs"}), result)],
                    &["read_file"],
                )
                .await
            }
        };

        put_traj(
            &s,
            expecting(json!({"tool_results": [{"name": "read_file", "contains": "^fn m"}]}), "strict"),
        )
        .await;
        assert_eq!(p.text(&second("fn main").await), "second", "{p:?}");

        put_traj(&s, expecting(json!({"tool_results": [{"id": "c1", "equals": "exact"}]}), "strict")).await;
        assert_eq!(p.text(&second("exact").await), "second", "{p:?}");
        let r = second("exact!").await;
        assert_eq!(r.status, 400, "{p:?}: {}", r.body);
        let msg = p.error_message(&r);
        assert!(
            msg.contains("diverged") && msg.contains("expected") && msg.contains("exact!"),
            "{p:?}: {msg}"
        );

        put_traj(&s, expecting(json!({"tool_results": [{"contains": "needle"}]}), "lenient")).await;
        assert_eq!(p.text(&second("hay").await), "second", "{p:?}");

        put_traj(&s, expecting(json!({"tool_results": [{"contains": "needle"}]}), "stop")).await;
        let r = second("hay").await;
        assert_eq!(p.text(&r), "halted", "{p:?}");
    }
}

#[tokio::test]
async fn is_error_expectation_reads_anthropic_tool_results() {
    // Only Anthropic marks results as errors; the other protocols never do.
    let s = start().await;
    attach(&s, "e").await;
    put_traj(
        &s,
        expecting(json!({"tool_results": [{"name": "read_file", "is_error": true}]}), "strict"),
    )
    .await;
    for p in [P::Chat, P::Responses] {
        let r = p
            .send(&s, "task", &[call_turn("c1", "read_file", json!({}), "boom")], &["read_file"])
            .await;
        assert_eq!(r.status, 400, "{p:?}: {}", r.body);
    }
    let mut body = P::Anthropic.body("task", &[call_turn("c1", "read_file", json!({}), "boom")], &["read_file"]);
    body["messages"][2]["content"][0]["is_error"] = json!(true);
    let r = post_json_with(&s, "/v1/messages", P::Anthropic.headers(), &body).await;
    assert_eq!(P::Anthropic.text(&r), "second");
}

fn mapped() -> Value {
    json!({"name": "m", "tool_map": {
        "bash": {"name": "run_shell", "args": {"cmd": "$command", "dir": "${cwd}/src", "lit": 7}},
        "view": {"name": "Read", "args": {"file_path": "$path"}, "keep_rest": true}
    }, "steps": [calls_step(vec![
        json!({"name": "bash", "arguments": {"command": "ls -la", "cwd": "/repo"}}),
        json!({"name": "view", "arguments": {"path": "/a", "offset": 10}})
    ])]})
}

#[tokio::test]
async fn tool_map_and_unknown_tool_on_every_protocol() {
    for p in ALL {
        let s = start().await;
        put_traj(&s, mapped()).await;
        attach(&s, "m").await;

        let r = p.send(&s, "go", &[], &["run_shell", "Read"]).await;
        let c = p.calls(&r);
        assert_eq!(c.len(), 2, "{p:?}");
        assert_eq!(c[0].0, "run_shell", "{p:?}");
        assert_eq!(c[0].1, json!({"cmd": "ls -la", "dir": "/repo/src", "lit": 7}), "{p:?}");
        assert_eq!(c[1].0, "Read", "{p:?}");
        assert_eq!(c[1].1, json!({"file_path": "/a", "offset": 10}), "{p:?}");

        // The client offers no `Read`: unknown_tool defaults to an error naming what it did offer.
        let r = p.send(&s, "go", &[], &["run_shell"]).await;
        assert_eq!(r.status, 400, "{p:?}: {}", r.body);
        let msg = p.error_message(&r);
        assert!(msg.contains("Read") && msg.contains("run_shell"), "{p:?}: {msg}");

        let mut t = mapped();
        t["unknown_tool"] = json!("stop");
        t["stop_text"] = json!("tool missing");
        put_traj(&s, t).await;
        assert_eq!(p.text(&p.send(&s, "go", &[], &["run_shell"]).await), "tool missing", "{p:?}");

        let mut t = mapped();
        t["unknown_tool"] = json!("pass");
        put_traj(&s, t).await;
        assert_eq!(p.calls(&p.send(&s, "go", &[], &["run_shell"]).await).len(), 2, "{p:?}");
    }
}

#[tokio::test]
async fn fault_step_then_recovery_on_every_protocol() {
    for p in ALL {
        let s = start().await;
        attach(&s, "f").await;
        put_traj(
            &s,
            json!({"name": "f", "steps": [
                {"reply": {"error": {"status": 429, "message": "slow down", "retry_after": 1}}},
                {"reply": {"text": "recovered"}}]}),
        )
        .await;
        let r = p.send(&s, "go", &[], &[]).await;
        assert_eq!(r.status, 429, "{p:?}: {}", r.body);
        assert_eq!(r.header("retry-after"), Some("1"), "{p:?}");
        assert!(p.error_message(&r).contains("slow down"), "{p:?}: {}", r.body);

        // A client that keeps the failed turn in its history moves to the next step.
        let r = p.send(&s, "go", &[Turn::Say("partial".into())], &[]).await;
        assert_eq!(p.text(&r), "recovered", "{p:?}");
    }
}

#[tokio::test]
async fn parallel_conversations_and_retries_on_every_protocol() {
    for p in ALL {
        let s = start().await;
        put_traj(&s, fix()).await;
        attach(&s, "fix").await;
        let offered = ["read_file", "edit_file"];
        let t1 = call_turn("a1", "read_file", json!({"path": "a.rs"}), "r");

        // Two conversations at different depths, interleaved, and the same request twice.
        let deep = p.send(&s, "one", std::slice::from_ref(&t1), &offered).await;
        let shallow = p.send(&s, "two", &[], &offered).await;
        let deep_again = p.send(&s, "one", std::slice::from_ref(&t1), &offered).await;
        assert_eq!(p.calls(&deep)[0].0, "edit_file", "{p:?}");
        assert_eq!(p.calls(&shallow)[0].0, "read_file", "{p:?}");
        assert_eq!(p.calls(&deep_again)[0].0, "edit_file", "a retry gets the same step: {p:?}");
    }
}

#[tokio::test]
async fn check_pairing_on_every_protocol() {
    for p in ALL {
        let s = start().await;
        attach(&s, "pair").await;
        put_traj(
            &s,
            json!({"name": "pair", "check_pairing": true, "steps": [
                calls_step(vec![json!({"name": "read_file", "arguments": {}, "id": "c1"})]),
                {"reply": {"text": "ok"}}]}),
        )
        .await;
        let ok = p
            .send(&s, "go", &[call_turn("c1", "read_file", json!({}), "r")], &["read_file"])
            .await;
        assert_eq!(p.text(&ok), "ok", "{p:?}");

        // Drop the result: the call is orphaned.
        let mut body = p.body("go", &[call_turn("c1", "read_file", json!({}), "r")], &["read_file"]);
        match p {
            P::Chat => drop(body["messages"].as_array_mut().unwrap().pop()),
            P::Responses => drop(body["input"].as_array_mut().unwrap().pop()),
            P::Anthropic => {
                body["messages"].as_array_mut().unwrap().pop();
                body["messages"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"role": "user", "content": "continue"}));
            }
        }
        let r = post_json_with(&s, p.path(), p.headers(), &body).await;
        assert_eq!(r.status, 400, "{p:?}: {}", r.body);
        assert!(p.error_message(&r).contains("no result"), "{p:?}: {}", r.body);
    }
}

#[tokio::test]
async fn streamed_trajectory_steps_on_every_protocol() {
    for p in ALL {
        let s = start().await;
        put_traj(&s, fix()).await;
        attach(&s, "fix").await;
        let mut body = p.body("fix it", &[], &["read_file", "edit_file"]);
        body["stream"] = json!(true);
        let mut sse = open_sse(&s, p.path(), p.headers(), &body).await;
        assert_eq!(sse.status, 200, "{p:?}");
        let frames = sse.all().await.join("\n");
        assert!(frames.contains("read_file"), "{p:?}: {frames}");
        assert!(frames.contains("a.rs"), "{p:?}: {frames}");
    }
}

#[tokio::test]
async fn request_log_names_the_trajectory_on_every_protocol() {
    for p in ALL {
        let s = start().await;
        put_traj(&s, fix()).await;
        put_rules(&s, json!([{"name": "agent", "reply": {"trajectory": "fix"}}])).await;
        p.send(&s, "go", &[], &["read_file"]).await;
        p.send(
            &s,
            "go",
            &[call_turn("c1", "read_file", json!({}), "r")],
            &["read_file", "edit_file"],
        )
        .await;
        let reqs = log(&s, "source=script").await;
        assert_eq!(reqs.len(), 2, "{p:?}");
        assert_eq!(reqs[1]["protocol"], p.log_name(), "{p:?}");
        assert_eq!(reqs[1]["source"]["kind"], "trajectory", "{p:?}");
        assert_eq!(reqs[1]["source"]["step"], 1, "{p:?}");
        assert_eq!(reqs[1]["source"]["name"], "agent", "{p:?}");
    }
}

#[tokio::test]
async fn by_hits_cursor_on_every_protocol() {
    for p in ALL {
        let s = start().await;
        attach(&s, "h").await;
        put_traj(
            &s,
            json!({"name": "h", "cursor": "by_hits", "steps": [
                {"reply": {"text": "one"}}, {"reply": {"text": "two"}}]}),
        )
        .await;
        // No history to look at: each request advances the counter.
        assert_eq!(p.text(&p.send(&s, "a", &[], &[]).await), "one", "{p:?}");
        assert_eq!(p.text(&p.send(&s, "b", &[], &[]).await), "two", "{p:?}");
        assert_eq!(p.send(&s, "c", &[], &[]).await.status, 400, "{p:?}");
    }
}
