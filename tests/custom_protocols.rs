//! Custom endpoint rules next to the Responses and Anthropic Messages routes: the same server answers
//! `POST /v1/systemone` from a rule and the standard routes from theirs, with each protocol's headers,
//! auth, path prefixes and log fields.

use serde_json::json;

mod common;
use common::*;
#[path = "common/protocols.rs"]
mod protocols;
use protocols::*;

fn custom_rule() -> serde_json::Value {
    json!({"name": "decide", "match": {"path": "/v1/systemone", "body": "angry"},
           "reply": {"body": {"answers": {"damaged": {"type": "noul", "noul": 0.9}}}}})
}

#[tokio::test]
async fn standard_routes_ignore_custom_rules_on_every_protocol() {
    let s = start().await;
    // Even a custom rule on a standard path, or one that matches everything, only sees custom requests.
    put_rules(
        &s,
        json!([
            {"match": {"path": "/v1/messages", "body": "."}, "reply": {"body": {"custom": true}}},
            {"match": {"path": "/v1/responses", "body": "."}, "reply": {"body": {"custom": true}}},
            {"match": {"path": "/v1/chat/completions", "body": "."}, "reply": {"body": {"custom": true}}},
            custom_rule(),
            {"match": {"last_user": "ping"}, "reply": {"text": "pong"}},
        ]),
    )
    .await;
    for p in ALL {
        let r = p.send(&s, "ping", &[], &[]).await;
        assert_eq!(p.text(&r), "pong", "{p:?}: {}", r.body);
        assert!(!r.body.contains("\"custom\""), "{p:?}: {}", r.body);
    }
    let r = post_json(&s, "/v1/systemone", &json!({"state": "angry"})).await;
    assert_eq!(r.json()["answers"]["damaged"]["noul"], 0.9);
}

#[tokio::test]
async fn custom_requests_with_anthropic_and_openai_headers() {
    let s = start().await;
    put_rules(&s, json!([custom_rule(), {"reply": {"text": "fallback"}}])).await;
    let body = json!({"state": "angry customer"}).to_string();
    for headers in [
        vec![
            ("content-type", "application/json"),
            ("anthropic-version", "2023-06-01"),
            ("x-api-key", "k"),
        ],
        vec![("content-type", "application/json"), ("authorization", "Bearer sk-x")],
        vec![("content-type", "application/json")],
    ] {
        let r = send(&s, "POST", "/v1/systemone", &headers, &body).await;
        assert_eq!(r.status, 200, "{headers:?}: {}", r.body);
        assert_eq!(r.json()["answers"]["damaged"]["noul"], 0.9, "{headers:?}");
    }
    let log = get(&s, "/_fakellm/requests?protocol=custom").await.json();
    assert_eq!(log["requests"].as_array().unwrap().len(), 3);
}

/// Current behaviour: the API key check lives in the standard routes, so a custom endpoint answers without it
/// (the standard routes next to it still reject a wrong key). Written down so a change is deliberate.
#[tokio::test]
async fn custom_endpoints_are_not_behind_the_api_key() {
    let s = start_with(fakellm::Config {
        api_key: Some("sk-right".into()),
        ..Default::default()
    })
    .await;
    put_rules(&s, json!([custom_rule(), {"reply": {"text": "ok"}}])).await;
    let body = json!({"state": "angry"}).to_string();
    for key in [("x-api-key", "nope"), ("authorization", "Bearer sk-right")] {
        let r = send(&s, "POST", "/v1/systemone", &[("content-type", "application/json"), key], &body).await;
        assert_eq!(r.status, 200, "{key:?}: {}", r.body);
    }
    for p in ALL {
        let headers: Vec<_> = p
            .headers()
            .iter()
            .copied()
            .chain([("x-api-key", "nope"), ("content-type", "application/json")])
            .collect();
        let r = send(&s, "POST", p.path(), &headers, &p.body("hi", &[], &[]).to_string()).await;
        assert_eq!(r.status, 401, "{p:?}: {}", r.body);
    }
}

#[tokio::test]
async fn custom_paths_behind_gateway_prefixes() {
    let s = start_with(fakellm::Config {
        path_prefix: Some("/anthropic".into()),
        ..Default::default()
    })
    .await;
    put_rules(
        &s,
        json!([
            {"match": {"path": "/v1/systemone", "body": "angry"}, "reply": {"body": {"via": "rule"}}},
            {"match": {"path": "/foo/v1/other"}, "reply": {"body": {"via": "literal"}}},
        ]),
    )
    .await;
    // The configured prefix is removed before custom rules are matched, so one rule serves both spellings.
    for path in ["/v1/systemone", "/anthropic/v1/systemone"] {
        let r = post_json(&s, path, &json!({"state": "angry"})).await;
        assert_eq!(r.status, 200, "{path}: {}", r.body);
        assert_eq!(r.json()["via"], "rule", "{path}");
    }
    // Other leading segments are only removed in front of the standard route suffixes: this stays literal.
    let r = post_json(&s, "/foo/v1/other", &json!({})).await;
    assert_eq!(r.json()["via"], "literal");
    // The log keeps the full path.
    let log = log(&s, "protocol=custom").await;
    assert_eq!(log[0]["path"], "/v1/systemone");
    assert_eq!(log[1]["path"], "/anthropic/v1/systemone");
    // The standard routes keep working under the same prefix, next to the custom ones.
    let r = post_json_with(
        &s,
        "/anthropic/v1/messages",
        P::Anthropic.headers(),
        &P::Anthropic.body("hi", &[], &[]),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = post_json(&s, "/anthropic/v1/responses", &P::Responses.body("hi", &[], &[])).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn custom_and_standard_requests_interleave_in_one_log() {
    let s = start().await;
    put_rules(&s, json!([custom_rule(), {"name": "text", "reply": {"text": "ok"}}])).await;
    for p in ALL {
        p.send(&s, "hello", &[], &[]).await;
        post_json(&s, "/v1/systemone", &json!({"state": "angry"})).await;
    }
    let all = log(&s, "").await;
    assert_eq!(all.len(), 6);
    let custom = log(&s, "protocol=custom").await;
    assert_eq!(custom.len(), 3);
    for protocol in ["openai.chat", "openai.responses", "anthropic.messages"] {
        let n = log(&s, &format!("protocol={protocol}")).await.len();
        assert_eq!(n, 1, "{protocol}");
    }
    // Rule hits are counted separately.
    let rules = get(&s, "/_fakellm/scripts").await.json();
    assert_eq!(rules["rules"][0]["hits"], 3);
    assert_eq!(rules["rules"][1]["hits"], 3);
}

#[tokio::test]
async fn times_spends_a_custom_rule_and_the_next_one_answers() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"match": {"path": "/v1/systemone", "body": "once"}, "times": 1, "reply": {"body": {"n": 1}}},
            {"match": {"path": "/v1/systemone"}, "reply": {"status": 503, "body": {"error": "down"}}},
        ]),
    )
    .await;
    let first = post_json(&s, "/v1/systemone", &json!({"state": "once"})).await;
    assert_eq!((first.status, first.json()["n"].as_u64()), (200, Some(1)));
    let second = post_json(&s, "/v1/systemone", &json!({"state": "once"})).await;
    assert_eq!(second.status, 503, "times: 1 is spent, the next rule answers");
    assert_eq!(second.json()["error"], "down");
}
