//! The in-process runtime keeps one OS thread per policy; stopping a policy (replace, delete, reset, snapshot restore, shutdown) must
//! join it. One test on purpose: it counts the threads of the whole test process (`Threads:` in /proc/self/status), so nothing else may
//! run next to it. Linux only.
#![cfg(all(feature = "tjs", target_os = "linux"))]

use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod common;
use common::*;

fn threads() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("Threads:"))
        .and_then(|n| n.trim().parse().ok())
        .expect("Threads: in /proc/self/status")
}

/// Waits (up to two seconds) for the thread count to reach `want`, then asserts it.
async fn expect_threads(want: usize, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while threads() != want && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(threads(), want, "{what}");
}

fn specs(names: &[&str]) -> Value {
    Value::Array(
        names
            .iter()
            .map(|n| json!({"name": n, "source": "export default () => ({ text: 'x' });"}))
            .collect(),
    )
}

async fn put(s: &fakellm::Server, names: &[&str]) {
    let r = send(
        s,
        "PUT",
        "/_fakellm/policies",
        &[("content-type", "application/json")],
        &specs(names).to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn policy_threads_are_joined_when_policies_stop() {
    let base = threads();
    let s = start().await;
    expect_threads(base, "a server without policies adds no threads").await;

    put(&s, &["a", "b", "c", "d", "e"]).await;
    expect_threads(base + 5, "one thread per policy").await;

    put(&s, &["a", "b"]).await;
    expect_threads(base + 2, "PUT replaces: the old threads are joined").await;

    let r = send(
        &s,
        "POST",
        "/_fakellm/policies",
        &[("content-type", "application/json")],
        &specs(&["a"]).to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    expect_threads(base + 2, "POST of an existing name swaps its thread").await;

    assert_eq!(send(&s, "DELETE", "/_fakellm/policies/a", &[], "").await.status, 200);
    expect_threads(base + 1, "DELETE joins the thread").await;

    put(&s, &["a", "b", "c"]).await;
    let snapshot = ctl_json(&s, "POST", "/_fakellm/snapshots", &json!({"name": "three"})).await;
    assert_eq!(snapshot.status, 201, "{}", snapshot.body);
    put(&s, &["z"]).await;
    expect_threads(base + 1, "replaced again").await;
    let r = send(&s, "POST", "/_fakellm/snapshots/three/restore", &[], "").await;
    assert_eq!(r.status, 200, "{}", r.body);
    expect_threads(base + 3, "a restore starts the snapshot's policies and joins the others").await;

    assert_eq!(post_json(&s, "/_fakellm/reset", &json!({})).await.status, 200);
    expect_threads(base, "reset joins every policy thread").await;

    // Shutdown with a decision in flight: it is interrupted, not waited for.
    let r = send(
        &s,
        "PUT",
        "/_fakellm/policies",
        &[("content-type", "application/json")],
        &json!({"name": "sleeper", "source": "export default async () => { await new Promise((r) => setTimeout(r, 60000)); return { text: 'late' }; };"})
            .to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    put_rules(&s, json!([{"reply": {"policy": "sleeper"}}])).await;
    let body = chat_body("gpt-4o", "hi").to_string();
    let mut client = tokio::net::TcpStream::connect(s.addr()).await.unwrap();
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    tokio::io::AsyncWriteExt::write_all(&mut client, request.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    expect_threads(base + 1, "the sleeper's thread").await;
    let started = Instant::now();
    s.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(2), "shutdown took {:?}", started.elapsed());
    expect_threads(base, "shutdown joins the policy threads").await;
    drop(client);

    // Dropping a server joins them too.
    let s = start().await;
    put(&s, &["a", "b"]).await;
    expect_threads(base + 2, "two again").await;
    drop(s);
    expect_threads(base, "dropping the server joins the policy threads").await;
}
