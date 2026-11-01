//! Control-plane snapshots: capture, mutate, restore, plus a restore racing a PUT and a restore that fails
//! (docs/m4-spec.md section 8 and 11).

use std::process::Command;

use fakellm::Server;
use serde_json::{Value, json};

mod common;
use common::*;

fn have_node() -> bool {
    let ok = Command::new("node").arg("--version").output().is_ok();
    if !ok {
        eprintln!("skip: node not on PATH (policy part of the snapshot test)");
    }
    ok
}

/// The default policy runtime works here: the embedded one, or `node` in a build without it.
fn have_policy_runtime() -> bool {
    cfg!(feature = "tjs") || have_node()
}

async fn snap(s: &Server, name: &str) -> Reply {
    ctl_json(s, "POST", "/_fakellm/snapshots", &json!({"name": name})).await
}

async fn restore(s: &Server, name: &str) -> Reply {
    send(s, "POST", &format!("/_fakellm/snapshots/{name}/restore"), &[], "").await
}

async fn rules(s: &Server) -> Vec<Value> {
    get(s, "/_fakellm/scripts").await.json()["rules"].as_array().unwrap().clone()
}

async fn trajectory(s: &Server, name: &str) -> Value {
    get(s, "/_fakellm/trajectories").await.json()["trajectories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("no trajectory {name}"))
        .clone()
}

async fn cassette_info(s: &Server, name: &str) -> Value {
    get(s, "/_fakellm/cassettes").await.json()["cassettes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no cassette {name}"))
        .clone()
}

const COUNTER: &str =
    "export default function decide(request, context) { const n = (context.state.n ?? 0) + 1; return { text: `n=${n}`, state: { n } }; }";

#[tokio::test]
async fn restore_brings_everything_back() {
    let node = have_policy_runtime();
    let s = start().await;
    s.clock().freeze(None).unwrap();
    let frozen_at = get(&s, "/_fakellm/clock").await.json()["now"].clone();

    put_rules(
        &s,
        json!([{"name": "greet", "match": {"last_user": "hi"}, "reply": {"text": "hello"}}]),
    )
    .await;
    let r = ctl_json(
        &s,
        "PUT",
        "/_fakellm/trajectories",
        &json!({"name": "t", "cursor": "by_hits", "dedupe": false, "steps": [{"reply": {"text": "s0"}}, {"reply": {"text": "s1"}}, {"reply": {"text": "s2"}}]}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let mut c = cassette("tape", vec![exchange("a", "A"), exchange("a", "A2")]);
    c["once"] = json!(true);
    put_cassettes(&s, &c).await;
    if node {
        let r = ctl_json(&s, "PUT", "/_fakellm/policies", &json!({"name": "counter", "source": COUNTER})).await;
        assert_eq!(r.status, 200, "{}", r.body);
    }

    // Reach the baseline: one rule hit, a trajectory one step in, one cassette exchange spent, a stored response.
    chat(&s, &chat_body("gpt-4o", "hi")).await;
    put_rules(&s, json!([{"name": "greet", "match": {"last_user": "hi"}, "reply": {"text": "hello"}}, {"name": "traj", "match": {"last_user": "walk"}, "reply": {"trajectory": "t"}}])).await;
    assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "walk")).await), "s0");
    assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "a")).await), "A");
    let resp = responses(&s, &responses_body("gpt-5.5", "remember me")).await.json();
    let resp_id = resp["id"].as_str().unwrap().to_owned();
    if node {
        put_rules(&s, json!([{"name": "greet", "match": {"last_user": "hi"}, "reply": {"text": "hello"}}, {"name": "traj", "match": {"last_user": "walk"}, "reply": {"trajectory": "t"}}, {"name": "pol", "match": {"last_user": "count"}, "reply": {"policy": "counter"}}])).await;
        assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "count")).await), "n=1");
        assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "count")).await), "n=2");
    }
    let before_rules = rules(&s).await;
    let before_traj = trajectory(&s, "t").await;
    let before_cass = cassette_info(&s, "tape").await;
    assert_eq!(before_traj["hits"], 1);
    assert_eq!(before_cass["consumed"], 1);

    let r = snap(&s, "base").await;
    assert_eq!(r.status, 201, "{}", r.body);
    let info = r.json();
    assert_eq!(info["name"], "base");
    assert_eq!(info["trajectories"], 1);
    assert_eq!(info["cassettes"], 1);
    assert_eq!(info["responses"], 1);

    // Mutate everything.
    put_rules(&s, json!([{"reply": {"text": "other"}}])).await;
    chat(&s, &chat_body("gpt-4o", "x")).await;
    send(&s, "DELETE", "/_fakellm/trajectories", &[], "").await;
    send(&s, "DELETE", "/_fakellm/cassettes", &[], "").await;
    send(&s, "DELETE", "/_fakellm/policies", &[], "").await;
    s.clear_responses();
    post_json(&s, "/_fakellm/clock/advance", &json!({"by": "1h"})).await;

    for _ in 0..2 {
        let r = restore(&s, "base").await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(rules(&s).await, before_rules);
        let t = trajectory(&s, "t").await;
        assert_eq!(
            (t["hits"].clone(), t["last_step"].clone()),
            (before_traj["hits"].clone(), before_traj["last_step"].clone())
        );
        let c = cassette_info(&s, "tape").await;
        assert_eq!(
            (c["hits"].clone(), c["consumed"].clone()),
            (before_cass["hits"].clone(), before_cass["consumed"].clone())
        );
        assert_eq!(get(&s, "/_fakellm/clock").await.json()["now"], frozen_at);
        assert!(
            get(&s, &format!("/v1/responses/{resp_id}")).await.status == 200,
            "stored response is back"
        );
        // Behavior continues from the captured point: the trajectory's next step, the cassette's second exchange.
        assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "a")).await), "A2");
        assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "walk")).await), "s1");
        if node {
            assert_eq!(
                chat_text(&chat(&s, &chat_body("gpt-4o", "count")).await),
                "n=3",
                "policy state came back"
            );
        }
    }
}

