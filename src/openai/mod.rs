//! The OpenAI frontend: `/v1/chat/completions` and `/v1/models`, plus the error body format and
//! API-key check shared by every OpenAI-shaped endpoint.

pub mod chat;
pub mod error;
pub mod models;
pub mod stream;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use http_body_util::{BodyExt, Limited};
use serde_json::Value;

use crate::control::MAX_BODY;
use crate::fallback;
use crate::ir::{ApiError, Outcome, Reply, Request};
use crate::openai::chat::ResponseMeta;
use crate::reqlog::RequestRecord;
use crate::{Response, Shared, json_response};

/// Raw bodies that did not parse as JSON are logged up to this many bytes.
const RAW_TEXT_LIMIT: usize = 64 << 10;

const JSON_PARSE_HINT: &str = "We could not parse the JSON body of your request. (HINT: This likely means you aren't using your HTTP \
library correctly. The OpenAI API expects a JSON payload, but what was sent was not valid JSON. If you have trouble figuring out how to \
fix this, please contact us through our help center at help.openai.com.)";

/// What the log wants to know about a request, filled in as handling progresses.
#[derive(Default)]
struct Trace {
    raw: Option<Arc<Value>>,
    raw_text: Option<String>,
    request: Option<Arc<Request>>,
    outcome: Option<Arc<Outcome>>,
    seed: Option<u64>,
}

enum Route<'a> {
    Chat,
    Models,
    Model(&'a str),
    Unknown,
}

fn route<'a>(method: &http::Method, path: &'a str) -> Route<'a> {
    let path = path.strip_prefix("/v1").unwrap_or(path);
    match (method, path) {
        (&http::Method::POST, "/chat/completions") => Route::Chat,
        (&http::Method::GET, "/models") => Route::Models,
        (&http::Method::GET, p) => match p.strip_prefix("/models/") {
            Some(id) if !id.is_empty() => Route::Model(id),
            _ => Route::Unknown,
        },
        _ => Route::Unknown,
    }
}

fn is_api_path(path: &str) -> bool {
    path == "/v1" || path.starts_with("/v1/") || path == "/chat/completions" || path == "/models" || path.starts_with("/models/")
}

