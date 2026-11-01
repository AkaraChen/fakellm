//! `POST /v1/embeddings`: deterministic fake vectors, so code that stores, compares or dimension-checks
//! embeddings runs against fakellm without a model.
//!
//! A vector is a pure function of `(model, input)`: an FNV-1a hash seeds a splitmix64 stream, the
//! first `dimensions` components are taken as uniform values in `[-1, 1)` and normalized to unit
//! length. Distinct inputs give distinct vectors, equal inputs equal vectors, and a shorter
//! `dimensions` is the prefix of the longer vector, renormalized (what the real `text-embedding-3`
//! models do). Scripts cannot override vectors; rules that answer with an `error` apply, so retry
//! and failure handling can be tested. See docs/m3-spec.md ("Embeddings").

use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::ir::{ApiError, Message, Outcome, Part, Protocol, Reply, Request, Role, Source, Usage};
use crate::{Response, Shared};

pub const DEFAULT_DIMENSIONS: usize = 1536;
pub const MAX_DIMENSIONS: usize = 8192;
/// Inputs per request; more is a 400, as in the real API.
pub const MAX_INPUTS: usize = 2048;

/// One element of `input`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Text(String),
    /// A pre-tokenized input (`number[]`); counts as that many tokens and is hashed by its numbers.
    Tokens(Vec<u32>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Float,
    /// Little-endian `f32` bytes, base64 (standard alphabet, padded).
    Base64,
}

/// A validated embeddings request.
#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    /// The IR for the request log: `Protocol::OpenaiEmbeddings`, one user message per input element
    /// (token arrays as `Part::Other`), `dimensions` and `encoding_format` left in `extra` as sent.
    pub request: Request,
    pub inputs: Vec<Input>,
    pub dimensions: usize,
    pub encoding: Encoding,
}

fn invalid_input() -> ApiError {
    ApiError::invalid_request("'$.input' is invalid. Please check the API reference: https://platform.openai.com/docs/api-reference.")
        .param("input")
}

fn token_array(items: &[Value]) -> Option<Vec<u32>> {
    if items.is_empty() {
        return None;
    }
    items.iter().map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok())).collect()
}

fn parse_inputs(value: &Value) -> Result<Vec<Input>, ApiError> {
    let text = |s: &str| {
        if s.is_empty() {
            Err(invalid_input())
        } else {
            Ok(Input::Text(s.to_owned()))
        }
    };
    let inputs = match value {
        Value::String(s) => vec![text(s)?],
        Value::Array(items) if items.is_empty() => return Err(invalid_input()),
        Value::Array(items) if items.iter().all(Value::is_number) => vec![Input::Tokens(token_array(items).ok_or_else(invalid_input)?)],
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => text(s),
                Value::Array(t) => token_array(t).map(Input::Tokens).ok_or_else(invalid_input),
                _ => Err(invalid_input()),
            })
            .collect::<Result<_, _>>()?,
        _ => return Err(invalid_input()),
    };
    if inputs.len() > MAX_INPUTS {
        return Err(ApiError::invalid_request(format!(
            "'$.input' is invalid: the array may contain at most {MAX_INPUTS} elements."
        ))
        .param("input"));
    }
    Ok(inputs)
}