#[tokio::test]
async fn snapshot_names_listing_reset_and_delete() {
    let s = start().await;
    assert_eq!(snap(&s, "a").await.status, 201);
    assert_eq!(snap(&s, "a").await.status, 409);
    for bad in ["", "has space", "slash/name", &"x".repeat(65)] {
        assert_eq!(snap(&s, bad).await.status, 400, "{bad:?}");
    }
    let auto = ctl_json(&s, "POST", "/_fakellm/snapshots", &json!({})).await;
    assert_eq!(auto.status, 201, "{}", auto.body);
    assert!(auto.json()["name"].as_str().unwrap().starts_with("snapshot-"));
    let via_query = send(&s, "POST", "/_fakellm/snapshots?name=q", &[], "").await;
    assert_eq!(via_query.status, 201, "{}", via_query.body);

    let list = get(&s, "/_fakellm/snapshots").await.json();
    let names: Vec<&str> = list["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    assert_eq!(get(&s, "/_fakellm").await.json()["snapshots"]["count"], 3);
    assert_eq!(get(&s, "/_fakellm/snapshots/a").await.status, 200);
    assert_eq!(restore(&s, "nope").await.status, 404);

    // reset keeps snapshots; reset?snapshots=true drops them.
    send(&s, "POST", "/_fakellm/reset", &[], "").await;
    assert_eq!(get(&s, "/_fakellm/snapshots/a").await.status, 200);
    send(&s, "POST", "/_fakellm/reset?snapshots=true", &[], "").await;
    assert_eq!(get(&s, "/_fakellm/snapshots").await.json(), json!({"snapshots": []}));

    snap(&s, "b").await;
    assert_eq!(send(&s, "DELETE", "/_fakellm/snapshots/b", &[], "").await.status, 200);
    assert_eq!(send(&s, "DELETE", "/_fakellm/snapshots/b", &[], "").await.status, 404);
}

#[tokio::test]
async fn library_snapshot_and_restore() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "one"}}])).await;
    s.control().snapshot(Some("lib")).await.unwrap();
    put_rules(&s, json!([{"reply": {"text": "two"}}])).await;
    s.control().restore("lib").await.unwrap();
    assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "x")).await), "one");
    assert!(s.control().restore("missing").await.is_err());
}

