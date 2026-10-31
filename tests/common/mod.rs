//! Minimal hyper client for end-to-end tests: one connection per request.
#![allow(dead_code)]

use std::time::Duration;

use bytes::Bytes;
use fakellm::{Config, Server};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpStream;

pub struct Reply {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: String,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

pub async fn start() -> Server {
    Server::start(Config::default()).await.unwrap()
}

pub async fn start_with(config: Config) -> Server {
    Server::start(config).await.unwrap()
}

async fn request(s: &Server, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> http::Response<Incoming> {
    let stream = TcpStream::connect(s.addr()).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(conn);
    let mut req = http::Request::builder()
        .method(method)
        .uri(path)
        .header("host", s.addr().to_string());
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    sender
        .send_request(req.body(Full::new(Bytes::from(body.to_owned()))).unwrap())
        .await
        .unwrap()
}

pub async fn send(s: &Server, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let resp = request(s, method, path, headers, body).await;
    let (parts, body) = resp.into_parts();
    let body = String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap();
    Reply {
        status: parts.status.as_u16(),
        headers: parts.headers,
        body,
    }
}

pub async fn get(s: &Server, path: &str) -> Reply {
    send(s, "GET", path, &[], "").await
}

pub async fn post_json(s: &Server, path: &str, body: &Value) -> Reply {
    send(s, "POST", path, &[("content-type", "application/json")], &body.to_string()).await
}

pub async fn put_rules(s: &Server, rules: Value) {
    let r = send(
        s,
        "PUT",
        "/_fakellm/scripts",
        &[("content-type", "application/json")],
        &rules.to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
}

pub fn chat_body(model: &str, user: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": user}]})
}

pub async fn chat(s: &Server, body: &Value) -> Reply {
    post_json(s, "/v1/chat/completions", body).await
}

/// A streaming response whose SSE frames are pulled one at a time.
pub struct Sse {
    pub status: u16,
    pub headers: http::HeaderMap,
    body: Incoming,
    buf: String,
}

pub async fn open_stream(s: &Server, body: &Value) -> Sse {
    open_sse(s, "/v1/chat/completions", &[], body).await
}

/// Opens a streaming POST on any generation endpoint (`/v1/messages`, `/v1/responses`, ...).
/// `headers` are added to `content-type: application/json`.
pub async fn open_sse(s: &Server, path: &str, headers: &[(&str, &str)], body: &Value) -> Sse {
    let mut all = vec![("content-type", "application/json")];
    all.extend_from_slice(headers);
    let resp = request(s, "POST", path, &all, &body.to_string()).await;
    let (parts, body) = resp.into_parts();
    Sse {
        status: parts.status.as_u16(),
        headers: parts.headers,
        body,
        buf: String::new(),
    }
}

/// `post_json` with extra request headers (`anthropic-version`, `x-api-key`, ...).
pub async fn post_json_with(s: &Server, path: &str, headers: &[(&str, &str)], body: &Value) -> Reply {
    let mut all = vec![("content-type", "application/json")];
    all.extend_from_slice(headers);
    send(s, "POST", path, &all, &body.to_string()).await
}

/// One SSE event: the `event:` name (if any) and the joined `data:` lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.data).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.data))
    }
}

impl Sse {
    /// Next `data:` payload, or None at end of body.
    pub async fn next(&mut self) -> Option<String> {
        loop {
            if let Some(i) = self.buf.find("\n\n") {
                let frame: String = self.buf.drain(..i + 2).collect();
                return Some(frame.trim_end().strip_prefix("data: ").unwrap_or(frame.trim_end()).to_owned());
            }
            match self.body.frame().await {
                Some(Ok(f)) => {
                    if let Ok(d) = f.into_data() {
                        self.buf.push_str(std::str::from_utf8(&d).unwrap());
                    }
                }
                Some(Err(e)) => panic!("body error: {e}"),
                None => return None,
            }
        }
    }

    /// Next event with its `event:` name, or None at end of body.
    pub async fn next_event(&mut self) -> Option<SseEvent> {
        loop {
            if let Some(i) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..i + 2).collect();
                let mut event = None;
                let mut data = Vec::new();
                for line in block.lines() {
                    if let Some(e) = line.strip_prefix("event: ") {
                        event = Some(e.to_owned());
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data.push(d);
                    }
                }
                return Some(SseEvent {
                    event,
                    data: data.join("\n"),
                });
            }
            match self.body.frame().await {
                Some(Ok(f)) => {
                    if let Ok(d) = f.into_data() {
                        self.buf.push_str(std::str::from_utf8(&d).unwrap());
                    }
                }
                Some(Err(e)) => panic!("body error: {e}"),
                None => return None,
            }
        }
    }

    /// Reads to the end; returns all events.
    pub async fn events(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        while let Some(e) = self.next_event().await {
            out.push(e);
        }
        out
    }

    pub async fn next_within(&mut self, d: Duration) -> Option<Option<String>> {
        tokio::time::timeout(d, self.next()).await.ok()
    }

    /// Reads to the end; returns all payloads.
    pub async fn all(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(f) = self.next().await {
            out.push(f);
        }
        out
    }
}

/// JSON chunks of a drained stream (everything except `[DONE]`).
pub fn json_chunks(frames: &[String]) -> Vec<Value> {
    frames
        .iter()
        .filter(|f| *f != "[DONE]")
        .map(|f| serde_json::from_str(f).unwrap())
        .collect()
}