/// Validates `{model, input, dimensions?, encoding_format?, user?}`. Errors are the 400s the real API gives:
/// missing `model` / `input` (`missing_required_parameter`), an empty input list or empty string, more than
/// [`MAX_INPUTS`] inputs, a non-integer or out-of-range `dimensions` (1..=[`MAX_DIMENSIONS`]), an
/// `encoding_format` other than `float` and `base64`; all `invalid_request_error` with `param` set.
pub fn parse(body: &Value) -> Result<Parsed, ApiError> {
    let Some(obj) = body.as_object() else {
        return Err(ApiError::invalid_request("Invalid request body: expected a JSON object."));
    };
    let present = |k: &str| obj.get(k).filter(|v| !v.is_null());
    let model = match present("model") {
        None => return Err(ApiError::missing_parameter("model")),
        Some(Value::String(s)) => s.clone(),
        Some(_) => {
            return Err(ApiError::invalid_request("Invalid type for 'model': expected a string.")
                .code("invalid_type")
                .param("model"));
        }
    };
    let inputs = parse_inputs(present("input").ok_or_else(|| ApiError::missing_parameter("input"))?)?;
    let dimensions = match present("dimensions") {
        None => DEFAULT_DIMENSIONS,
        Some(v) => match v.as_u64() {
            Some(n) if (1..=MAX_DIMENSIONS as u64).contains(&n) => n as usize,
            _ => {
                return Err(ApiError::invalid_request(format!(
                    "Invalid 'dimensions': expected an integer between 1 and {MAX_DIMENSIONS}."
                ))
                .code("invalid_value")
                .param("dimensions"));
            }
        },
    };
    let encoding = match present("encoding_format").map(|v| v.as_str()) {
        None | Some(Some("float")) => Encoding::Float,
        Some(Some("base64")) => Encoding::Base64,
        Some(_) => {
            return Err(
                ApiError::invalid_request("Invalid 'encoding_format': expected one of 'float' or 'base64'.")
                    .code("invalid_value")
                    .param("encoding_format"),
            );
        }
    };

    let messages = inputs
        .iter()
        .map(|input| match input {
            Input::Text(t) => Message::text(Role::User, t.clone()),
            Input::Tokens(t) => Message {
                content: vec![Part::Other { value: json!(t) }],
                ..Message::text(Role::User, "")
            },
        })
        .collect();
    let mut request = Request::new(model, messages);
    request.protocol = Protocol::OpenaiEmbeddings;
    request.user = present("user").and_then(Value::as_str).map(str::to_owned);
    let mut extra = Map::new();
    for key in ["dimensions", "encoding_format"] {
        if let Some(v) = present(key) {
            extra.insert(key.into(), v.clone());
        }
    }
    request.extra = extra;
    Ok(Parsed {
        request,
        inputs,
        dimensions,
        encoding,
    })
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The unit vector for `(model, input)` with `dimensions` components.
pub fn vector(model: &str, input: &Input, dimensions: usize) -> Vec<f32> {
    let mut seed_bytes = model.as_bytes().to_vec();
    seed_bytes.push(0xff);
    match input {
        Input::Text(t) => {
            seed_bytes.push(b'T');
            seed_bytes.extend_from_slice(t.as_bytes());
        }
        Input::Tokens(t) => {
            seed_bytes.push(b'K');
            let joined = t.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
            seed_bytes.extend_from_slice(joined.as_bytes());
        }
    }
    let mut state = crate::fallback::hash64(&seed_bytes);
    let mut v: Vec<f32> = (0..dimensions)
        .map(|_| (splitmix64(&mut state) >> 40) as f32 / (1u32 << 23) as f32 - 1.0)
        .collect();
    let norm = v.iter().map(|&x| f64::from(x) * f64::from(x)).sum::<f64>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x = (f64::from(*x) / norm) as f32;
        }
    }
    v
}

/// Tokens the input counts for in `usage`: the tokenizer's count for text (at least 1), the length for token arrays.
pub fn input_tokens(input: &Input) -> u32 {
    match input {
        Input::Text(t) => crate::tokenizer::count(t).max(1),
        Input::Tokens(t) => t.len() as u32,
    }
}

fn float_json(x: f32) -> Value {
    // Shortest round-trip form of the f32, so the JSON does not carry f64 noise digits.
    x.to_string()
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map_or(Value::Null, Value::Number)
}

/// The response body: `{"object":"list","data":[{"object":"embedding","index":i,"embedding":[...]|"<base64>"}],
/// "model":<as sent>,"usage":{"prompt_tokens":n,"total_tokens":n}}`.
pub fn render(parsed: &Parsed) -> Value {
    let model = &parsed.request.model;
    let data: Vec<Value> = parsed
        .inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let v = vector(model, input, parsed.dimensions);
            let embedding = match parsed.encoding {
                Encoding::Float => Value::Array(v.iter().map(|&x| float_json(x)).collect()),
                Encoding::Base64 => Value::String(encode_base64(&v)),
            };
            json!({"object": "embedding", "index": index, "embedding": embedding})
        })
        .collect();
    let tokens: u32 = parsed.inputs.iter().map(input_tokens).sum();
    json!({
        "object": "list",
        "data": data,
        "model": model,
        "usage": {"prompt_tokens": tokens, "total_tokens": tokens},
    })
}

