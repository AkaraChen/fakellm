//! `/v1/embeddings` over raw HTTP: deterministic vectors, `dimensions`, base64, validation errors, `error` rules and the request log.

use serde_json::{Value, json};

mod common;
use common::*;

const MODEL: &str = "text-embedding-3-small";

async fn embed(s: &fakellm::Server, body: Value) -> Reply {
    post_json(s, "/v1/embeddings", &body).await
}

fn vectors(r: &Reply) -> Vec<Vec<f64>> {
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["embedding"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect())
        .collect()
}

fn base64_decode(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("not base64: {}", c as char),
    };
    assert_eq!(s.len() % 4, 0, "standard base64 is padded");
    let mut out = Vec::new();
    for chunk in s.as_bytes().chunks(4) {
        let pad = chunk.iter().filter(|c| **c == b'=').count();
        let n = chunk.iter().take(4 - pad).fold(0u32, |acc, c| (acc << 6) | val(*c) as u32) << (6 * pad);
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..4 - pad]);
    }
    out
}

fn norm(v: &[f64]) -> f64 {
    v.iter().map(|x| x * x).sum::<f64>().sqrt()
}

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>() / (norm(a) * norm(b))
}

#[tokio::test]
async fn response_shape() {
    let s = start().await;
    let r = embed(&s, json!({"model": MODEL, "input": "hello world"})).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.header("x-request-id").is_some());
    let v = r.json();
    assert_eq!(v["object"], "list");
    assert_eq!(v["model"], MODEL);
    assert_eq!(v["data"][0]["object"], "embedding");
    assert_eq!(v["data"][0]["index"], 0);
    let vec = &vectors(&r)[0];
    assert_eq!(vec.len(), 1536);
    assert!((norm(vec) - 1.0).abs() < 1e-4, "unit length, got {}", norm(vec));
    assert!(vec.iter().all(|x| (-1.0..=1.0).contains(x)));
}

#[tokio::test]
async fn deterministic_and_distinct() {
    let s = start().await;
    let a = vectors(&embed(&s, json!({"model": MODEL, "input": "alpha"})).await).remove(0);
    let a2 = vectors(&embed(&s, json!({"model": MODEL, "input": "alpha"})).await).remove(0);
    let b = vectors(&embed(&s, json!({"model": MODEL, "input": "beta"})).await).remove(0);
    let other_model = vectors(&embed(&s, json!({"model": "text-embedding-3-large", "input": "alpha"})).await).remove(0);
    assert_eq!(a, a2);
    assert_ne!(a, b);
    assert_ne!(a, other_model);
    assert!(
        cosine(&a, &b).abs() < 0.3,
        "distinct inputs are not near-duplicates: {}",
        cosine(&a, &b)
    );

    let fresh = start().await;
    assert_eq!(
        a,
        vectors(&embed(&fresh, json!({"model": MODEL, "input": "alpha"})).await).remove(0),
        "stable across servers"
    );
}

#[tokio::test]
async fn dimensions_is_a_renormalized_prefix() {
    let s = start().await;
    let full = vectors(&embed(&s, json!({"model": MODEL, "input": "prefix", "dimensions": 64})).await).remove(0);
    let short = vectors(&embed(&s, json!({"model": MODEL, "input": "prefix", "dimensions": 16})).await).remove(0);
    assert_eq!((full.len(), short.len()), (64, 16));
    assert!((norm(&short) - 1.0).abs() < 1e-4);
    let scale = norm(&full[..16]);
    for (s, f) in short.iter().zip(&full) {
        assert!((s - f / scale).abs() < 1e-4, "{s} vs {}", f / scale);
    }
}