/// Routes one request. Order of work for `POST /v1/chat/completions`:
///
/// 1. API key check (`Shared::api_key`): `Authorization: Bearer <k>` or `x-api-key: <k>`.
///    Mismatch is a 401 `invalid_api_key`; with no configured key everything passes.
/// 2. Read the body (limit `control::MAX_BODY`); non-JSON is a 400 `invalid_request_error`
///    ("We could not parse the JSON body of your request. ...").
/// 3. `chat::parse` into the IR; validation errors are 400s with `param` set.
/// 4. `Control::engine.resolve` for the outcome.
/// 5. Render: `error::render` for errors (after `delivery.ttft` on the server clock),
///    `chat::render_sync` + JSON for sync replies (after the computed latency),
///    `stream::respond` for streams.
/// 6. Log a `RequestRecord` (after step 5 has produced the response head, so a sync reply or error
///    parked on the server clock appears in the log only once it is released) (status, latency to head, headers, raw, IR, outcome) when
///    `Control::log` is enabled. Every API request is logged, including rejected ones.
///
/// `GET /v1/models` and `GET /v1/models/{id}` come from `models`. Unknown paths are a 404
/// `invalid_request_error` with message `Invalid URL (<METHOD> <path>)`, like the real API.
///
/// The body of a POST is read before the key check so that rejected requests are logged with
/// what the client sent. Chat Completions is also served without the `/v1` prefix, for base URLs
/// configured either way.
pub(crate) async fn handle(shared: &Shared, req: http::Request<hyper::body::Incoming>) -> Response {
    let started = Instant::now();
    let arrived = shared.control.clock.now();
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_owned();
    let logged = is_api_path(&path) && shared.control.log.is_enabled();
    let mut trace = Trace::default();

    let mut response = dispatch(shared, &parts, &path, body, &mut trace).await;

    let seed = trace.seed.unwrap_or_else(|| fallback::hash64(path.as_bytes()));
    let headers = response.headers_mut();
    if !headers.contains_key("x-request-id") {
        headers.insert("x-request-id", header_value(&format!("req_{seed:016x}")));
    }
    headers.insert("openai-processing-ms", header_value(&started.elapsed().as_millis().to_string()));

    if logged {
        shared.control.log.record(RequestRecord {
            seq: 0,
            time: arrived,
            method: parts.method.clone(),
            path: parts.uri.path_and_query().map_or(path.clone(), |p| p.as_str().to_owned()),
            status: response.status().as_u16(),
            latency: started.elapsed(),
            headers: parts
                .headers
                .iter()
                .map(|(k, v)| (k.as_str().to_ascii_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
                .collect(),
            raw: trace.raw,
            raw_text: trace.raw_text,
            request: trace.request,
            outcome: trace.outcome,
        });
    }
    response
}

fn header_value(s: &str) -> http::HeaderValue {
    http::HeaderValue::from_str(s).unwrap_or_else(|_| http::HeaderValue::from_static(""))
}

async fn dispatch(shared: &Shared, parts: &http::request::Parts, path: &str, body: hyper::body::Incoming, trace: &mut Trace) -> Response {
    let route = route(&parts.method, path);
    if matches!(route, Route::Unknown) {
        return reject(
            trace,
            ApiError::new(404, "invalid_request_error", format!("Invalid URL ({} {path})", parts.method)),
        );
    }

    let mut body_bytes = None;
    if matches!(route, Route::Chat) {
        match Limited::new(body, MAX_BODY).collect().await {
            Ok(b) => body_bytes = Some(b.to_bytes()),
            Err(e) => {
                let err = if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() {
                    ApiError::new(413, "invalid_request_error", "Request body is too large.")
                } else {
                    ApiError::invalid_request("Failed to read the request body.")
                };
                return reject(trace, err);
            }
        }
    }

    if let Some(key) = &shared.api_key {
        let h = &parts.headers;
        let bearer = h
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let x_api_key = h.get("x-api-key").and_then(|v| v.to_str().ok());
        if bearer != Some(key.as_str()) && x_api_key != Some(key.as_str()) {
            note_body(trace, body_bytes.as_deref());
            return reject(trace, ApiError::invalid_api_key());
        }
    }

    match route {
        Route::Models => json_response(200, &models::list(&shared.models)),
        Route::Model(id) => match models::get(&shared.models, &percent_encoding::percent_decode_str(id).decode_utf8_lossy()) {
            Some(m) => json_response(200, &m),
            None => reject(trace, ApiError::model_not_found(id)),
        },
        Route::Chat => chat_completions(shared, &body_bytes.unwrap_or_default(), trace).await,
        Route::Unknown => unreachable!("handled above"),
    }
}

/// Fills `raw` or `raw_text` of the trace from a body; returns the parsed JSON.
fn note_body(trace: &mut Trace, bytes: Option<&[u8]>) -> Option<Value> {
    let bytes = bytes?;
    match serde_json::from_slice::<Value>(bytes) {
        Ok(v) => {
            trace.raw = Some(Arc::new(v.clone()));
            Some(v)
        }
        Err(_) => {
            let cut = &bytes[..bytes.len().min(RAW_TEXT_LIMIT)];
            trace.raw_text = Some(String::from_utf8_lossy(cut).into_owned());
            None
        }
    }
}

fn reject(trace: &mut Trace, err: ApiError) -> Response {
    let response = error::render(&err);
    trace.outcome = Some(Arc::new(Outcome::rejected(err)));
    response
}

async fn chat_completions(shared: &Shared, bytes: &[u8], trace: &mut Trace) -> Response {
    let control = &shared.control;
    trace.seed = Some(fallback::hash64(bytes));
    let Some(value) = note_body(trace, Some(bytes)) else {
        return reject(trace, ApiError::invalid_request(JSON_PARSE_HINT));
    };
    let req = match chat::parse(&value) {
        Ok(r) => r,
        Err(e) => return reject(trace, e),
    };
    let seed = fallback::request_seed(&req);
    trace.seed = Some(seed);
    let outcome = control.engine.resolve(&req);
    let req = Arc::new(req);
    trace.request = Some(req.clone());
    trace.outcome = Some(Arc::new(outcome.clone()));

    match outcome {
        Outcome::Error { error, delivery, .. } => {
            wait(&control.clock, delivery.ttft).await;
            error::render(&error)
        }
        Outcome::Reply { reply, delivery, .. } => {
            let meta = ResponseMeta {
                id: fallback::completion_id(seed ^ fallback::hash64(&ID_COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes())),
                created: control.clock.unix_secs(),
                model: req.model.clone(),
                system_fingerprint: fallback::fingerprint(&req.model),
            };
            if req.stream {
                let chunks = chat::chunks(&req, &reply, &meta, delivery.chunk_tokens, delivery.args_chunk_bytes);
                stream::respond(control.clock.clone(), delivery, chunks, format!("req_{seed:016x}"))
            } else {
                wait(&control.clock, sync_latency(&reply, &delivery)).await;
                json_response(200, &chat::render_sync(&req, &reply, &meta))
            }
        }
    }
}

/// Mixed into the completion id so identical requests (retries, parallel agents) get distinct ids.
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn sync_latency(reply: &Reply, delivery: &crate::ir::Delivery) -> Duration {
    let extra = delivery
        .token_interval
        .checked_mul(reply.usage.completion_tokens.saturating_sub(1))
        .unwrap_or(Duration::ZERO);
    delivery.ttft.saturating_add(extra)
}

async fn wait(clock: &crate::clock::Clock, d: Duration) {
    if d.is_zero() {
        return;
    }
    let now: SystemTime = clock.now();
    if let Some(deadline) = now.checked_add(d) {
        clock.wait_until(deadline).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        use http::Method as M;
        assert!(matches!(route(&M::POST, "/v1/chat/completions"), Route::Chat));
        assert!(matches!(route(&M::POST, "/chat/completions"), Route::Chat));
        assert!(matches!(route(&M::GET, "/v1/models"), Route::Models));
        assert!(matches!(route(&M::GET, "/v1/models/gpt-4o"), Route::Model("gpt-4o")));
        assert!(matches!(route(&M::GET, "/v1/models/"), Route::Unknown));
        assert!(matches!(route(&M::GET, "/v1/chat/completions"), Route::Unknown));
        assert!(matches!(route(&M::POST, "/v1/embeddings"), Route::Unknown));
    }

    #[test]
    fn api_paths() {
        assert!(is_api_path("/v1/anything"));
        assert!(is_api_path("/chat/completions"));
        assert!(!is_api_path("/favicon.ico"));
        assert!(!is_api_path("/v10"));
    }

    #[test]
    fn note_body_splits_json_and_text() {
        let mut t = Trace::default();
        assert!(note_body(&mut t, Some(br#"{"a":1}"#)).is_some());
        assert!(t.raw.is_some() && t.raw_text.is_none());
        let mut t = Trace::default();
        assert!(note_body(&mut t, Some(&vec![b'x'; RAW_TEXT_LIMIT + 10])).is_none());
        assert_eq!(t.raw_text.unwrap().len(), RAW_TEXT_LIMIT);
    }

    #[test]
    fn sync_latency_counts_inter_token_gaps() {
        let reply = Reply {
            usage: crate::ir::Usage {
                completion_tokens: 5,
                ..Default::default()
            },
            ..Default::default()
        };
        let d = crate::ir::Delivery {
            ttft: Duration::from_millis(100),
            token_interval: Duration::from_millis(10),
            ..Default::default()
        };
        assert_eq!(sync_latency(&reply, &d), Duration::from_millis(140));
    }
}
