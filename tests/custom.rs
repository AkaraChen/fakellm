//! Custom endpoint rules over raw HTTP: `match.path` + `reply.body` answer paths no standard route
//! serves (xsai's `POST /v1/systemone`), matched on the raw body; everything else stays a 404.

use serde_json::json;

mod common;
use common::*;

async fn rules(s: &fakellm::Server) {
    put_rules(
        s,
        json!([
            {"name": "angry", "match": {"path": "/v1/systemone", "body": "angry"},
             "reply": {"body": {"answers": {"damaged": {"type": "noul", "noul": 0.9}}, "usage": {"input_tokens": 5, "output_tokens": 1}}}},
            {"name": "happy", "match": {"path": "/v1/system*", "body": "/HAPPY/i"},
             "reply": {"status": 202, "headers": {"x-custom": "yes"}, "body": {"answers": {"damaged": {"type": "noul", "noul": 0.1}}}}},
        ]),
    )
    .await;
}

#[tokio::test]
async fn body_regexes_pick_the_rule() {
    let s = start().await;
    rules(&s).await;
    let a = post_json(&s, "/v1/systemone", &json!({"state": "an angry customer"})).await;
    assert_eq!(a.status, 200, "{}", a.body);
    assert_eq!(a.header("content-type"), Some("application/json"));
    assert_eq!(a.json()["answers"]["damaged"]["noul"], 0.9);
    assert_eq!(a.json()["usage"]["input_tokens"], 5);
    let h = post_json(&s, "/v1/systemone", &json!({"state": "so Happy"})).await;
    assert_eq!(h.json()["answers"]["damaged"]["noul"], 0.1);
}

#[tokio::test]
async fn status_and_headers() {
    let s = start().await;
    rules(&s).await;
    let h = post_json(&s, "/v1/systemone", &json!({"state": "happy"})).await;
    assert_eq!(h.status, 202);
    assert_eq!(h.header("x-custom"), Some("yes"));
}

#[tokio::test]
async fn unmatched_stays_404() {
    let s = start().await;
    rules(&s).await;
    let r = post_json(&s, "/v1/systemone", &json!({"state": "neutral"})).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["message"], "Invalid URL (POST /v1/systemone)");
    let r = post_json(&s, "/v1/other", &json!({"state": "angry"})).await;
    assert_eq!(r.status, 404);
    // Custom rules never touch the standard routes.
    let c = chat(&s, &chat_body("m", "angry")).await;
    assert_eq!(c.status, 200);
    assert!(!c.body.contains("noul"));
}

#[tokio::test]
async fn logged_as_custom() {
    let s = start().await;
    rules(&s).await;
    post_json(&s, "/v1/systemone", &json!({"state": "angry"})).await;
    let log = get(&s, "/_fakellm/requests?full=true&protocol=custom").await.json();
    let e = &log["requests"][0];
    assert_eq!(e["protocol"], "custom");
    assert_eq!(e["path"], "/v1/systemone");
    assert_eq!(e["status"], 200);
    assert_eq!(e["raw"]["state"], "angry");
    assert_eq!(e["rule"], "angry");
    assert_eq!(e["source"]["kind"], "script");
    // Hit counts show up on the rule.
    let rules = get(&s, "/_fakellm/scripts").await.json();
    assert_eq!(rules["rules"][0]["hits"], 1);
}

#[tokio::test]
async fn invalid_rules_are_rejected() {
    let s = start().await;
    for bad in [
        json!({"match": {"path": "/x"}, "reply": {"text": "hi"}}),
        json!({"match": {}, "reply": {"text": "hi", "status": 201}}),
        json!({"match": {"path": "/x"}, "reply": {"body": {}, "status": 99}}),
    ] {
        let r = send(
            &s,
            "PUT",
            "/_fakellm/scripts",
            &[("content-type", "application/json")],
            &json!([bad]).to_string(),
        )
        .await;
        assert_eq!(r.status, 400, "{}", r.body);
    }
}
