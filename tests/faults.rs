//! Fault matchers over HTTP: probability, every_nth, hang released by the virtual clock, hang
//! logging and error headers.

use std::time::Duration;

use serde_json::json;

mod common;
use common::*;

async fn status(s: &fakellm::Server) -> u16 {
    chat(s, &chat_body("gpt-4o", "hi")).await.status
}

#[tokio::test]
async fn probability_one_and_zero() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"match": {"probability": 1.0}, "reply": {"error": {"status": 500}}}, {"reply": {"text": "ok"}}]),
    )
    .await;
    for _ in 0..5 {
        assert_eq!(status(&s).await, 500);
    }
    put_rules(
        &s,
        json!([{"match": {"probability": 0.0}, "reply": {"error": {"status": 500}}}, {"reply": {"text": "ok"}}]),
    )
    .await;
    for _ in 0..5 {
        assert_eq!(status(&s).await, 200);
    }
}

#[tokio::test]
async fn every_nth_two() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"match": {"every_nth": 2}, "reply": {"error": {"status": 503}}}, {"reply": {"text": "ok"}}]),
    )
    .await;
    let got = [status(&s).await, status(&s).await, status(&s).await, status(&s).await];
    assert_eq!(got, [200, 503, 200, 503]);
}

#[tokio::test]
async fn hang_ms_released_by_clock_advance() {
    let s = start().await;
    s.clock().freeze(None).unwrap();
    put_rules(&s, json!([{"reply": {"text": "late", "hang_ms": 1000}}])).await;
    let body = chat_body("gpt-4o", "hi");
    let req = chat(&s, &body);
    tokio::pin!(req);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut req).await.is_err(),
        "answered before the clock moved"
    );
    let by = json!({"by": "1s"});
    let adv = post_json(&s, "/_fakellm/clock/advance", &by);
    let (r, a) = tokio::join!(req, adv);
    assert_eq!(a.status, 200);
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["choices"][0]["message"]["content"], "late");
}

#[tokio::test]
async fn hung_request_is_logged_with_status_zero_on_disconnect() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"hang": true}}])).await;
    let timed_out = tokio::time::timeout(Duration::from_millis(300), chat(&s, &chat_body("gpt-4o", "hi"))).await;
    assert!(timed_out.is_err());
    let mut log = serde_json::Value::Null;
    for _ in 0..50 {
        log = get(&s, "/_fakellm/requests?full=true").await.json();
        if !log["requests"].as_array().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let rec = &log["requests"][0];
    assert_eq!(rec["status"], 0, "{log}");
    assert!(rec["request"].is_object());
    assert_eq!(rec["reply"]["hang"], json!({"until_ms": null}), "{rec}");
}

#[tokio::test]
async fn error_headers_reach_the_response() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"reply": {"error": {"status": 429, "headers": {"X-Should-Retry": "true", "retry-after-ms": "1500"}}}}]),
    )
    .await;
    let r = chat(&s, &chat_body("gpt-4o", "hi")).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.header("x-should-retry"), Some("true"));
    assert_eq!(r.header("retry-after-ms"), Some("1500"));
}

/// Raw TCP reader: what a client sees when the connection is reset mid-stream.
async fn raw_post(s: &fakellm::Server, body: &serde_json::Value) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(s.addr()).await.unwrap();
    let b = body.to_string();
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nhost: x\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{b}",
        b.len()
    );
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut out = vec![];
    // EOF or an error, whichever the abort produces; both end the read.
    let _ = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut out))
        .await
        .expect("connection stayed open");
    out
}

#[tokio::test]
async fn reset_after_chunks_has_no_terminating_chunk() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"reply": {"text": "one two three four", "stream": {"reset_after_chunks": 2}}}]),
    )
    .await;
    let mut body = chat_body("gpt-4o", "hi");
    body["stream"] = json!(true);
    let raw = String::from_utf8_lossy(&raw_post(&s, &body).await).into_owned();
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
    assert!(raw.contains("one"), "{raw}");
    assert!(!raw.contains("[DONE]") && !raw.contains("finish_reason\":\"stop"), "{raw}");
    assert!(!raw.ends_with("0\r\n\r\n"), "clean chunked end: {raw:?}");

    // The clean cut, for contrast, does end with the terminating chunk.
    put_rules(
        &s,
        json!([{"reply": {"text": "one two three four", "stream": {"cut_after_chunks": 2}}}]),
    )
    .await;
    let raw = String::from_utf8_lossy(&raw_post(&s, &body).await).into_owned();
    assert!(raw.ends_with("0\r\n\r\n"), "{raw:?}");
}

#[tokio::test]
async fn reset_true_also_aborts_streams() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "one two three four", "reset": true}}])).await;
    let mut body = chat_body("gpt-4o", "hi");
    body["stream"] = json!(true);
    let raw = String::from_utf8_lossy(&raw_post(&s, &body).await).into_owned();
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
    assert!(!raw.contains("[DONE]") && !raw.ends_with("0\r\n\r\n"), "{raw:?}");
}

#[tokio::test]
async fn reset_streams_are_still_logged() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"reply": {"text": "one two three four", "stream": {"reset_after_chunks": 1}}}]),
    )
    .await;
    let mut body = chat_body("gpt-4o", "hi");
    body["stream"] = json!(true);
    raw_post(&s, &body).await;
    let log = get(&s, "/_fakellm/requests").await.json();
    assert_eq!(log["requests"].as_array().unwrap().len(), 1, "{log}");
}

#[tokio::test]
async fn sync_reset_sends_headers_then_dies() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "hello there", "reset": true}}])).await;
    let mut sock = tokio::net::TcpStream::connect(s.addr()).await.unwrap();
    let b = chat_body("gpt-4o", "hi").to_string();
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nhost: x\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{b}",
        b.len()
    );
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut out = vec![];
    let _ = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut out))
        .await
        .expect("connection stayed open");
    let raw = String::from_utf8_lossy(&out).into_owned();
    let (head, rest) = raw.split_once("\r\n\r\n").unwrap_or_else(|| panic!("no headers: {raw:?}"));
    let declared: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .map(|v| v.trim().parse().unwrap())
        })
        .expect("content-length");
    assert!(head.starts_with("HTTP/1.1 200") && rest.len() < declared, "{raw:?}");
}
