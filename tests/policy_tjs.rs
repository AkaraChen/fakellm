//! Policy scripts on a real txiki.js. Skipped unless `FAKELLM_TJS=/path/to/tjs` is set (there is no
//! prebuilt Linux release; build it from https://github.com/saghul/txiki.js, see docs/agent.md).

use serde_json::{Value, json};

mod common;
use common::*;

fn tjs() -> Option<String> {
    let p = std::env::var("FAKELLM_TJS").ok().filter(|p| !p.is_empty());
    if p.is_none() {
        eprintln!("skip: FAKELLM_TJS not set");
    }
    p
}

macro_rules! need_tjs {
    () => {
        match tjs() {
            Some(p) => p,
            None => return,
        }
    };
}

async fn put_policy(s: &fakellm::Server, rt: &str, name: &str, source: &str) -> Reply {
    send(
        s,
        "PUT",
        "/_fakellm/policies",
        &[("content-type", "application/json")],
        &json!({"name": name, "source": source, "runtime": rt}).to_string(),
    )
    .await
}

async fn add_policy(s: &fakellm::Server, rt: &str, name: &str, source: &str) {
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

fn text(r: &Reply) -> String {
    r.json()["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_owned()
}

fn code(r: &Reply) -> String {
    r.json()["error"]["code"].as_str().unwrap_or_default().to_owned()
}

fn msg(s: &str) -> Value {
    chat_body("gpt-4o", s)
}

#[tokio::test]
async fn state_hits_logs_and_crash_recovery() {
    let rt = need_tjs!();
    let s = start().await;
    add_policy(
        &s,
        &rt,
        "p",
        r#"export default function (request, context) {
  if (JSON.stringify(request).includes('CRASH')) tjs.exit(1);
  context.log('deciding', context.hits);
  console.log('plain log');
  const n = (context.state.n ?? 0) + 1;
  return { text: `n=${n} hits=${context.hits} tools=${request.tools.length}`, state: { n } };
}"#,
    )
    .await;
    attach(&s, "p").await;
    assert_eq!(text(&chat(&s, &msg("one")).await), "n=1 hits=0 tools=0");
    assert_eq!(text(&chat(&s, &msg("two")).await), "n=2 hits=1 tools=0");
    let r = chat(&s, &msg("CRASH now")).await;
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(code(&r).contains("policy"), "{}", r.body);
    assert_eq!(text(&chat(&s, &msg("three")).await), "n=3 hits=2 tools=0");
    let i = info(&s, "p").await;
    assert_eq!((i["restarts"].as_u64(), i["running"].as_bool()), (Some(1), Some(true)));
    let log = i["log"].to_string();
    assert!(log.contains("deciding") && log.contains("plain log"), "{log}");
}

#[tokio::test]
async fn errors_declines_and_bad_modules() {
    let rt = need_tjs!();
    let s = start().await;
    add_policy(&s, &rt, "t", "export default async () => { throw new Error('boom from tjs'); };").await;
    attach(&s, "t").await;
    let r = chat(&s, &msg("x")).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "fakellm_policy_error"), "{}", r.body);
    assert!(r.body.contains("boom from tjs"), "{}", r.body);

    add_policy(&s, &rt, "d", "export default () => null;").await;
    put_rules(&s, json!([{"reply": {"policy": "d"}}, {"reply": {"text": "fallthrough"}}])).await;
    assert_eq!(text(&chat(&s, &msg("hi")).await), "fallthrough");

    for source in [
        "export default (",
        "export const nothing = 1;",
        "export default 42;",
        "throw new Error('at load');",
    ] {
        let r = put_policy(&s, &rt, "bad", source).await;
        assert_eq!(r.status, 400, "{source}: {}", r.body);
    }
}

#[tokio::test]
async fn timeout_and_large_payloads() {
    let rt = need_tjs!();
    let s = start_with(fakellm::Config {
        policy_timeout_ms: 500,
        ..Default::default()
    })
    .await;
    add_policy(
        &s,
        &rt,
        "p",
        "export default async (request) => { const t = JSON.stringify(request); if (t.includes('SLEEP')) await new Promise((r) => setTimeout(r, 60000)); return { text: `len=${t.length >= 200000}` }; };",
    )
    .await;
    attach(&s, "p").await;
    let big = "x".repeat(300_000);
    assert_eq!(
        text(&chat(&s, &msg(&big)).await),
        "len=true",
        "a payload larger than the read buffer"
    );
    let r = chat(&s, &msg("SLEEP")).await;
    assert_eq!((r.status, code(&r).as_str()), (500, "fakellm_policy_timeout"), "{}", r.body);
    assert_eq!(text(&chat(&s, &msg(&big)).await), "len=true");
}