/// Standard padded base64 of the little-endian bytes of `vector`.
pub fn encode_base64(vector: &[f32]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = vector.iter().flat_map(|x| x.to_le_bytes()).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The endpoint after routing and the API-key check: parse (a failure is rejected like any 400),
/// log the IR, apply a matching `error` rule (`Engine::resolve_error`, delayed by its `delay_ms` on the
/// server clock), else answer 200 with [`render`]. The logged outcome is a `Reply` with only `usage` set and
/// `Source::Fallback`.
pub(super) async fn respond(shared: &Shared, bytes: &[u8], trace: &mut super::Trace) -> Response {
    let control = &shared.control;
    trace.seed = Some(crate::fallback::hash64(bytes));
    let Some(value) = super::note_body(trace, Some(bytes)) else {
        return super::reject(trace, super::Style::Openai, ApiError::invalid_request(super::JSON_PARSE_HINT));
    };
    let parsed = match parse(&value) {
        Ok(p) => p,
        Err(e) => return super::reject(trace, super::Style::Openai, e),
    };
    trace.request = Some(Arc::new(parsed.request.clone()));
    if let Some(outcome) = control.engine.resolve_error(&parsed.request) {
        trace.outcome = Some(Arc::new(outcome.clone()));
        if let Outcome::Error { error, delivery, .. } = outcome {
            super::wait(&control.clock, delivery.ttft).await;
            return super::error::render(&error);
        }
    }
    if control.upstream.is_some() {
        return super::proxy(shared, Arc::new(parsed.request), bytes, trace, super::Style::Openai).await;
    }
    let tokens: u32 = parsed.inputs.iter().map(input_tokens).sum();
    trace.outcome = Some(Arc::new(Outcome::Reply {
        reply: Reply {
            usage: Usage {
                prompt_tokens: tokens,
                ..Usage::default()
            },
            ..Reply::default()
        },
        delivery: Default::default(),
        source: Source::Fallback,
    }));
    crate::json_response(200, &render(&parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: Value) -> Result<Parsed, ApiError> {
        parse(&v)
    }

    #[test]
    fn base64_known_vectors() {
        assert_eq!(encode_base64(&[1.0]), "AACAPw==");
        assert_eq!(encode_base64(&[]), "");
        assert_eq!(encode_base64(&[1.0, 2.0]), "AACAPwAAAEA=");
    }

    #[test]
    fn vectors_are_unit_deterministic_and_prefix_stable() {
        let a = Input::Text("hello".into());
        let v = vector("m", &a, 64);
        assert_eq!(v, vector("m", &a, 64));
        assert_eq!(v.len(), 64);
        let norm: f64 = v.iter().map(|&x| f64::from(x).powi(2)).sum();
        assert!((norm - 1.0).abs() < 1e-5);
        assert_ne!(v, vector("m", &Input::Text("hellp".into()), 64));
        assert_ne!(v, vector("n", &a, 64));
        let short = vector("m", &a, 8);
        let ratio = short[0] / v[0];
        for i in 0..8 {
            assert!((short[i] / v[i] - ratio).abs() < 1e-4);
        }
    }

    #[test]
    fn parse_accepts_all_input_shapes() {
        assert_eq!(p(json!({"model":"m","input":"a"})).unwrap().inputs, vec![Input::Text("a".into())]);
        assert_eq!(p(json!({"model":"m","input":["a","b"]})).unwrap().inputs.len(), 2);
        assert_eq!(
            p(json!({"model":"m","input":[1,2]})).unwrap().inputs,
            vec![Input::Tokens(vec![1, 2])]
        );
        assert_eq!(p(json!({"model":"m","input":[[1],[2,3]]})).unwrap().inputs.len(), 2);
        let parsed = p(json!({"model":"m","input":"a","dimensions":8,"encoding_format":"base64","user":"u"})).unwrap();
        assert_eq!((parsed.dimensions, parsed.encoding), (8, Encoding::Base64));
        assert_eq!(parsed.request.protocol, Protocol::OpenaiEmbeddings);
        assert_eq!(parsed.request.user.as_deref(), Some("u"));
        assert_eq!(parsed.request.extra["dimensions"], 8);
    }

    #[test]
    fn parse_rejects_bad_requests() {
        let param = |v: Value| p(v).unwrap_err().param;
        assert_eq!(param(json!({"input":"a"})).as_deref(), Some("model"));
        assert_eq!(param(json!({"model":"m"})).as_deref(), Some("input"));
        assert_eq!(param(json!({"model":"m","input":""})).as_deref(), Some("input"));
        assert_eq!(param(json!({"model":"m","input":[]})).as_deref(), Some("input"));
        assert_eq!(param(json!({"model":"m","input":[1,"a"]})).as_deref(), Some("input"));
        assert_eq!(
            param(json!({"model":"m","input":vec!["a"; MAX_INPUTS + 1]})).as_deref(),
            Some("input")
        );
        assert_eq!(
            param(json!({"model":"m","input":"a","dimensions":0})).as_deref(),
            Some("dimensions")
        );
        assert_eq!(
            param(json!({"model":"m","input":"a","dimensions":1.5})).as_deref(),
            Some("dimensions")
        );
        assert_eq!(
            param(json!({"model":"m","input":"a","dimensions":9000})).as_deref(),
            Some("dimensions")
        );
        assert_eq!(
            param(json!({"model":"m","input":"a","encoding_format":"x"})).as_deref(),
            Some("encoding_format")
        );
    }

    #[test]
    fn render_shape_and_usage() {
        let parsed = p(json!({"model":"m","input":["hello world",[1,2,3]],"dimensions":4})).unwrap();
        let body = render(&parsed);
        assert_eq!(body["object"], "list");
        assert_eq!(body["model"], "m");
        assert_eq!(body["data"][1]["index"], 1);
        assert_eq!(body["data"][0]["embedding"].as_array().unwrap().len(), 4);
        let expected = input_tokens(&parsed.inputs[0]) + 3;
        assert_eq!(body["usage"], json!({"prompt_tokens": expected, "total_tokens": expected}));
    }

    #[test]
    fn render_base64_matches_floats() {
        let parsed = p(json!({"model":"m","input":"a","dimensions":5,"encoding_format":"base64"})).unwrap();
        let body = render(&parsed);
        assert_eq!(body["data"][0]["embedding"], encode_base64(&vector("m", &parsed.inputs[0], 5)));
    }

    #[test]
    fn float_json_round_trips_f32() {
        let x = 0.123_456_79_f32;
        assert_eq!(float_json(x).as_f64().unwrap() as f32, x);
        assert_eq!(float_json(0.5).to_string(), "0.5");
    }
}
