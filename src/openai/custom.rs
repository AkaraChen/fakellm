//! Custom endpoints: JSON APIs that clients call next to the LLM routes (`POST /v1/systemone`
//! from xsai). A path no standard route serves is answered by the first rule with `match.path`
//! and `reply.body` that matches; with none, it stays a 404 `Invalid URL`. The request is logged
//! as `Protocol::Custom` with the raw body and the matched rule. See docs/scripts.md.

use std::sync::Arc;

use serde_json::Value;

use crate::fallback;
use crate::ir::{Delivery, FinishReason, Outcome, Protocol, Reply, Request, Source, Usage};
use crate::openai::{Trace, note_body};
use crate::{Response, Shared, full_body};

/// The response for `bytes` posted to `path`, or `None` when no custom rule matches.
pub(super) fn respond(shared: &Shared, path: &str, bytes: &[u8], trace: &mut Trace) -> Option<Response> {
    let mut req = Request::new("", Vec::new());
    req.protocol = Protocol::Custom;
    req.raw_body = Some(String::from_utf8_lossy(bytes).into());
    req.path = Some(path.to_owned());
    let (index, name, spec) = shared.control.engine.resolve_custom(&req)?;
    trace.seed = Some(fallback::hash64(bytes));
    trace.custom = true;
    note_body(trace, Some(bytes));
    let body = spec.body.unwrap_or(Value::Null);
    let status = spec.status.unwrap_or(200);
    let source = Source::Script { index, name };
    trace.request = Some(Arc::new(req));
    trace.outcome = Some(Arc::new(Outcome::Reply {
        reply: Reply {
            text: Some(body.to_string()),
            finish: FinishReason::Stop,
            usage: Usage::default(),
            ..Reply::default()
        },
        delivery: Delivery::default(),
        source,
    }));
    let bytes = bytes::Bytes::from(serde_json::to_vec(&body).unwrap_or_default());
    let mut response = http::Response::new(full_body(bytes.clone()));
    *response.status_mut() = http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::OK);
    let headers = response.headers_mut();
    headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("application/json"));
    for (k, v) in spec.headers.iter().flatten() {
        if let (Ok(n), Ok(v)) = (http::HeaderName::from_bytes(k.as_bytes()), http::HeaderValue::from_str(v)) {
            headers.insert(n, v);
        }
    }
    headers.insert(http::header::CONTENT_LENGTH, bytes.len().into());
    Some(response)
}
