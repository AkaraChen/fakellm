//! Compressed request bodies (`content-encoding`) are decoded before parsing and logging.

use std::io::Write;

use bytes::Bytes;
use fakellm::Server;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpStream;

mod common;
use common::*;

async fn post_raw(s: &Server, encoding: &str, body: Vec<u8>) -> (u16, Value) {
    let stream = TcpStream::connect(s.addr()).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(conn);
    let req = http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("host", s.addr().to_string())
        .header("content-type", "application/json")
        .header("content-encoding", encoding)
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn zstd(b: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(b, ruzstd::encoding::CompressionLevel::Fastest)
}

fn gzip(b: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

fn zlib(b: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

async fn scripted() -> Server {
    let s = start().await;
    put_rules(
        &s,
        json!([{"name": "enc", "match": {"last_user": "/compressed/"}, "reply": {"text": "scripted ok"}}]),
    )
    .await;
    s
}

async fn assert_decoded(s: &Server, encoding: &str, body: Vec<u8>) {
    let (status, j) = post_raw(s, encoding, body).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["choices"][0]["message"]["content"], "scripted ok");
    let rec = log(s, "").await.pop().unwrap();
    assert_eq!(rec["raw"]["messages"][0]["content"], "a compressed hello");
    assert_eq!(rec["request"]["messages"][0]["role"], "user");
    assert!(rec["headers"].to_string().contains(encoding));
}

fn body() -> Vec<u8> {
    chat_body("gpt-4o", "a compressed hello").to_string().into_bytes()
}

#[tokio::test]
async fn zstd_body() {
    let s = scripted().await;
    assert_decoded(&s, "zstd", zstd(&body())).await;
}

#[tokio::test]
async fn gzip_body() {
    let s = scripted().await;
    assert_decoded(&s, "gzip", gzip(&body())).await;
}

#[tokio::test]
async fn deflate_body() {
    let s = scripted().await;
    assert_decoded(&s, "deflate", zlib(&body())).await;
}

#[tokio::test]
async fn stacked_encodings_decode_last_first() {
    let s = scripted().await;
    assert_decoded(&s, "gzip, zstd", zstd(&gzip(&body()))).await;
}

#[tokio::test]
async fn unknown_encoding_is_415() {
    let s = start().await;
    let (status, j) = post_raw(&s, "snappy", body()).await;
    assert_eq!(status, 415);
    assert_eq!(j["error"]["type"], "invalid_request_error");
    assert!(j["error"]["message"].as_str().unwrap().contains("snappy"));
}

#[tokio::test]
async fn oversized_decompressed_body_is_413() {
    let s = start().await;
    let big = format!("{{\"model\":\"gpt-4o\",\"messages\":[],\"pad\":\"{}\"}}", "a".repeat(5 << 20));
    let packed = gzip(big.as_bytes());
    assert!(packed.len() < 1 << 20);
    let (status, j) = post_raw(&s, "gzip", packed).await;
    assert_eq!(status, 413, "{j}");
}

#[tokio::test]
async fn corrupt_body_is_400() {
    let s = start().await;
    let (status, _) = post_raw(&s, "zstd", b"not zstd".to_vec()).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn proxy_forwards_the_original_compressed_bytes() {
    let up = start().await;
    put_rules(
        &up,
        json!([{"name": "enc", "match": {"last_user": "/compressed/"}, "reply": {"text": "from upstream"}}]),
    )
    .await;
    // No local rule matches, so the proxy relays.
    let proxy = start_with(fakellm::Config {
        upstream: Some(up.base_url().parse().unwrap()),
        ..Default::default()
    })
    .await;
    let (status, j) = post_raw(&proxy, "gzip", gzip(&body())).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["choices"][0]["message"]["content"], "from upstream");
    let seen = log(&up, "").await.pop().unwrap();
    assert!(
        seen["headers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h[0] == "content-encoding" && h[1] == "gzip")
    );
    assert_eq!(seen["raw"]["messages"][0]["content"], "a compressed hello");
    let local = log(&proxy, "").await.pop().unwrap();
    assert_eq!(local["raw"]["messages"][0]["content"], "a compressed hello");
}
