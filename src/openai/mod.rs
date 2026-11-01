//! The API router and request pipeline for every endpoint under `/v1`: OpenAI Chat Completions,
//! OpenAI Responses, Anthropic Messages and the model list. The per-protocol parsing and
//! rendering live in `chat`, `responses*` and `crate::anthropic`; this module owns what they
//! share: the API-key check, body reading, scripts, the server-clock timing of replies and
//! errors, the Responses store hand-off, and the request log. See docs/m1-spec.md.

pub mod chat;
pub mod error;
pub mod models;
pub mod responses;
pub mod responses_store;
pub mod responses_stream;

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use http_body_util::{BodyExt, Limited};
use serde_json::Value;

use crate::anthropic;
use crate::anthropic::messages::MessageMeta;
use crate::control::MAX_BODY;
use crate::fallback;
use crate::ir::{ApiError, Outcome, Reply, Request};
use crate::openai::chat::ResponseMeta;
use crate::reqlog::RequestRecord;
use crate::sse;
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
    /// `x-request-id`; derived from the request seed and the per-server counter.
    request_id: Option<String>,
    /// Method, path, headers and arrival time, for [`HangGuard`].
    head: Option<RequestRecord>,
    /// Log seq of the record written when the request was parked by a `hang` rule; the final
    /// status and latency go there instead of into a new record.
    hung_seq: Option<u64>,
}

/// Records a request as `status: 0` when its future is dropped while parked on the server clock
/// (a hang, or the latency of an error or sync reply; client disconnected); hyper drops the
/// handler then, so the normal log call at the end never runs. A request parked by a `hang` rule
/// already has a record (`hung_seq`), which is updated instead. Skipped when a reset happened
/// since the request arrived, so a stale request cannot write into the cleared log.
struct HangGuard {
    control: Arc<crate::control::Control>,
    record: Option<RequestRecord>,
    generation: u64,
    started: Instant,
    hung_seq: Option<u64>,
}

impl Drop for HangGuard {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else { return };
        if self.control.generation() != self.generation {
            return;
        }
        match self.hung_seq {
            Some(seq) => {
                let latency = self.started.elapsed();
                self.control.log.update(seq, |r| {
                    r.hung = false;
                    r.latency = latency;
                });
            }
            None => {
                self.control.log.record(record);
            }
        }
    }
}

enum Route<'a> {
    /// `POST /v1/chat/completions`
    Chat,
    /// `POST /v1/messages`
    Messages,
    /// `POST /v1/messages/count_tokens`
    CountTokens,
    /// `POST /v1/responses`
    Responses,
    /// `GET /v1/responses/{id}`
    ResponseGet(&'a str),
    /// `DELETE /v1/responses/{id}`
    ResponseDelete(&'a str),
    Models,
    Model(&'a str),
    Unknown,
}

/// Which error body format a route answers with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    Openai,
    Anthropic,
}

/// Which protocol a POST generation endpoint speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Api {
    Chat,
    Messages,
    Responses,
}

fn route<'a>(method: &http::Method, path: &'a str) -> Route<'a> {
    let path = path.strip_prefix("/v1").unwrap_or(path);
    match (method, path) {
        (&http::Method::POST, "/chat/completions") => Route::Chat,
        (&http::Method::POST, "/messages") => Route::Messages,
        (&http::Method::POST, "/messages/count_tokens") => Route::CountTokens,
        (&http::Method::POST, "/responses") => Route::Responses,
        (&http::Method::GET, "/models") => Route::Models,
        (&http::Method::GET, p) => match (p.strip_prefix("/models/"), p.strip_prefix("/responses/")) {
            (Some(id), _) if !id.is_empty() => Route::Model(id),
            (_, Some(id)) if !id.is_empty() && !id.contains('/') => Route::ResponseGet(id),
            _ => Route::Unknown,
        },
        (&http::Method::DELETE, p) => match p.strip_prefix("/responses/") {
            Some(id) if !id.is_empty() && !id.contains('/') => Route::ResponseDelete(id),
            _ => Route::Unknown,
        },
        _ => Route::Unknown,
    }
}

fn is_api_path(path: &str) -> bool {
    path == "/v1"
        || path.starts_with("/v1/")
        || ["/chat/completions", "/messages", "/responses", "/models"]
            .iter()
            .any(|p| path == *p || path.strip_prefix(p).is_some_and(|rest| rest.starts_with('/')))
}