#[tokio::test]
async fn base64_equals_float_vector() {
    let s = start().await;
    let floats = vectors(&embed(&s, json!({"model": MODEL, "input": "encode me", "dimensions": 32})).await).remove(0);
    let r = embed(
        &s,
        json!({"model": MODEL, "input": "encode me", "dimensions": 32, "encoding_format": "base64"}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let encoded = r.json()["data"][0]["embedding"].as_str().unwrap().to_owned();
    let bytes = base64_decode(&encoded);
    assert_eq!(bytes.len(), 32 * 4);
    let decoded: Vec<f64> = bytes.chunks(4).map(|c| f32::from_le_bytes(c.try_into().unwrap()) as f64).collect();
    for (d, f) in decoded.iter().zip(&floats) {
        assert!((d - f).abs() < 1e-6, "{d} vs {f}");
    }
}

#[tokio::test]
async fn batches_and_token_arrays() {
    let s = start().await;
    let r = embed(&s, json!({"model": MODEL, "input": ["one", "two", "three"], "dimensions": 8})).await;
    let vs = vectors(&r);
    assert_eq!(vs.len(), 3);
    assert!(vs.iter().all(|v| v.len() == 8));
    let idx: Vec<_> = r.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["index"].as_u64().unwrap())
        .collect();
    assert_eq!(idx, [0, 1, 2]);
    let single = vectors(&embed(&s, json!({"model": MODEL, "input": "two", "dimensions": 8})).await).remove(0);
    assert_eq!(vs[1], single, "a batch element equals the same input alone");

    let r = embed(&s, json!({"model": MODEL, "input": [1, 2, 3, 4], "dimensions": 8})).await;
    assert_eq!(vectors(&r).len(), 1);
    assert_eq!(r.json()["usage"]["prompt_tokens"], 4, "token arrays count their length");
    let r = embed(&s, json!({"model": MODEL, "input": [[1, 2], [3, 4, 5]], "dimensions": 8})).await;
    assert_eq!(vectors(&r).len(), 2);
    assert_eq!(r.json()["usage"]["prompt_tokens"], 5);
}

#[tokio::test]
async fn usage_invariants() {
    let s = start().await;
    let u = embed(&s, json!({"model": MODEL, "input": "a short sentence", "dimensions": 4}))
        .await
        .json()["usage"]
        .clone();
    assert!(u["prompt_tokens"].as_u64().unwrap() > 0);
    assert!(u["total_tokens"].as_u64().unwrap() >= u["prompt_tokens"].as_u64().unwrap());
    assert_eq!(u["total_tokens"], u["prompt_tokens"]);
    let batch = embed(
        &s,
        json!({"model": MODEL, "input": ["a short sentence", "a short sentence"], "dimensions": 4}),
    )
    .await
    .json()["usage"]
        .clone();
    assert_eq!(batch["prompt_tokens"].as_u64().unwrap(), 2 * u["prompt_tokens"].as_u64().unwrap());
}

#[tokio::test]
async fn validation_errors() {
    let s = start().await;
    let invalid = "'$.input' is invalid. Please check the API reference: https://platform.openai.com/docs/api-reference.";
    let cases = [
        (json!({"input": "x"}), "model"),
        (json!({"model": MODEL}), "input"),
        (json!({"model": MODEL, "input": ""}), "input"),
        (json!({"model": MODEL, "input": []}), "input"),
        (json!({"model": MODEL, "input": [1, "a"]}), "input"),
        (json!({"model": MODEL, "input": {"a": 1}}), "input"),
        (json!({"model": MODEL, "input": ["ok", ""]}), "input"),
        (json!({"model": MODEL, "input": vec!["x"; 2049]}), "input"),
        (json!({"model": MODEL, "input": "x", "dimensions": 0}), "dimensions"),
        (json!({"model": MODEL, "input": "x", "dimensions": -3}), "dimensions"),
        (json!({"model": MODEL, "input": "x", "dimensions": 8193}), "dimensions"),
        (json!({"model": MODEL, "input": "x", "dimensions": "big"}), "dimensions"),
        (json!({"model": MODEL, "input": "x", "encoding_format": "hex"}), "encoding_format"),
    ];
    for (body, param) in cases {
        let short = body.to_string().chars().take(80).collect::<String>();
        let r = embed(&s, body).await;
        assert_eq!(r.status, 400, "{short}: {}", r.body);
        let e = r.json()["error"].clone();
        assert_eq!(e["type"], "invalid_request_error", "{short}");
        assert_eq!(e["param"], param, "{short}");
        if param == "input" && short.contains("\"input\":") {
            let m = e["message"].as_str().unwrap();
            assert!(m == invalid || m.contains("input"), "{short}: {m}");
        }
    }
    let r = send(&s, "POST", "/v1/embeddings", &[("content-type", "application/json")], "{nope").await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn error_rules_apply_with_times() {
    let s = start().await;
    put_rules(
        &s,
        json!([
            {"reply": {"text": "ignored on this route"}},
            {"times": 2, "reply": {"error": {"status": 429, "message": "slow down", "retry_after": 1}}}
        ]),
    )
    .await;
    for _ in 0..2 {
        let r = embed(&s, json!({"model": MODEL, "input": "x"})).await;
        assert_eq!(r.status, 429, "{}", r.body);
        assert_eq!(r.json()["error"]["message"], "slow down");
    }
    let r = embed(&s, json!({"model": MODEL, "input": "x"})).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let rules = get(&s, "/_fakellm/scripts").await.json();
    assert_eq!(rules["rules"][0]["hits"], 0, "rules without error keep their budget");
    assert_eq!(rules["rules"][1]["hits"], 2);
}

#[tokio::test]
async fn request_log_records_inputs_not_vectors() {
    let s = start().await;
    embed(
        &s,
        json!({"model": MODEL, "input": ["first text", "second text"], "dimensions": 8, "encoding_format": "float"}),
    )
    .await;
    post_json(&s, "/v1/chat/completions", &chat_body("gpt-4o", "chat")).await;
    let log = get(&s, "/_fakellm/requests?path=/v1/embeddings").await.json();
    let reqs = log["requests"].as_array().unwrap();
    assert_eq!(reqs.len(), 1, "{log}");
    let r = &reqs[0];
    assert_eq!(r["request"]["model"], MODEL);
    assert_eq!(r["protocol"], "openai.embeddings");
    assert_eq!(r["request"]["messages"].as_array().unwrap().len(), 2);
    assert_eq!(r["request"]["extra"]["dimensions"], 8);
    assert_eq!(r["source"]["kind"], "fallback");
    assert!(r["reply"]["usage"]["prompt_tokens"].as_u64().unwrap() > 0, "{r}");
    assert!(!r.to_string().contains("\"embedding\""), "vectors are not logged");
}

#[tokio::test]
async fn models_api_key_and_paths() {
    let s = start().await;
    let models = get(&s, "/v1/models").await.json();
    let ids: Vec<_> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        ids.contains(&"text-embedding-3-small".to_owned()) && ids.contains(&"text-embedding-3-large".to_owned()),
        "{ids:?}"
    );

    let body = json!({"model": MODEL, "input": "x", "dimensions": 4});
    for path in ["/embeddings", "/openai/v1/embeddings", "/v1/embeddings"] {
        let r = post_json(&s, path, &body).await;
        assert_eq!(r.status, 200, "{path}: {}", r.body);
    }

    let s = start_with(fakellm::Config {
        api_key: Some("sk-secret".into()),
        ..Default::default()
    })
    .await;
    assert_eq!(embed(&s, body.clone()).await.status, 401);
    let r = post_json_with(&s, "/v1/embeddings", &[("authorization", "Bearer sk-secret")], &body).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn request_log_protocol_filter() {
    let s = start().await;
    embed(&s, json!({"model": MODEL, "input": "x", "dimensions": 4})).await;
    post_json(&s, "/v1/chat/completions", &chat_body("gpt-4o", "chat")).await;
    let log = get(&s, "/_fakellm/requests?protocol=openai.embeddings").await;
    assert_eq!(log.status, 200, "{}", log.body);
    assert_eq!(log.json()["requests"].as_array().unwrap().len(), 1);
}
