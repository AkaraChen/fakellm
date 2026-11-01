//! Policy scripts over HTTP: JS modules decide the reply of a rule. Every scenario in `suite!` runs twice: on the default runtime (the
//! embedded txiki.js, or `node` in a build without the `tjs` feature) and on an explicit `node` subprocess, which is skipped with a notice
//! when `node` is not on PATH. The tests after the suite are specific to one runtime. Timeouts are host-clock, so these tests really wait.

use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};

mod common;
use common::*;

/// Which runtime a scenario registers its policies with.
#[derive(Clone, Copy, Debug)]
enum Rt {
    /// No `runtime` field: whatever the server defaults to.
    Default,
    /// `"runtime": "node"`: the subprocess path.
    Node,
}

impl Rt {
    /// The `runtime` field of a registration, absent for the default.
    fn field(self) -> Option<&'static str> {
        match self {
            Rt::Default => None,
            Rt::Node => Some("node"),
        }
    }

    /// What `GET /_fakellm/policies` reports as the runtime.
    fn reported(self) -> &'static str {
        match self {
            Rt::Node => "node",
            Rt::Default if cfg!(feature = "tjs") => "tjs",
            Rt::Default => "node",
        }
    }

    /// `PolicySpec::runtime`.
    fn config(self) -> Option<fakellm::Runtime> {
        self.field().map(|_| fakellm::Runtime::Node)
    }

    fn in_process(self) -> bool {
        self.reported() == "tjs"
    }

    fn spec(self, name: &str, source: &str) -> Value {
        let mut v = json!({"name": name, "source": source});
        if let Some(rt) = self.field() {
            v["runtime"] = json!(rt);
        }
        v
    }

    /// Whether this runtime can run here.
    fn available(self) -> bool {
        let needs_node = matches!(self, Rt::Node) || !cfg!(feature = "tjs");
        let ok = !needs_node || Command::new("node").arg("--version").output().is_ok();
        if !ok {
            eprintln!("skip: node not on PATH");
        }
        ok
    }
}

