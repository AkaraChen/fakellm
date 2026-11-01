//! M4 contract tests: configuration errors, control-plane routes, status fields and request-log filters.
//! Replay, upstream, snapshot and hot-reload behavior is tested in cassette.rs, upstream.rs, snapshots.rs and watch.rs.

use fakellm::{Config, Server};
use serde_json::json;

mod common;
use common::*;

#[tokio::test]
async fn status_reports_the_new_registries() {
    let s = start().await;
    let status = get(&s, "/_fakellm").await.json();
    assert_eq!(status["cassettes"]["count"], 0);
    assert_eq!(status["snapshots"]["count"], 0);
    assert_eq!(status["upstream"], json!(null));
}

#[tokio::test]
async fn cassette_routes_without_cassettes() {
    let s = start().await;
    assert_eq!(get(&s, "/_fakellm/cassettes").await.json(), json!({"cassettes": []}));
    assert_eq!(get(&s, "/_fakellm/cassettes/nope").await.status, 404);
    assert_eq!(get(&s, "/_fakellm/cassettes/recording").await.status, 404);
    assert_eq!(send(&s, "DELETE", "/_fakellm/cassettes/nope", &[], "").await.status, 404);
    assert_eq!(
        send(&s, "DELETE", "/_fakellm/cassettes", &[], "").await.json(),
        json!({"cleared": true})
    );
    assert_eq!(send(&s, "PATCH", "/_fakellm/cassettes", &[], "").await.status, 405);
}

#[tokio::test]
async fn snapshot_routes_without_snapshots() {
    let s = start().await;
    assert_eq!(get(&s, "/_fakellm/snapshots").await.json(), json!({"snapshots": []}));
    assert_eq!(get(&s, "/_fakellm/snapshots/nope").await.status, 404);
    assert_eq!(send(&s, "DELETE", "/_fakellm/snapshots/nope", &[], "").await.status, 404);
    assert_eq!(
        send(&s, "DELETE", "/_fakellm/snapshots", &[], "").await.json(),
        json!({"cleared": true})
    );
    assert_eq!(send(&s, "PUT", "/_fakellm/snapshots", &[], "").await.status, 405);
}

#[tokio::test]
async fn reset_accepts_the_snapshots_flag_and_the_log_the_new_sources() {
    let s = start().await;
    assert_eq!(send(&s, "POST", "/_fakellm/reset?snapshots=true", &[], "").await.status, 200);
    assert_eq!(send(&s, "POST", "/_fakellm/reset?snapshots=maybe", &[], "").await.status, 400);
    for source in ["cassette", "upstream", "script", "fallback"] {
        let r = get(&s, &format!("/_fakellm/requests?source={source}")).await;
        assert_eq!(r.status, 200, "{source}");
    }
    assert_eq!(get(&s, "/_fakellm/requests?source=nope").await.status, 400);
}

#[tokio::test]
async fn upstream_configuration_is_validated_at_start() {
    let record_only = Server::start(Config {
        record: Some(std::env::temp_dir().join("fakellm-contract-record.jsonl")),
        ..Config::default()
    })
    .await;
    assert_eq!(record_only.err().map(|e| e.kind()), Some(std::io::ErrorKind::InvalidInput));
    let bad_scheme = Server::start(Config {
        upstream: Some("ftp://example.com".parse().unwrap()),
        ..Config::default()
    })
    .await;
    assert_eq!(bad_scheme.err().map(|e| e.kind()), Some(std::io::ErrorKind::InvalidInput));
}

#[tokio::test]
async fn requests_without_cassettes_or_upstream_are_unchanged() {
    let s = start().await;
    let r = post_json(&s, "/v1/chat/completions", &chat_body("gpt-4o", "hello")).await;
    assert_eq!(r.status, 200);
    let log = get(&s, "/_fakellm/requests?source=fallback").await.json();
    assert_eq!(log["total"], 1);
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test]
async fn an_upstream_that_is_the_servers_own_address_is_refused_at_start() {
    let port = free_port();
    for host in ["127.0.0.1", "localhost"] {
        let r = Server::start(Config {
            host: "127.0.0.1".into(),
            port,
            upstream: Some(format!("http://{host}:{port}/v1").parse().unwrap()),
            ..Config::default()
        })
        .await;
        let err = r.err().unwrap_or_else(|| panic!("{host} was accepted"));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{host}");
        assert!(err.to_string().contains("own address"), "{err}");
    }
}
