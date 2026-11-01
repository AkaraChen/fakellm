//! Policy scripts on every wire format (Chat, Responses, Anthropic Messages) and on every JavaScript runtime available:
//! the embedded `tjs` (cargo feature, on by default) and `node` and `bun` when found on PATH. Runtimes that are not
//! available are skipped with a notice. `"default"` below means no `runtime` field: whatever the server defaults to.

use std::process::Command;

use serde_json::{Value, json};

mod common;
use common::*;
#[path = "common/protocols.rs"]
mod protocols;
use protocols::*;

fn have(runtime: &str) -> bool {
    let ok = match runtime {
        "tjs" => cfg!(feature = "tjs"),
        "default" => cfg!(feature = "tjs") || Command::new("node").arg("--version").output().is_ok(),
        other => Command::new(other).arg("--version").output().is_ok(),
    };
    if !ok {
        eprintln!("skip: {runtime} not on PATH");
    }
    ok
}

const RUNTIMES: [&str; 3] = ["tjs", "node", "bun"];

async fn add_policy(s: &fakellm::Server, name: &str, source: &str, runtime: &str) {
    let mut spec = json!({"name": name, "source": source});
    if runtime != "default" {
        spec["runtime"] = json!(runtime);
    }
    let r = send(
        s,
        "PUT",
        "/_fakellm/policies",
        &[("content-type", "application/json")],
        &spec.to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{runtime}: {}", r.body);
}

async fn attach(s: &fakellm::Server, name: &str) {
    put_rules(s, json!([{"reply": {"policy": name}}])).await;
}

const AGENT: &str = r#"
const text = (m) => m.content.map((p) => p.text ?? '').join('');
export default function decide(request, context) {
  const last = request.messages.at(-1);
  const state = context.state;
  if (last.role !== 'tool') {
    return { tool_calls: [{ name: 'read_file', arguments: { path: 'a.rs' } }], state: { stage: 'read', tries: 0 } };
  }
  if (state.stage === 'read') {
    return { tool_calls: [{ name: 'edit_file', arguments: { path: 'a.rs', try: 1 } }], state: { stage: 'edit', tries: 1 } };
  }
  if (text(last).startsWith('ERROR')) {
    if (state.tries < 2) {
      const tries = state.tries + 1;
      return { tool_calls: [{ name: 'edit_file', arguments: { path: 'a.rs', try: tries } }], state: { stage: 'edit', tries } };
    }
    return { text: 'giving up', state: { stage: 'done', tries: state.tries } };
  }
  return { text: 'edited', state: { stage: 'done', tries: state.tries } };
}
"#;

const TOOLS: [&str; 2] = ["read_file", "edit_file"];

#[tokio::test]
async fn agent_loop_on_every_protocol_and_runtime() {
    for runtime in RUNTIMES {
        if !have(runtime) {
            continue;
        }
        for p in ALL {
            let s = start().await;
            add_policy(&s, "agent", AGENT, runtime).await;
            attach(&s, "agent").await;

            let c = p.calls(&p.send(&s, "fix a.rs", &[], &TOOLS).await);
            assert_eq!(
                (c[0].0.as_str(), c[0].1.clone()),
                ("read_file", json!({"path": "a.rs"})),
                "{runtime} {p:?}"
            );
            let t1 = call_turn(&c[0].2, "read_file", json!({"path": "a.rs"}), "fn main() {}");

            let c = p.calls(&p.send(&s, "fix a.rs", std::slice::from_ref(&t1), &TOOLS).await);
            assert_eq!(c[0].0, "edit_file", "{runtime} {p:?}");
            let t2 = call_turn(&c[0].2, "edit_file", c[0].1.clone(), "ERROR: no match");

            // The error result makes the policy retry once more, then give up.
            let c = p.calls(&p.send(&s, "fix a.rs", &[t1.clone(), t2.clone()], &TOOLS).await);
            assert_eq!((c[0].0.as_str(), c[0].1["try"].as_u64()), ("edit_file", Some(2)), "{runtime} {p:?}");
            let t3 = call_turn(&c[0].2, "edit_file", c[0].1.clone(), "ERROR: again");
            let r = p.send(&s, "fix a.rs", &[t1, t2, t3], &TOOLS).await;
            assert_eq!(p.text(&r), "giving up", "{runtime} {p:?}");

            let info = get(&s, "/_fakellm/policies/agent").await.json();
            assert_eq!(info["hits"], 4, "{runtime} {p:?}");
            assert_eq!(info["state"]["stage"], "done", "{runtime} {p:?}");
            assert_eq!(info["runtime"], runtime, "{p:?}");
        }
    }
}

/// What the policy is shown is the same on every protocol: roles, tool results as `tool` messages, offered tools.
#[tokio::test]
async fn the_policy_sees_one_normalized_request() {
    if !have("default") {
        return;
    }
    let src = "export default (req) => ({ text: JSON.stringify({ protocol: req.protocol, roles: req.messages.map((m) => m.role), tools: (req.tools ?? []).map((t) => t.name ?? t.function?.name), stream: !!req.stream }) });";
    for p in ALL {
        let s = start().await;
        add_policy(&s, "peek", src, "default").await;
        attach(&s, "peek").await;
        let r = p.send(&s, "go", &[call_turn("c1", "read_file", json!({}), "data")], &TOOLS).await;
        let seen: Value = serde_json::from_str(&p.text(&r)).unwrap();
        assert_eq!(seen["roles"], json!(["user", "assistant", "tool"]), "{p:?}: {seen}");
        assert_eq!(seen["tools"], json!(["read_file", "edit_file"]), "{p:?}: {seen}");
        assert_eq!(seen["protocol"], p.log_name(), "{p:?}");
    }
}

#[tokio::test]
async fn declines_fall_through_on_every_protocol() {
    if !have("default") {
        return;
    }
    for p in ALL {
        let s = start().await;
        add_policy(
            &s,
            "picky",
            "export default (r) => r.tools?.length ? { text: 'policy' } : { decline: true };",
            "default",
        )
        .await;
        put_rules(&s, json!([{"reply": {"policy": "picky"}}, {"reply": {"text": "other"}}])).await;
        assert_eq!(p.text(&p.send(&s, "hi", &[], &[]).await), "other", "{p:?}");
        assert_eq!(p.text(&p.send(&s, "hi", &[], &TOOLS).await), "policy", "{p:?}");
        assert_eq!(p.text(&p.send(&s, "hi", &[], &[]).await), "other", "{p:?}");
        assert_eq!(get(&s, "/_fakellm/policies/picky").await.json()["hits"], 1, "{p:?}");
    }
}

#[tokio::test]
async fn policy_errors_keep_the_protocol_envelope() {
    if !have("default") {
        return;
    }
    for p in ALL {
        let s = start().await;
        add_policy(&s, "p", "export default () => { throw new Error('boom from policy'); };", "default").await;
        attach(&s, "p").await;
        let r = p.send(&s, "x", &[], &[]).await;
        assert_eq!(r.status, 400, "{p:?}: {}", r.body);
        assert!(p.error_message(&r).contains("boom from policy"), "{p:?}: {}", r.body);

        add_policy(&s, "p", "export default () => ({});", "default").await;
        assert_eq!(p.send(&s, "x", &[], &[]).await.status, 400, "{p:?}");

        put_rules(&s, json!([{"reply": {"policy": "ghost"}}])).await;
        let r = p.send(&s, "x", &[], &[]).await;
        assert_eq!(r.status, 400, "{p:?}");
        assert!(r.body.contains("policy"), "{p:?}: {}", r.body);
    }
}

#[tokio::test]
async fn policy_error_replies_and_delivery_defaults_on_every_protocol() {
    if !have("default") {
        return;
    }
    for p in ALL {
        let s = start().await;
        add_policy(
            &s,
            "p",
            "export default (r) => r.messages.at(-1).role === 'tool' ? { error: { status: 529, message: 'busy' } } : { text: 'alpha beta gamma delta' };",
            "default",
        )
        .await;
        put_rules(
            &s,
            json!([{"reply": {"policy": "p", "usage": {"prompt_tokens": 21, "completion_tokens": 3}}}]),
        )
        .await;
        let r = p.send(&s, "x", &[], &[]).await;
        let u = r.json()["usage"].clone();
        let (input, output) = if p == P::Chat {
            ("prompt_tokens", "completion_tokens")
        } else {
            ("input_tokens", "output_tokens")
        };
        assert_eq!((u[input].as_u64(), u[output].as_u64()), (Some(21), Some(3)), "{p:?}: {u}");

        let r = p.send(&s, "x", &[call_turn("c1", "t", json!({}), "r")], &["t"]).await;
        assert_eq!(r.status, 529, "{p:?}: {}", r.body);
        assert!(p.error_message(&r).contains("busy"), "{p:?}");
    }
}

#[tokio::test]
async fn streamed_policy_answers_on_every_protocol() {
    if !have("default") {
        return;
    }
    for p in ALL {
        let s = start().await;
        add_policy(
            &s,
            "p",
            "export default () => ({ text: 'streamed from policy', tool_calls: [{ name: 'read_file', arguments: { path: 'z.rs' } }] });",
            "default",
        )
        .await;
        attach(&s, "p").await;
        let mut body = p.body("go", &[], &TOOLS);
        body["stream"] = json!(true);
        let mut sse = open_sse(&s, p.path(), p.headers(), &body).await;
        assert_eq!(sse.status, 200, "{p:?}");
        let frames = sse.all().await.join("\n");
        for needle in ["policy", "read_file", "z.rs"] {
            assert!(frames.contains(needle), "{p:?}: {needle} missing in {frames}");
        }
    }
}

#[tokio::test]
async fn policy_may_pick_a_trajectory_on_every_protocol() {
    if !have("default") {
        return;
    }
    for p in ALL {
        let s = start().await;
        let r = send(
            &s,
            "PUT",
            "/_fakellm/trajectories",
            &[("content-type", "application/json")],
            &json!({"name": "script", "steps": [{"reply": {"text": "first"}}, {"reply": {"text": "second"}}]}).to_string(),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.body);
        add_policy(&s, "router", "export default () => ({ trajectory: 'script' });", "default").await;
        attach(&s, "router").await;
        assert_eq!(p.text(&p.send(&s, "go", &[], &[]).await), "first", "{p:?}");
        assert_eq!(
            p.text(&p.send(&s, "go", &[Turn::Say("first".into())], &[]).await),
            "second",
            "{p:?}"
        );
    }
}

#[tokio::test]
async fn request_log_marks_policy_answers_on_every_protocol() {
    if !have("default") {
        return;
    }
    for p in ALL {
        let s = start().await;
        add_policy(&s, "p", "export default () => ({ text: 'logged' });", "default").await;
        put_rules(&s, json!([{"name": "r", "reply": {"policy": "p"}}])).await;
        p.send(&s, "x", &[], &[]).await;
        let reqs = log(&s, "source=script").await;
        assert_eq!(reqs.len(), 1, "{p:?}");
        assert_eq!(reqs[0]["protocol"], p.log_name(), "{p:?}");
        assert_eq!(reqs[0]["source"]["kind"], "policy", "{p:?}");
        assert_eq!(reqs[0]["source"]["policy"], "p", "{p:?}");
        assert_eq!(reqs[0]["source"]["name"], "r", "{p:?}");
    }
}