/// `/v1/messages*` is always Anthropic. `/v1/models*` and unknown paths are Anthropic only for
/// clients that send `anthropic-version`.
fn style(route: &Route<'_>, path: &str, headers: &http::HeaderMap) -> Style {
    let anthropic_path = matches!(route, Route::Messages | Route::CountTokens) || path.trim_start_matches("/v1").starts_with("/messages");
    if anthropic_path || ((matches!(route, Route::Models | Route::Model(_) | Route::Unknown)) && anthropic::is_anthropic_client(headers)) {
        Style::Anthropic
    } else {
        Style::Openai
    }
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
///    `sse::respond` for streams.
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
    let generation = shared.control.generation();
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_owned();
    let logged = is_api_path(&path) && shared.control.log.is_enabled();
    let mut trace = Trace::default();
    if logged {
        trace.head = Some(RequestRecord {
            seq: 0,
            time: arrived,
            method: parts.method.clone(),
            path: parts.uri.path_and_query().map_or(path.clone(), |p| p.as_str().to_owned()),
            status: 0,
            hung: false,
            latency: Duration::ZERO,
            headers: parts
                .headers
                .iter()
                .map(|(k, v)| (k.as_str().to_ascii_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
                .collect(),
            raw: None,
            raw_text: None,
            request: None,
            outcome: None,
        });
    }

    let mut response = dispatch(shared, &parts, &path, body, &mut trace).await;

    let seed = trace.seed.unwrap_or_else(|| fallback::hash64(path.as_bytes()));
    let headers = response.headers_mut();
    if !headers.contains_key("x-request-id") {
        let id = trace.request_id.clone().unwrap_or_else(|| format!("req_{seed:016x}"));
        headers.insert("x-request-id", header_value(&id));
    }
    if style(&route(&parts.method, &path), &path, &parts.headers) == Style::Anthropic
        && let Some(id) = headers.get("x-request-id").cloned()
    {
        headers.insert("request-id", id);
    }
    headers.insert("openai-processing-ms", header_value(&started.elapsed().as_millis().to_string()));

    if logged && shared.control.generation() == generation {
        if let Some(seq) = trace.hung_seq {
            let (status, latency) = (response.status().as_u16(), started.elapsed());
            shared.control.log.update(seq, |r| {
                r.status = status;
                r.latency = latency;
                r.hung = false;
            });
            return response;
        }
        shared.control.log.record(RequestRecord {
            seq: 0,
            time: arrived,
            method: parts.method.clone(),
            path: parts.uri.path_and_query().map_or(path.clone(), |p| p.as_str().to_owned()),
            status: response.status().as_u16(),
            hung: false,
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

/// 404/413 for non-generation failures: OpenAI's vocabulary is a plain `invalid_request_error`,
/// Anthropic has dedicated types.
fn style_error(style: Style, status: u16, message: impl Into<String>) -> ApiError {
    match style {
        Style::Openai => ApiError::new(status, "invalid_request_error", message),
        Style::Anthropic => ApiError::for_status_in(crate::ir::Protocol::AnthropicMessages, status).with_message(message),
    }
}

fn header_value(s: &str) -> http::HeaderValue {
    http::HeaderValue::from_str(s).unwrap_or_else(|_| http::HeaderValue::from_static(""))
}

async fn dispatch(shared: &Shared, parts: &http::request::Parts, path: &str, body: hyper::body::Incoming, trace: &mut Trace) -> Response {
    let route = route(&parts.method, path);
    let style = style(&route, path, &parts.headers);
    if matches!(route, Route::Unknown) {
        return reject(
            trace,
            style,
            style_error(style, 404, format!("Invalid URL ({} {path})", parts.method)),
        );
    }

    let mut body_bytes = None;
    if matches!(route, Route::Chat | Route::Messages | Route::CountTokens | Route::Responses) {
        match Limited::new(body, MAX_BODY).collect().await {
            Ok(b) => body_bytes = Some(b.to_bytes()),
            Err(e) => {
                let err = if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() {
                    style_error(style, 413, "Request body is too large.")
                } else {
                    ApiError::invalid_request("Failed to read the request body.")
                };
                return reject(trace, style, err);
            }
        }
    }

    if let Some(key) = &shared.api_key {
        let h = &parts.headers;
        let bearer = h
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| token);
        let x_api_key = h.get("x-api-key").and_then(|v| v.to_str().ok());
        if bearer != Some(key.as_str()) && x_api_key != Some(key.as_str()) {
            note_body(trace, body_bytes.as_deref());
            let err = match style {
                Style::Openai => ApiError::invalid_api_key(),
                Style::Anthropic => ApiError::for_status_in(crate::ir::Protocol::AnthropicMessages, 401),
            };
            return reject(trace, style, err);
        }
    }

    let bytes = body_bytes.unwrap_or_default();
    match route {
        Route::Models if style == Style::Anthropic => json_response(200, &anthropic::models_list(&shared.models)),
        Route::Models => json_response(200, &models::list(&shared.models)),
        Route::Model(id) => {
            let id = percent_encoding::percent_decode_str(id).decode_utf8_lossy();
            let found = match style {
                Style::Anthropic => anthropic::model_get(&shared.models, &id),
                Style::Openai => models::get(&shared.models, &id),
            };
            match found {
                Some(m) => json_response(200, &m),
                None if style == Style::Anthropic => reject(
                    trace,
                    style,
                    ApiError::for_status_in(crate::ir::Protocol::AnthropicMessages, 404).with_message(format!("model: {id}")),
                ),
                None => reject(trace, style, ApiError::model_not_found(&id)),
            }
        }
        Route::Chat => complete(shared, Api::Chat, &bytes, trace).await,
        Route::Messages => complete(shared, Api::Messages, &bytes, trace).await,
        Route::Responses => complete(shared, Api::Responses, &bytes, trace).await,
        Route::CountTokens => count_tokens(shared, &bytes, trace),
        Route::ResponseGet(id) => match shared.control.responses.get(id) {
            Some(r) => json_response(200, &r.response),
            None => reject(trace, style, responses::not_found(id)),
        },
        Route::ResponseDelete(id) => {
            if shared.control.responses.remove(id) {
                json_response(200, &responses::deleted_json(id))
            } else {
                reject(trace, style, responses::not_found(id))
            }
        }
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

fn reject(trace: &mut Trace, style: Style, err: ApiError) -> Response {
    let response = match style {
        Style::Openai => error::render(&err),
        Style::Anthropic => anthropic::error::render(&err),
    };
    trace.outcome = Some(Arc::new(Outcome::rejected(err)));
    response
}

/// `POST /v1/messages/count_tokens`: parse like Messages (without requiring `max_tokens`) and
/// answer `{"input_tokens": n}`. Only a rule with `usage.input_tokens` is consulted, and its hit
/// is counted (see `Engine::count_input_tokens`).
fn count_tokens(shared: &Shared, bytes: &[u8], trace: &mut Trace) -> Response {
    trace.seed = Some(fallback::hash64(bytes));
    let Some(value) = note_body(trace, Some(bytes)) else {
        return reject(trace, Style::Anthropic, ApiError::invalid_request(ANTHROPIC_JSON_HINT));
    };
    match anthropic::messages::parse(&value, false) {
        Ok(req) => {
            let body = anthropic::count_tokens(&shared.control.engine, &req);
            trace.request = Some(Arc::new(req));
            json_response(200, &body)
        }
        Err(e) => reject(trace, Style::Anthropic, e),
    }
}

const ANTHROPIC_JSON_HINT: &str = "The request body is not valid JSON.";

/// The pipeline for the three generation endpoints, in order:
///
/// 1. `note_body`: non-JSON is a 400 (Chat: the OpenAI hint text; Messages: [`ANTHROPIC_JSON_HINT`]).
/// 2. The protocol's `parse` into the IR; validation errors are 400s.
/// 3. `Engine::resolve`. `Outcome::Hang` parks on the server clock first: `until` after arrival,
///    forever when `None`; then the wrapped outcome continues below.
/// 4. Errors: `delivery.ttft` on the server clock, then either an in-stream error (when
///    `error.in_stream` and the request streams: `sse::respond` with the protocol's error
///    frames) or the protocol's HTTP error body.
/// 5. Replies: streams through `sse::respond`; sync bodies after `sync_latency`. Responses are
///    stored (`Control::responses`) once delivery starts (after the latency for sync bodies) unless the request said `store: false`.
async fn complete(shared: &Shared, api: Api, bytes: &[u8], trace: &mut Trace) -> Response {
    let control = &shared.control;
    let arrived = control.clock.now();
    let style = if api == Api::Messages { Style::Anthropic } else { Style::Openai };
    trace.seed = Some(fallback::hash64(bytes));
    let Some(value) = note_body(trace, Some(bytes)) else {
        let hint = if api == Api::Messages {
            ANTHROPIC_JSON_HINT
        } else {
            JSON_PARSE_HINT
        };
        return reject(trace, style, ApiError::invalid_request(hint));
    };
    let parsed = match api {
        Api::Chat => chat::parse(&value),
        Api::Messages => anthropic::messages::parse(&value, true),
        Api::Responses => responses::parse(&value, &control.responses),
    };
    let req = match parsed {
        Ok(r) => r,
        Err(e) => return reject(trace, style, e),
    };
    let seed = fallback::request_seed(&req);
    trace.seed = Some(seed);
    let mut outcome = control.engine.resolve(&req);
    let req = Arc::new(req);
    trace.request = Some(req.clone());
    trace.outcome = Some(Arc::new(outcome.clone()));

    let mut guard = HangGuard {
        control: shared.control.clone(),
        record: trace.head.clone().map(|h| RequestRecord {
            raw: trace.raw.clone(),
            raw_text: trace.raw_text.clone(),
            request: trace.request.clone(),
            outcome: trace.outcome.clone(),
            ..h
        }),
        generation: control.generation(),
        started: Instant::now(),
        hung_seq: None,
    };
    if let Outcome::Hang { until, then } = outcome {
        // Registered before the record is visible, so a reset right after cannot be missed.
        let released = control.release.notified();
        tokio::pin!(released);
        released.as_mut().enable();
        if let Some(record) = &guard.record {
            let seq = control.log.record(RequestRecord {
                hung: true,
                ..record.clone()
            });
            (guard.hung_seq, trace.hung_seq) = ((seq != 0).then_some(seq), (seq != 0).then_some(seq));
        }
        let by_reset = tokio::select! {
            () = async {
                match until.and_then(|d| arrived.checked_add(d)) {
                    Some(deadline) => control.clock.wait_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => false,
            () = &mut released => true,
        };
        outcome = *then;
        // A hang without a reply of its own, woken by a reset, can only be an overload.
        if by_reset && until.is_none() && is_placeholder(&outcome) {
            outcome = Outcome::Error {
                error: ApiError::overloaded(),
                delivery: Default::default(),
                source: outcome.source().clone(),
            };
        }
    }
    let unique = seed ^ fallback::hash64(&control.next_salt().to_le_bytes());
    let request_id = format!("req_{unique:016x}");
    trace.request_id = Some(request_id.clone());

    let response = async {
        match outcome {
            Outcome::Error { error, delivery, .. } => {
                if error.in_stream && req.stream {
                    let mut headers = crate::error_headers(&error);
                    headers.push(("x-request-id".into(), request_id));
                    let frames = match api {
                        Api::Chat => chat::error_frames(&error),
                        Api::Messages => {
                            let meta = MessageMeta {
                                id: fallback::message_id(unique),
                                model: req.model.clone(),
                            };
                            anthropic::stream::error_frames(&error, &meta, crate::tokenizer::count_request(&req))
                        }
                        Api::Responses => responses_stream::error_frames(&req, &error, &response_meta(control, &req, unique)),
                    };
                    return sse::respond(control.clock.clone(), delivery, frames, headers, sentinel(api));
                }
                wait(&control.clock, delivery.ttft).await;
                match style {
                    Style::Openai => error::render(&error),
                    Style::Anthropic => anthropic::error::render(&error),
                }
            }
            Outcome::Reply { reply, delivery, .. } => match api {
                Api::Chat => {
                    let meta = ResponseMeta {
                        id: fallback::completion_id(unique),
                        created: control.clock.unix_secs(),
                        model: req.model.clone(),
                        system_fingerprint: fallback::fingerprint(&req.model),
                    };
                    if req.stream {
                        let frames = chat::chunks(&req, &reply, &meta, delivery.chunk_tokens, delivery.args_chunk_bytes);
                        crate::sse::respond(
                            control.clock.clone(),
                            delivery,
                            frames,
                            vec![("x-request-id".into(), request_id)],
                            Some("[DONE]"),
                        )
                    } else {
                        wait(&control.clock, sync_latency(&reply, &delivery)).await;
                        sync_done(json_response(200, &chat::render_sync(&req, &reply, &meta)), &delivery).await
                    }
                }
                Api::Messages => {
                    let meta = MessageMeta {
                        id: fallback::message_id(unique),
                        model: req.model.clone(),
                    };
                    if req.stream {
                        let frames = anthropic::stream::frames(&req, &reply, &meta, delivery.chunk_tokens, delivery.args_chunk_bytes);
                        sse::respond(
                            control.clock.clone(),
                            delivery,
                            frames,
                            vec![("x-request-id".into(), request_id)],
                            None,
                        )
                    } else {
                        wait(&control.clock, sync_latency(&reply, &delivery)).await;
                        sync_done(
                            json_response(200, &anthropic::messages::render_sync(&req, &reply, &meta)),
                            &delivery,
                        )
                        .await
                    }
                }
                Api::Responses => {
                    let meta = response_meta(control, &req, unique);
                    let stored = (req.store != Some(false)).then(|| responses::to_stored(&req, &reply, &meta));
                    if req.stream {
                        control.responses.insert_opt(stored);
                        let frames = responses_stream::frames(&req, &reply, &meta, delivery.chunk_tokens, delivery.args_chunk_bytes);
                        sse::respond(
                            control.clock.clone(),
                            delivery,
                            frames,
                            vec![("x-request-id".into(), request_id)],
                            None,
                        )
                    } else {
                        // Stored before the latency wait, like the streaming path, so a reset during
                        // the wait cannot be undone by a late insert.
                        control.responses.insert_opt(stored);
                        wait(&control.clock, sync_latency(&reply, &delivery)).await;
                        sync_done(json_response(200, &responses::render_sync(&req, &reply, &meta)), &delivery).await
                    }
                }
            },
            Outcome::Hang { .. } => unreachable!("Engine::resolve never nests a Hang"),
        }
    }
    .await;
    guard.record = None;
    response
}

/// The empty reply `Engine::resolve` builds for a rule that only says `hang: true`.
fn is_placeholder(outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::Reply { reply, .. } if reply.text.as_deref().is_none_or(str::is_empty) && reply.tool_calls.is_empty())
}

fn response_meta(control: &crate::control::Control, req: &Request, unique: u64) -> responses::ResponseMeta {
    responses::ResponseMeta {
        id: fallback::responses_id("resp", unique, 0),
        created: control.clock.unix_secs(),
        model: req.model.clone(),
        seed: unique,
    }
}

/// Chat ends its streams with `data: [DONE]`; the other protocols end with their last event.
fn sentinel(api: Api) -> Option<&'static str> {
    (api == Api::Chat).then_some("[DONE]")
}

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

async fn sync_done(resp: crate::Response, delivery: &crate::ir::Delivery) -> crate::Response {
    if delivery.reset { sse::reset_body(resp).await } else { resp }
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
        assert!(matches!(route(&M::POST, "/v1/messages"), Route::Messages));
        assert!(matches!(route(&M::POST, "/messages"), Route::Messages));
        assert!(matches!(route(&M::POST, "/v1/messages/count_tokens"), Route::CountTokens));
        assert!(matches!(route(&M::POST, "/v1/responses"), Route::Responses));
        assert!(matches!(route(&M::GET, "/v1/responses/resp_1"), Route::ResponseGet("resp_1")));
        assert!(matches!(route(&M::DELETE, "/v1/responses/resp_1"), Route::ResponseDelete("resp_1")));
        assert!(matches!(route(&M::GET, "/v1/responses"), Route::Unknown));
        assert!(matches!(route(&M::GET, "/v1/responses/a/input_items"), Route::Unknown));
        assert!(matches!(route(&M::DELETE, "/v1/responses/"), Route::Unknown));
    }

    #[test]
    fn styles() {
        use http::Method as M;
        let none = http::HeaderMap::new();
        let mut anthropic = http::HeaderMap::new();
        anthropic.insert("anthropic-version", "2023-06-01".parse().unwrap());
        let st = |m: &M, p: &str, h: &http::HeaderMap| style(&route(m, p), p, h);
        assert_eq!(st(&M::POST, "/v1/messages", &none), Style::Anthropic);
        assert_eq!(st(&M::POST, "/v1/messages/count_tokens", &none), Style::Anthropic);
        assert_eq!(st(&M::GET, "/v1/models", &none), Style::Openai);
        assert_eq!(st(&M::GET, "/v1/models", &anthropic), Style::Anthropic);
        assert_eq!(st(&M::GET, "/v1/nope", &anthropic), Style::Anthropic);
        assert_eq!(st(&M::POST, "/v1/chat/completions", &anthropic), Style::Openai);
        assert_eq!(st(&M::POST, "/v1/responses", &anthropic), Style::Openai);
    }

    #[test]
    fn api_paths() {
        assert!(is_api_path("/v1/anything"));
        assert!(is_api_path("/chat/completions"));
        assert!(is_api_path("/messages/count_tokens"));
        assert!(is_api_path("/responses/resp_1"));
        assert!(!is_api_path("/responses_x"));
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