#[tokio::test]
async fn restore_racing_a_put_ends_in_one_of_the_two_states() {
    let s = start().await;
    put_rules(&s, json!([{"name": "base", "reply": {"text": "base"}}])).await;
    put_cassettes(&s, &cassette("base", vec![exchange("q", "base")])).await;
    snap(&s, "base").await;
    for round in 0..10 {
        let put = async {
            put_rules(&s, json!([{"name": "other", "reply": {"text": "other"}}])).await;
            put_cassettes(&s, &cassette("other", vec![exchange("q", "other")])).await;
        };
        let (_, r) = tokio::join!(put, restore(&s, "base"));
        assert_eq!(r.status, 200, "{}", r.body);
        let names: Vec<String> = rules(&s).await.iter().map(|r| r["name"].as_str().unwrap().to_owned()).collect();
        assert_eq!(names.len(), 1, "round {round}: a merge of both states: {names:?}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_restore_changes_nothing() {
    use std::os::unix::fs::PermissionsExt;
    if !have_node() {
        return;
    }
    // A runtime that exists when the snapshot is taken and is gone when it is restored.
    let wrapper = temp_path("node-wrapper.sh");
    std::fs::write(&wrapper, "#!/bin/sh\nexec node \"$@\"\n").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let s = start().await;
    let r = ctl_json(
        &s,
        "PUT",
        "/_fakellm/policies",
        &json!({"name": "counter", "source": COUNTER, "runtime": wrapper.to_str().unwrap()}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    put_rules(&s, json!([{"name": "pol", "reply": {"policy": "counter"}}])).await;
    assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "x")).await), "n=1");
    assert_eq!(snap(&s, "with-policy").await.status, 201);

    // Change everything else, remove the runtime, restore.
    put_rules(&s, json!([{"name": "after", "reply": {"text": "after"}}])).await;
    put_cassettes(&s, &cassette("later", vec![exchange("a", "A")])).await;
    std::fs::remove_file(&wrapper).unwrap();
    let rules_before = rules(&s).await;
    let cassettes_before = get(&s, "/_fakellm/cassettes").await.json();
    let policies_before = get(&s, "/_fakellm/policies").await.json()["policies"][0]["name"].clone();

    let r = restore(&s, "with-policy").await;
    assert!(r.status >= 400, "restore must fail without the runtime: {}", r.body);
    assert_eq!(rules(&s).await, rules_before);
    assert_eq!(get(&s, "/_fakellm/cassettes").await.json(), cassettes_before);
    assert_eq!(get(&s, "/_fakellm/policies").await.json()["policies"][0]["name"], policies_before);
    assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4o", "x")).await), "after");
}

#[tokio::test]
async fn snapshot_count_is_limited() {
    // The default is 100; a smaller limit keeps the test quick and proves it is configurable.
    let s = start_with(fakellm::Config {
        max_snapshots: 3,
        ..Default::default()
    })
    .await;
    for i in 0..3 {
        assert_eq!(snap(&s, &format!("s{i}")).await.status, 201);
    }
    let r = snap(&s, "s3").await;
    assert_eq!(r.status, 409, "{}", r.body);
    assert!(r.body.contains("too many snapshots"), "{}", r.body);
    send(&s, "DELETE", "/_fakellm/snapshots/s0", &[], "").await;
    assert_eq!(snap(&s, "s3").await.status, 201);
    assert_eq!(fakellm::MAX_SNAPSHOTS, 100);
    assert_eq!(fakellm::Config::default().max_snapshots, 100);
}
