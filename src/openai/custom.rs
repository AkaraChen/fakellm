//! Custom endpoint rules: a rule with `match.path` answers by itself, whatever the path is. Two
//! uses: JSON APIs clients call next to the LLM routes (`POST /v1/systemone` from xsai; a path no
//! standard route serves stays a 404 `Invalid URL` when no rule matches), and recorded responses
//! replayed verbatim on the standard routes (`/v1/chat/completions`, `/v1/responses`, ...), where a
//! matching rule takes precedence over the protocol handling like a msw / wiremock fixture would.
//!
//! The reply is `reply.body` (JSON) or `reply.raw` (any status, content type and body; an SSE body is
//! streamed one event per chunk). The request is parsed into the IR when its route is one of the
//! generation endpoints and the body parses, so `last_user`, `model` and the rest match as usual, and
//! logged as `Protocol::Custom` (or the route's own protocol) with the raw body and the matched rule.
//! See docs/scripts.md.

use std::sync::Arc;

use bytes::Bytes;
use serde_json::Value;

use crate::fallback;
use crate::ir::{Delivery, FinishReason, Outcome, Protocol, Reply, Request, Source, Usage};
use crate::openai::{Api, Trace, note_body, sync_done, wait};
use crate::script::RawSpec;
use crate::{Response, Shared, anthropic, full_body, sse};

/// What the router knows about the route a request came in on.
pub(super) struct Target {
    /// The generation endpoint whose body is parsed into the IR, if any.
    pub api: Option<Api>,
    /// The protocol of the route, `Custom` for a path no route serves.
    pub protocol: Protocol,
}

/// The response for `bytes` sent to `path`, or `None` when no custom rule matches.
pub(super) async fn respond(
    shared: &Shared,
    parts: &http::request::Parts,
    path: &str,
    bytes: &[u8],
    target: Target,
    trace: &mut Trace,
) -> Option<Response> {
    let control = &shared.control;
    let mut req = serde_json::from_slice::<Value>(bytes)
        .ok()
        .zip(target.api)
        .and_then(|(value, api)| match api {
            Api::Chat => crate::openai::chat::parse(&value).ok(),
            Api::Messages => anthropic::messages::parse(&value, true).ok(),
            Api::Responses => crate::openai::responses::parse(&value, &control.responses).ok(),
        })
        .unwrap_or_else(|| Request::new("", Vec::new()));
    req.protocol = target.protocol;
    req.raw_body = Some(String::from_utf8_lossy(bytes).into());
    req.path = Some(path.to_owned());
    super::note_http(&mut req, parts);
    let (index, name, spec, delivery) = control.engine.resolve_custom(&req)?;
    trace.seed = Some(fallback::hash64(bytes));
    trace.custom = true;
    note_body(trace, Some(bytes));
    let text = match (&spec.raw, &spec.body) {
        (Some(raw), _) => raw.body.clone(),
        (None, body) => body.as_ref().unwrap_or(&Value::Null).to_string(),
    };
    let source = Source::Script { index, name };
    trace.request = Some(Arc::new(req));
    trace.outcome = Some(Arc::new(Outcome::Reply {
        reply: Reply {
            text: Some(text),
            finish: FinishReason::Stop,
            usage: Usage::default(),
            ..Reply::default()
        },
        delivery: delivery.clone(),
        source,
    }));

    if let Some(raw) = &spec.raw {
        return Some(raw_response(shared, raw, delivery).await);
    }
    let body = spec.body.unwrap_or(Value::Null);
    let status = spec.status.unwrap_or(200);
    let bytes = Bytes::from(serde_json::to_vec(&body).unwrap_or_default());
    let mut response = http::Response::new(full_body(bytes.clone()));
    *response.status_mut() = http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::OK);
    let headers = response.headers_mut();
    headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("application/json"));
    insert_headers(headers, spec.headers.iter().flatten());
    headers.insert(http::header::CONTENT_LENGTH, bytes.len().into());
    Some(response)
}

fn insert_headers<'a>(headers: &mut http::HeaderMap, extra: impl Iterator<Item = (&'a String, &'a String)>) {
    for (k, v) in extra {
        if let (Ok(n), Ok(v)) = (http::HeaderName::from_bytes(k.as_bytes()), http::HeaderValue::from_str(v)) {
            headers.insert(n, v);
        }
    }
}

/// `raw` as a response: one buffered body with `content-length` after `ttft`, or chunks on the clock.
async fn raw_response(shared: &Shared, raw: &RawSpec, delivery: Delivery) -> Response {
    let clock = &shared.control.clock;
    let status = http::StatusCode::from_u16(raw.status.unwrap_or(200)).unwrap_or(http::StatusCode::OK);
    let mut headers = http::HeaderMap::new();
    let content_type = raw.content_type.as_deref().unwrap_or("application/json");
    if let Ok(v) = http::HeaderValue::from_str(content_type) {
        headers.insert(http::header::CONTENT_TYPE, v);
    }
    if raw.is_event_stream() {
        headers.insert(http::header::CACHE_CONTROL, http::HeaderValue::from_static("no-cache"));
    }
    insert_headers(&mut headers, raw.headers.iter().flatten());
    if raw.streams() {
        let chunks = if raw.is_event_stream() {
            split_events(&raw.body)
        } else {
            vec![Bytes::from(raw.body.clone())]
        };
        return sse::respond_chunks(clock.clone(), delivery, status, headers, chunks);
    }
    wait(clock, delivery.ttft).await;
    let body = Bytes::from(raw.body.clone());
    headers.insert(http::header::CONTENT_LENGTH, body.len().into());
    let mut response = http::Response::new(full_body(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    sync_done(response, &delivery).await
}

/// An SSE body cut after each blank line (`\n\n` or `\r\n\r\n`), the blank line kept with its event;
/// text after the last blank line is the final chunk.
fn split_events(body: &str) -> Vec<Bytes> {
    let b = body.as_bytes();
    let (mut out, mut start, mut i) = (Vec::new(), 0, 0);
    while i < b.len() {
        let end = if b[i..].starts_with(b"\n\n") {
            i + 2
        } else if b[i..].starts_with(b"\r\n\r\n") {
            i + 4
        } else {
            i += 1;
            continue;
        };
        out.push(Bytes::copy_from_slice(&b[start..end]));
        (start, i) = (end, end);
    }
    if start < b.len() {
        out.push(Bytes::copy_from_slice(&b[start..]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> Vec<String> {
        split_events(s).iter().map(|c| String::from_utf8_lossy(c).into_owned()).collect()
    }

    #[test]
    fn events_split_after_each_blank_line() {
        assert_eq!(
            split("data: a\n\ndata: b\n\ndata: [DONE]\n\n"),
            ["data: a\n\n", "data: b\n\n", "data: [DONE]\n\n"]
        );
        assert_eq!(
            split("event: x\ndata: 1\n\nevent: y\ndata: 2\n\n"),
            ["event: x\ndata: 1\n\n", "event: y\ndata: 2\n\n"]
        );
        assert_eq!(split("data: a\r\n\r\ndata: b\r\n\r\n"), ["data: a\r\n\r\n", "data: b\r\n\r\n"]);
    }

    #[test]
    fn text_after_the_last_blank_line_is_kept() {
        assert_eq!(split("data: a\n\ndata: b"), ["data: a\n\n", "data: b"]);
        assert_eq!(split(""), Vec::<String>::new());
        assert_eq!(split("no blank line"), ["no blank line"]);
    }
}