/// Runs each scenario on the default runtime and on explicit `node`.
macro_rules! suite {
    ($($name:ident),* $(,)?) => {
        mod default_runtime {
            use super::*;
            $(#[tokio::test]
            async fn $name() {
                if Rt::Default.available() {
                    super::$name(Rt::Default).await;
                }
            })*
        }
        mod node_runtime {
            use super::*;
            $(#[tokio::test]
            async fn $name() {
                if Rt::Node.available() {
                    super::$name(Rt::Node).await;
                }
            })*
        }
    };
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

async fn put_policy(s: &fakellm::Server, rt: Rt, name: &str, source: &str) -> Reply {
    post_policies(s, "PUT", &rt.spec(name, source)).await
}

async fn post_policies(s: &fakellm::Server, method: &str, body: &Value) -> Reply {
    send(
        s,
        method,
        "/_fakellm/policies",
        &[("content-type", "application/json")],
        &body.to_string(),
    )
    .await
}

async fn add_policy(s: &fakellm::Server, rt: Rt, name: &str, source: &str) {
    let r = put_policy(s, rt, name, source).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

async fn attach(s: &fakellm::Server, name: &str) {
    put_rules(s, json!([{"reply": {"policy": name}}])).await;
}

async fn info(s: &fakellm::Server, name: &str) -> Value {
    let r = get(s, &format!("/_fakellm/policies/{name}")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()
}

fn tools(names: &[&str]) -> Value {
    Value::Array(
        names
            .iter()
            .map(|n| json!({"type": "function", "function": {"name": n, "parameters": {"type": "object"}}}))
            .collect(),
    )
}

fn tool_turn(id: &str, name: &str, result: &str) -> [Value; 2] {
    [
        json!({"role": "assistant", "content": null, "tool_calls": [{"id": id, "type": "function", "function": {"name": name, "arguments": "{}"}}]}),
        json!({"role": "tool", "tool_call_id": id, "content": result}),
    ]
}

fn conversation(turns: &[(&str, &str, &str)]) -> Value {
    let mut messages = vec![json!({"role": "user", "content": "fix a.rs"})];
    for (id, name, result) in turns {
        messages.extend(tool_turn(id, name, result));
    }
    json!({"model": "gpt-4o", "messages": messages, "tools": tools(&["read_file", "edit_file"])})
}

fn tool_name(r: &Reply) -> String {
    r.json()["choices"][0]["message"]["tool_calls"][0]["function"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn text(r: &Reply) -> String {
    r.json()["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_owned()
}

fn code(r: &Reply) -> String {
    r.json()["error"]["code"].as_str().unwrap_or_default().to_owned()
}

fn msg(s: &str) -> Value {
    chat_body("gpt-4o", s)
}

async fn two_step_agent_loop(rt: Rt) {
    let s = start().await;
    add_policy(&s, rt, "agent", AGENT).await;
    attach(&s, "agent").await;

    let r = chat(&s, &conversation(&[])).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(tool_name(&r), "read_file");
    let r = chat(&s, &conversation(&[("c1", "read_file", "fn main() {}")])).await;
    assert_eq!(tool_name(&r), "edit_file");
    let r = chat(&s, &conversation(&[("c1", "read_file", "src"), ("c2", "edit_file", "applied")])).await;
    assert_eq!(text(&r), "edited");

    let i = info(&s, "agent").await;
    assert_eq!(i["hits"], 3);
    assert_eq!(i["state"]["stage"], "done");
    assert_eq!(i["runtime"], rt.reported());
    assert_eq!(i["in_process"], rt.in_process());
}

async fn retries_once_then_gives_up(rt: Rt) {
    let s = start().await;
    add_policy(&s, rt, "agent", AGENT).await;
    attach(&s, "agent").await;

    chat(&s, &conversation(&[])).await;
    chat(&s, &conversation(&[("c1", "read_file", "src")])).await;
    let r = chat(
        &s,
        &conversation(&[("c1", "read_file", "src"), ("c2", "edit_file", "ERROR: no match")]),
    )
    .await;
    assert_eq!(tool_name(&r), "edit_file", "first error: retry");
    let args: Value = serde_json::from_str(
        r.json()["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(args["try"], 2);
    let r = chat(
        &s,
        &conversation(&[
            ("c1", "read_file", "src"),
            ("c2", "edit_file", "ERROR: no match"),
            ("c3", "edit_file", "ERROR: again"),
        ]),
    )
    .await;
    assert_eq!(text(&r), "giving up");
}

async fn registration_accepts_yaml_and_lists(rt: Rt) {
    let s = start().await;
    let runtime = rt.field().map(|r| format!("runtime: {r}\n")).unwrap_or_default();
    let yaml = format!("name: yml\n{runtime}source: |\n  export default () => ({{ text: 'from yaml' }});\n");
    let r = send(&s, "PUT", "/_fakellm/policies", &[("content-type", "application/yaml")], &yaml).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json(), json!({"added": 1, "count": 1}));
    attach(&s, "yml").await;
    assert_eq!(text(&chat(&s, &msg("hi")).await), "from yaml");

    let r = post_policies(
        &s,
        "POST",
        &json!({"policies": [rt.spec("two", "export default () => ({ text: 'two' });")]}),
    )
    .await;
    assert_eq!(r.json(), json!({"added": 1, "count": 2}));
    let list = get(&s, "/_fakellm/policies").await.json();
    assert_eq!(list["policies"].as_array().unwrap().len(), 2);
    assert_eq!(get(&s, "/_fakellm").await.json()["policies"]["count"], 2);

    let r = post_policies(&s, "POST", &json!([rt.spec("two", "export default () => ({ text: 'two v2' });")])).await;
    assert_eq!(r.json()["count"], 2, "POST upserts by name");
    attach(&s, "two").await;
    assert_eq!(text(&chat(&s, &msg("hi")).await), "two v2");

    assert_eq!(
        send(&s, "DELETE", "/_fakellm/policies/two", &[], "").await.json(),
        json!({"deleted": true})
    );
    assert_eq!(get(&s, "/_fakellm/policies/two").await.status, 404);
    assert_eq!(send(&s, "DELETE", "/_fakellm/policies/two", &[], "").await.status, 404);
    assert_eq!(
        send(&s, "DELETE", "/_fakellm/policies", &[], "").await.json(),
        json!({"cleared": true})
    );
    assert_eq!(get(&s, "/_fakellm/policies").await.json()["policies"], json!([]));
}

async fn put_replaces_everything(rt: Rt) {
    let s = start().await;
    add_policy(&s, rt, "a", "export default () => ({ text: 'a' });").await;
    add_policy(&s, rt, "b", "export default () => ({ text: 'b' });").await;
    assert_eq!(get(&s, "/_fakellm/policies").await.json()["policies"].as_array().unwrap().len(), 1);
    assert_eq!(get(&s, "/_fakellm/policies/a").await.status, 404);
}

async fn state_survives_a_crash_and_hits_count_decisions(rt: Rt) {
    let s = start().await;
    add_policy(
        &s,
        rt,
        "p",
        r#"export default function (request, context) {
  if (JSON.stringify(request).includes('CRASH')) process.exit(1);
  const n = (context.state.n ?? 0) + 1;
  return { text: `n=${n} hits=${context.hits}`, state: { n } };
}"#,
    )
    .await;
    attach(&s, "p").await;
    assert_eq!(text(&chat(&s, &msg("one")).await), "n=1 hits=0");
    assert_eq!(text(&chat(&s, &msg("two")).await), "n=2 hits=1");

    let r = chat(&s, &msg("CRASH now")).await;
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(code(&r).contains("policy"), "{}", r.body);
    let i = info(&s, "p").await;
    assert_eq!(i["hits"], 2, "a failure is not a hit");

    assert_eq!(
        text(&chat(&s, &msg("three")).await),
        "n=3 hits=2",
        "state and hits outlive the process"
    );
    let i = info(&s, "p").await;
    assert_eq!(i["restarts"], 1);
    assert_eq!(i["running"], true);
}

async fn log_lines_appear_in_policy_info(rt: Rt) {
    let s = start().await;
    add_policy(
        &s,
        rt,
        "p",
        "export default (request, context) => { context.log('deciding', context.hits); console.log('plain log'); return { text: 'ok' }; };",
    )
    .await;
    attach(&s, "p").await;
    assert_eq!(text(&chat(&s, &msg("x")).await), "ok");
    let log = info(&s, "p").await["log"].clone();
    let lines: Vec<&str> = log.as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
    assert!(lines.iter().any(|l| l.contains("deciding") && l.contains('0')), "{lines:?}");
    assert!(
        lines.iter().any(|l| l.contains("plain log")),
        "console.log goes to stderr, {lines:?}"
    );
}

/// A policy that logs in a loop until the timeout must not grow the host without bound: the glue caps lines and line length.
async fn log_flood_is_bounded(rt: Rt) {
    if !matches!(rt, Rt::Default) {
        return; // the node and bun shims forward lines as they come; only the embedded runtime buffers them
    }
    let s = start_with(fakellm::Config {
        policy_timeout_ms: 1000,
        ..Default::default()
    })
    .await;
    add_policy(
        &s,
        rt,
        "p",
        "export default (request) => { if (JSON.stringify(request).includes('FLOOD')) { const big = 'y'.repeat(100000); for (;;) console.log(big); } return { text: 'ok' }; };",
    )
    .await;
    attach(&s, "p").await;
    let r = chat(&s, &msg("FLOOD")).await;
    assert_eq!((r.status, code(&r).as_str()), (500, "fakellm_policy_timeout"), "{}", r.body);
    let log = info(&s, "p").await["log"].clone();
    let lines = log.as_array().unwrap();
    assert!(lines.len() <= 1100, "{} lines", lines.len());
    assert!(lines.iter().all(|l| l.as_str().unwrap().len() <= 9000));
    assert_eq!(text(&chat(&s, &msg("hello")).await), "ok");
}

async fn throwing_policy_is_a_400_with_its_message(rt: Rt) {
    let s = start().await;
    add_policy(&s, rt, "p", "export default () => { throw new Error('boom from policy'); };").await;
    attach(&s, "p").await;
    let r = chat(&s, &msg("x")).await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(code(&r), "fakellm_policy_error");
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("boom from policy"));
    assert_eq!(info(&s, "p").await["hits"], 0);

    add_policy(&s, rt, "p", "export default async () => { throw new Error('async boom'); };").await;
    let r = chat(&s, &msg("x")).await;
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("async boom"), "{}", r.body);
}

async fn invalid_replies_are_policy_errors(rt: Rt) {
    let s = start().await;
    attach(&s, "p").await;
    for (source, why) in [
        ("export default () => ({});", "empty reply"),
        ("export default () => 42;", "not an object"),
        ("export default () => ({ text: 'x', error: { status: 500 } });", "text and error"),
        ("export default () => ({ policy: 'p' });", "policy is not allowed"),
        ("export default () => ({ text: 'x', bogus: 1 });", "unknown field"),
    ] {
        add_policy(&s, rt, "p", source).await;
        let r = chat(&s, &msg("x")).await;
        assert_eq!(r.status, 400, "{why}: {}", r.body);
        assert_eq!(code(&r), "fakellm_policy_error", "{why}");
    }
}

async fn unknown_policy_is_a_per_request_400(_rt: Rt) {
    let s = start().await;
    attach(&s, "ghost").await;
    let r = chat(&s, &msg("x")).await;
    assert_eq!(r.status, 400);
    assert!(code(&r).contains("policy"), "{}", r.body);
}

async fn timeout_is_a_500_and_the_next_request_works(rt: Rt) {
    let s = start_with(fakellm::Config {
        policy_timeout_ms: 300,
        ..Default::default()
    })
    .await;
    add_policy(
        &s,
        rt,
        "p",
        "export default async (request) => { if (JSON.stringify(request).includes('SLEEP')) await new Promise((r) => setTimeout(r, 60000)); return { text: 'awake' }; };",
    )
    .await;
    attach(&s, "p").await;
    assert_eq!(text(&chat(&s, &msg("hello")).await), "awake");
    let r = chat(&s, &msg("SLEEP")).await;
    assert_eq!(r.status, 500, "{}", r.body);
    assert_eq!(code(&r), "fakellm_policy_timeout");
    assert_eq!(text(&chat(&s, &msg("hello again")).await), "awake");
    assert_eq!(info(&s, "p").await["restarts"], 1);
}

async fn bad_modules_are_rejected_at_registration(rt: Rt) {
    let s = start().await;
    add_policy(&s, rt, "keep", "export default () => ({ text: 'kept' });").await;

    for (source, why) in [
        ("export default (", "syntax error"),
        ("export const nothing = 1;", "no default export"),
        ("export default 42;", "default export is not a function"),
        ("throw new Error('at load');", "throws at load"),
    ] {
        let r = put_policy(&s, rt, "bad", source).await;
        assert_eq!(r.status, 400, "{why}: {}", r.body);
        assert!(r.json()["error"]["message"].as_str().unwrap().contains("bad"), "{why}: {}", r.body);
    }
    let r = post_policies(
        &s,
        "PUT",
        &json!({"name": "x", "source": "export default () => ({text: 'x'});", "runtime": "/definitely/not/a/runtime"}),
    )
    .await;
    assert_eq!(r.status, 400, "{}", r.body);
    let r = post_policies(&s, "PUT", &rt.spec("bad name!", "export default () => ({text: 'x'});")).await;
    assert_eq!(r.status, 400, "{}", r.body);
    let mut no_source = rt.spec("no-source", "");
    no_source.as_object_mut().unwrap().remove("source");
    let r = post_policies(&s, "PUT", &no_source).await;
    assert_eq!(r.status, 400, "{}", r.body);

    let names: Vec<_> = get(&s, "/_fakellm/policies").await.json()["policies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["keep"], "a rejected PUT changes nothing");
}

async fn policy_may_pick_a_trajectory(rt: Rt) {
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
    add_policy(&s, rt, "router", "export default (request) => ({ trajectory: 'script' });").await;
    attach(&s, "router").await;
    assert_eq!(text(&chat(&s, &msg("go")).await), "first");
    let again = json!({"model": "gpt-4o", "messages": [
        {"role": "user", "content": "go"}, {"role": "assistant", "content": "first"}, {"role": "user", "content": "next"}]});
    assert_eq!(text(&chat(&s, &again).await), "second");
}

async fn rule_delivery_fields_are_defaults_for_the_reply(rt: Rt) {
    let s = start().await;
    add_policy(&s, rt, "p", "export default () => ({ text: 'alpha beta gamma delta' });").await;
    put_rules(
        &s,
        json!([{"reply": {"policy": "p", "usage": {"prompt_tokens": 21, "completion_tokens": 3}, "stream": {"chunk_tokens": 1}}}]),
    )
    .await;
    let u = chat(&s, &msg("x")).await.json()["usage"].clone();
    assert_eq!((u["prompt_tokens"].as_u64(), u["completion_tokens"].as_u64()), (Some(21), Some(3)));

    let mut body = msg("x");
    body["stream"] = json!(true);
    let mut sse = open_stream(&s, &body).await;
    let chunks = json_chunks(&sse.all().await);
    let deltas = chunks
        .iter()
        .filter(|c| c["choices"][0]["delta"]["content"].as_str().is_some_and(|t| !t.is_empty()))
        .count();
    assert_eq!(deltas, 4);

    add_policy(
        &s,
        rt,
        "p",
        "export default () => ({ text: 'own', usage: { prompt_tokens: 5, completion_tokens: 1 } });",
    )
    .await;
    let u = chat(&s, &msg("x")).await.json()["usage"].clone();
    assert_eq!(u["prompt_tokens"], 5, "the policy's own fields win");
}

async fn concurrent_requests_are_serialized(rt: Rt) {
    let s = start().await;
    add_policy(
        &s,
        rt,
        "p",
        "export default async (request, context) => { const n = context.state.n ?? 0; await new Promise((r) => setTimeout(r, 20)); return { text: String(n), state: { n: n + 1 } }; };",
    )
    .await;
    attach(&s, "p").await;
    let bodies: Vec<_> = (0..8).map(|i| msg(&format!("req {i}"))).collect();
    let futs: Vec<_> = bodies.iter().map(|b| chat(&s, b)).collect();
    let mut seen: Vec<u32> = futures::future::join_all(futs)
        .await
        .iter()
        .map(|r| text(r).parse().unwrap())
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        (0..8).collect::<Vec<_>>(),
        "every decision saw the state the previous one left"
    );
    assert_eq!(info(&s, "p").await["state"]["n"], 8);
}

async fn request_log_marks_policy_answers(rt: Rt) {
    let s = start().await;
    add_policy(&s, rt, "p", "export default () => ({ text: 'logged' });").await;
    put_rules(&s, json!([{"name": "r", "reply": {"policy": "p"}}])).await;
    chat(&s, &msg("x")).await;
    let log = get(&s, "/_fakellm/requests?source=script").await.json();
    let src = &log["requests"][0]["source"];
    assert_eq!(src["kind"], "policy");
    assert_eq!(src["policy"], "p");
    assert_eq!(src["name"], "r");
}

fn policy_dir_with(marker: &str) -> Option<PathBuf> {
    std::fs::read_dir(std::env::temp_dir()).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("fakellm-policy-"))
            && std::fs::read_to_string(p.join("policy.mjs")).is_ok_and(|s| s.contains(marker))
    })
}

async fn reset_removes_policies_and_their_files(rt: Rt) {
    let s = start().await;
    let marker = "reset-marker-8f3a1c";
    add_policy(&s, rt, "p", &format!("export default () => ({{ text: '{marker}' }}); // {marker}")).await;
    assert!(policy_dir_with(marker).is_some(), "registration writes the module to a temp dir");
    assert_eq!(post_json(&s, "/_fakellm/reset", &json!({})).await.status, 200);
    assert_eq!(get(&s, "/_fakellm/policies").await.json()["policies"], json!([]));
    // The in-process runtime removes the directory from its joiner thread, so allow it a moment.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while policy_dir_with(marker).is_some() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(policy_dir_with(marker).is_none(), "reset deletes the temp dir");
}

async fn config_policies_load_at_start(rt: Rt) {
    let spec = fakellm::PolicySpec {
        name: "boot".into(),
        source: "export default () => ({ text: 'from config' });".into(),
        files: Default::default(),
        runtime: rt.config(),
    };
    let s = start_with(fakellm::Config {
        policies: vec![spec],
        ..Default::default()
    })
    .await;
    assert_eq!(get(&s, "/_fakellm").await.json()["policies"]["count"], 1);
    attach(&s, "boot").await;
    assert_eq!(text(&chat(&s, &msg("x")).await), "from config");

    let broken = fakellm::PolicySpec {
        name: "broken".into(),
        source: "export default (".into(),
        files: Default::default(),
        runtime: rt.config(),
    };
    let err = fakellm::Server::start(fakellm::Config {
        policies: vec![broken],
        ..Default::default()
    })
    .await
    .err()
    .expect("a module that does not load fails the start");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

async fn policy_drives_responses_and_anthropic_too(rt: Rt) {
    let s = start().await;
    add_policy(
        &s,
        rt,
        "p",
        "export default (request) => ({ text: `saw ${request.protocol} ${request.messages.length}` });",
    )
    .await;
    attach(&s, "p").await;
    let r = post_json(&s, "/v1/responses", &json!({"model": "gpt-4o", "input": "hi"})).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("saw openai.responses 1"), "{}", r.body);
    let r = post_json_with(
        &s,
        "/v1/messages",
        &[("anthropic-version", "2023-06-01")],
        &json!({"model": "claude-sonnet-4", "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("saw anthropic.messages 1"), "{}", r.body);
}

async fn idle_death_restarts_before_the_next_request(rt: Rt) {
    let s = start().await;
    add_policy(
        &s,
        rt,
        "mortal",
        "export default (req, ctx) => { setTimeout(() => process.exit(1), 50); return { text: 'ok' + ctx.hits }; }",
    )
    .await;
    attach(&s, "mortal").await;
    for n in 0..3 {
        let r = chat(&s, &msg("hi")).await;
        assert_eq!((r.status, text(&r)), (200, format!("ok{n}")), "{}", r.body);
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    assert_eq!(info(&s, "mortal").await["restarts"], 2);
}

async fn declining_policy_falls_through_to_the_next_rule(rt: Rt) {
    let s = start().await;
    // null, undefined and {decline: true} all mean "not mine"
    for (i, body) in ["return null;", "return undefined;", "return { decline: true };"]
        .iter()
        .enumerate()
    {
        let name = format!("d{i}");
        add_policy(&s, rt, &name, &format!("export default () => {{ {body} }}")).await;
        put_rules(
            &s,
            json!([
                {"name": "pol", "times": 5, "reply": {"policy": name}},
                {"name": "txt", "reply": {"text": "fallthrough"}},
            ]),
        )
        .await;
        let r = chat(&s, &msg("hi")).await;
        assert_eq!((r.status, text(&r)), (200, "fallthrough".to_owned()), "{body}");
        let rules = get(&s, "/_fakellm/scripts").await.json();
        let rules = rules["rules"].as_array().unwrap_or_else(|| panic!("{rules}")).clone();
        assert_eq!((rules[0]["hits"].as_u64(), rules[1]["hits"].as_u64()), (Some(0), Some(1)), "{body}");
        assert_eq!(info(&s, &name).await["hits"], 0, "a decline is not a policy hit");
    }
}

async fn policy_declines_one_request_and_accepts_a_later_one(rt: Rt) {
    let s = start().await;
    add_policy(
        &s,
        rt,
        "picky",
        "export default (r, c) => r.messages.at(-1).content.map((p) => p.text ?? '').join('').includes('mine') ? { text: 'policy', state: { n: (c.state?.n ?? 0) + 1 } } : null;",
    )
    .await;
    put_rules(&s, json!([{"reply": {"policy": "picky"}}, {"reply": {"text": "other"}}])).await;
    assert_eq!(text(&chat(&s, &msg("hello")).await), "other");
    assert_eq!(text(&chat(&s, &msg("this is mine")).await), "policy");
    assert_eq!(text(&chat(&s, &msg("hello again")).await), "other");
    let i = info(&s, "picky").await;
    assert_eq!((i["hits"].as_u64(), i["state"]["n"].as_u64()), (Some(1), Some(1)));
}

async fn declining_everything_reaches_the_fallback(rt: Rt) {
    let s = start().await;
    add_policy(&s, rt, "no", "export default () => null;").await;
    attach(&s, "no").await;
    let r = chat(&s, &msg("hi")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_ne!(code(&r), "fakellm_policy_error");
}

#[tokio::test]
async fn match_body_sees_the_raw_json() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"match": {"body": "\"damaged\"\\s*:\\s*true"}, "reply": {"text": "angry"}},
            {"reply": {"text": "calm"}},
        ]),
    )
    .await;
    let mut body = msg("hi");
    assert_eq!(text(&chat(&s, &body).await), "calm");
    body["damaged"] = json!(true);
    assert_eq!(text(&chat(&s, &body).await), "angry");
}

async fn policy_files_let_the_entry_import_a_helper(rt: Rt) {
    let s = start().await;
    let mut body = rt.spec(
        "multi",
        "import { greet } from './helper.js';\nimport { n } from './lib/num.mjs';\nexport default () => ({ text: greet('x') + n });",
    );
    body["files"] = json!({
        "helper.js": "export const greet = (w) => 'hello ' + w + ' ';",
        "lib/num.mjs": "export const n = 42;"
    });
    let r = send(
        &s,
        "PUT",
        "/_fakellm/policies",
        &[("content-type", "application/json")],
        &body.to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    attach(&s, "multi").await;
    assert_eq!(text(&chat(&s, &msg("x")).await), "hello x 42");

    for bad in ["../evil.js", "/etc/x", "policy.mjs", ""] {
        let mut b = body.clone();
        b["name"] = json!("bad");
        b["files"] = json!({ bad: "x" });
        let r = send(
            &s,
            "PUT",
            "/_fakellm/policies",
            &[("content-type", "application/json")],
            &b.to_string(),
        )
        .await;
        assert_eq!(r.status, 400, "{bad}: {}", r.body);
    }
}

suite!(
    two_step_agent_loop,
    retries_once_then_gives_up,
    registration_accepts_yaml_and_lists,
    put_replaces_everything,
    log_lines_appear_in_policy_info,
    log_flood_is_bounded,
    throwing_policy_is_a_400_with_its_message,
    invalid_replies_are_policy_errors,
    unknown_policy_is_a_per_request_400,
    timeout_is_a_500_and_the_next_request_works,
    bad_modules_are_rejected_at_registration,
    policy_may_pick_a_trajectory,
    rule_delivery_fields_are_defaults_for_the_reply,
    concurrent_requests_are_serialized,
    request_log_marks_policy_answers,
    reset_removes_policies_and_their_files,
    config_policies_load_at_start,
    policy_drives_responses_and_anthropic_too,
    declining_policy_falls_through_to_the_next_rule,
    policy_declines_one_request_and_accepts_a_later_one,
    declining_everything_reaches_the_fallback,
    policy_files_let_the_entry_import_a_helper,
);

// A subprocess runtime can die on its own; these scenarios exit the process from inside the policy.

#[tokio::test]
async fn node_state_survives_a_crash_and_hits_count_decisions() {
    if Rt::Node.available() {
        state_survives_a_crash_and_hits_count_decisions(Rt::Node).await;
    }
}

#[tokio::test]
async fn node_idle_death_restarts_before_the_next_request() {
    if Rt::Node.available() {
        idle_death_restarts_before_the_next_request(Rt::Node).await;
    }
}

// The in-process runtime (txiki.js embedded in the binary).

#[cfg(feature = "tjs")]
mod in_process {
    use super::*;

    const TJS: Rt = Rt::Default;

    #[tokio::test]
    async fn the_default_runtime_is_in_process_and_listed_as_such() {
        let s = start().await;
        add_policy(&s, TJS, "p", "export default () => ({ text: 'in process' });").await;
        attach(&s, "p").await;
        assert_eq!(text(&chat(&s, &msg("x")).await), "in process");
        let i = info(&s, "p").await;
        assert_eq!(
            (i["runtime"].as_str(), i["in_process"].as_bool(), i["running"].as_bool()),
            (Some("tjs"), Some(true), Some(true))
        );
        let list = get(&s, "/_fakellm/policies").await.json();
        assert_eq!(list["policies"][0]["in_process"], true);

        // An explicit runtime stays a subprocess.
        if Rt::Node.available() {
            add_policy(&s, Rt::Node, "n", "export default () => ({ text: 'subprocess' });").await;
            let i = info(&s, "n").await;
            assert_eq!((i["runtime"].as_str(), i["in_process"].as_bool()), (Some("node"), Some(false)));
        }
        // "tjs" is accepted as the explicit spelling of the default.
        let r = post_policies(
            &s,
            "PUT",
            &json!({"name": "e", "source": "export default () => null;", "runtime": "tjs"}),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(info(&s, "e").await["in_process"], true);
    }

    #[tokio::test]
    async fn the_tjs_api_is_available() {
        let s = start().await;
        add_policy(
            &s,
            TJS,
            "api",
            r#"import path from "tjs:path";
export default async function decide(request, context) {
  const bytes = new TextEncoder().encode("abc");
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  const dir = await tjs.makeTempDir("fakellm-api-XXXXXX");
  const file = path.join(dir, "note.txt");
  await tjs.writeFile(file, "from tjs.writeFile");
  const back = new TextDecoder().decode(await tjs.readFile(file));
  await tjs.remove(dir);
  const seen = [];
  await new Promise((resolve) => setTimeout(() => { seen.push("timer"); resolve(); }, 10));
  return {
    text: JSON.stringify({
      version: typeof tjs.version,
      env: typeof tjs.env,
      fetch: typeof fetch,
      url: new URL("https://example.com/a?b=1").searchParams.get("b"),
      digest: digest.byteLength,
      uuid: crypto.randomUUID().length,
      back,
      seen,
      structured: structuredClone({ a: [1, 2] }).a.length,
      cwd: typeof tjs.cwd,
    }),
  };
}"#,
        )
        .await;
        attach(&s, "api").await;
        let r = chat(&s, &msg("x")).await;
        assert_eq!(r.status, 200, "{}", r.body);
        let seen: Value = serde_json::from_str(&text(&r)).unwrap();
        assert_eq!(
            seen,
            json!({"version": "string", "env": "object", "fetch": "function", "url": "1", "digest": 32, "uuid": 36,
                   "back": "from tjs.writeFile", "seen": ["timer"], "structured": 2, "cwd": "string"})
        );
    }

    #[tokio::test]
    async fn a_busy_loop_times_out_and_the_next_decision_gets_a_fresh_runtime() {
        let s = start_with(fakellm::Config {
            policy_timeout_ms: 300,
            ..Default::default()
        })
        .await;
        add_policy(
            &s,
            TJS,
            "p",
            "let calls = 0;\nexport default (request, ctx) => { calls++; if (JSON.stringify(request).includes('SPIN')) { for (;;) {} } return { text: `calls=${calls} hits=${ctx.hits}`, state: { kept: ctx.hits } }; };",
        )
        .await;
        attach(&s, "p").await;
        assert_eq!(text(&chat(&s, &msg("one")).await), "calls=1 hits=0");
        let started = std::time::Instant::now();
        let r = chat(&s, &msg("SPIN")).await;
        assert_eq!((r.status, code(&r).as_str()), (500, "fakellm_policy_timeout"), "{}", r.body);
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "{:?}", started.elapsed());
        let i = info(&s, "p").await;
        assert_eq!(
            (i["running"].as_bool(), i["hits"].as_u64(), i["restarts"].as_u64()),
            (Some(false), Some(1), Some(0))
        );

        // A new runtime: the module's own variables start over, fakellm's state and hit count do not.
        assert_eq!(text(&chat(&s, &msg("two")).await), "calls=1 hits=1");
        let i = info(&s, "p").await;
        assert_eq!(
            (i["running"].as_bool(), i["hits"].as_u64(), i["restarts"].as_u64()),
            (Some(true), Some(2), Some(1))
        );
        assert_eq!(i["state"], json!({"kept": 1}));
    }

    #[tokio::test]
    async fn a_policy_that_cannot_load_again_is_an_error_and_recovers() {
        // The module loads once for the dry run; after a timeout it is loaded again and may fail then.
        let s = start_with(fakellm::Config {
            policy_timeout_ms: 300,
            ..Default::default()
        })
        .await;
        add_policy(
            &s,
            TJS,
            "p",
            "if (await tjs.stat(import.meta.dirname + '/second.flag').then(() => true, () => false)) throw new Error('second load fails');\nexport default (request) => { if (JSON.stringify(request).includes('SPIN')) { for (;;) {} } return { text: 'ok' }; };",
        )
        .await;
        attach(&s, "p").await;
        assert_eq!(text(&chat(&s, &msg("one")).await), "ok");
        let dir = policy_dir_with("second load fails").expect("the policy dir exists");
        assert_eq!(code(&chat(&s, &msg("SPIN")).await), "fakellm_policy_timeout");
        std::fs::write(dir.join("second.flag"), "").unwrap();
        let r = chat(&s, &msg("two")).await;
        assert_eq!((r.status, code(&r).as_str()), (400, "fakellm_policy_error"), "{}", r.body);
        assert!(r.body.contains("second load fails"), "{}", r.body);
        std::fs::remove_file(dir.join("second.flag")).unwrap();
        assert_eq!(text(&chat(&s, &msg("three")).await), "ok");
        assert_eq!(info(&s, "p").await["restarts"], 1);
    }

    #[tokio::test]
    async fn removing_a_policy_interrupts_the_decision_in_flight() {
        let s = std::sync::Arc::new(start().await);
        add_policy(
            &s,
            TJS,
            "slow",
            "export default async () => { await new Promise((r) => setTimeout(r, 60000)); return { text: 'late' }; };",
        )
        .await;
        attach(&s, "slow").await;
        let pending = {
            let s = s.clone();
            tokio::spawn(async move { chat(&s, &msg("x")).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let started = std::time::Instant::now();
        assert_eq!(send(&s, "DELETE", "/_fakellm/policies/slow", &[], "").await.status, 200);
        let r = pending.await.unwrap();
        assert_eq!((r.status, code(&r).as_str()), (500, "fakellm_policy_crashed"), "{}", r.body);
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_client_that_goes_away_mid_decision_does_not_wedge_the_policy() {
        let s = start().await;
        add_policy(
            &s,
            TJS,
            "p",
            "export default async (request) => { if (JSON.stringify(request).includes('HANG')) await new Promise(() => setTimeout(() => {}, 60000)); return { text: 'fine' }; };",
        )
        .await;
        attach(&s, "p").await;
        let hung = tokio::time::timeout(std::time::Duration::from_millis(300), chat(&s, &msg("HANG"))).await;
        assert!(hung.is_err(), "the request should still be waiting");
        // The abandoned decision is cancelled, so this one does not wait behind it.
        let started = std::time::Instant::now();
        assert_eq!(text(&chat(&s, &msg("next")).await), "fine");
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        assert_eq!(info(&s, "p").await["restarts"], 1);
    }

    #[tokio::test]
    async fn a_promise_that_never_settles_is_an_error_not_a_hang() {
        let s = start().await;
        add_policy(&s, TJS, "p", "export default () => new Promise(() => {});").await;
        attach(&s, "p").await;
        let r = chat(&s, &msg("x")).await;
        assert_eq!((r.status, code(&r).as_str()), (400, "fakellm_policy_error"), "{}", r.body);
        assert!(r.body.contains("never settle"), "{}", r.body);
    }

    #[tokio::test]
    async fn policies_do_not_block_each_other() {
        let s = start().await;
        add_policy(
            &s,
            TJS,
            "slow",
            "export default async () => { await new Promise((r) => setTimeout(r, 400)); return { text: 'slow' }; };",
        )
        .await;
        post_policies(
            &s,
            "POST",
            &json!({"name": "fast", "source": "export default () => ({ text: 'fast' });"}),
        )
        .await;
        put_rules(
            &s,
            json!([{"match": {"last_user": "slow"}, "reply": {"policy": "slow"}}, {"reply": {"policy": "fast"}}]),
        )
        .await;
        let slow_body = msg("slow");
        let slow = chat(&s, &slow_body);
        let fast = async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let started = std::time::Instant::now();
            let r = chat(&s, &msg("quick")).await;
            (text(&r), started.elapsed())
        };
        let (slow, (fast, took)) = tokio::join!(slow, fast);
        assert_eq!((text(&slow).as_str(), fast.as_str()), ("slow", "fast"));
        assert!(took < std::time::Duration::from_millis(300), "{took:?}");
    }

    #[tokio::test]
    async fn many_policies_and_a_big_state_round_trip() {
        let s = start().await;
        let specs: Vec<_> = (0..12)
            .map(|i| TJS.spec(&format!("p{i}"), &format!("export default (r, c) => ({{ text: 'p{i} ' + (c.state.blob?.length ?? 0), state: {{ blob: 'x'.repeat(200000) }} }});")))
            .collect();
        let r = post_policies(&s, "PUT", &Value::Array(specs)).await;
        assert_eq!(r.status, 200, "{}", r.body);
        for i in [0, 5, 11] {
            put_rules(&s, json!([{"reply": {"policy": format!("p{i}")}}])).await;
            assert_eq!(text(&chat(&s, &msg("a")).await), format!("p{i} 0"));
            assert_eq!(text(&chat(&s, &msg("b")).await), format!("p{i} 200000"));
        }
    }
}

#[cfg(not(feature = "tjs"))]
#[tokio::test]
async fn without_the_tjs_feature_the_in_process_runtime_is_refused_clearly() {
    let s = start().await;
    let r = post_policies(
        &s,
        "PUT",
        &json!({"name": "p", "source": "export default () => null;", "runtime": "tjs"}),
    )
    .await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("feature `tjs`"), "{}", r.body);
}
