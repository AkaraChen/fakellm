//! The Anthropic frontend: `POST /v1/messages` (sync and SSE), `POST /v1/messages/count_tokens`,
//! and `GET /v1/models` in Anthropic's list shape. Request parsing and the sync body live in
//! [`messages`], the event sequence in [`stream`], the error body in [`error`]. The request
//! pipeline (auth, body reading, scripts, timing, logging) is shared with the OpenAI endpoints
//! and lives in `openai/mod.rs`; see docs/m1-spec.md.

pub mod error;
pub mod messages;
pub mod stream;

use http::HeaderMap;
use serde_json::{Value, json};

use crate::ir::Request;

/// Whether the client identified itself as an Anthropic client (`anthropic-version` header).
/// Decides the shape of `GET /v1/models` and of 404s on unknown paths.
pub fn is_anthropic_client(headers: &HeaderMap) -> bool {
    headers.contains_key("anthropic-version")
}

/// `{"data": [{"type": "model", "id", "display_name", "created_at"}...], "has_more": false,
/// "first_id", "last_id"}`. `display_name` is the id; `created_at` is an RFC 3339 UTC string
/// derived from the name like `openai::models` does; `first_id`/`last_id` are `null` for an
/// empty list. No pagination: `limit`, `after_id` and `before_id` are ignored.
pub fn models_list(models: &[String]) -> Value {
    json!({
        "data": models.iter().map(|m| model(m)).collect::<Vec<_>>(),
        "has_more": false,
        "first_id": models.first(),
        "last_id": models.last(),
    })
}

/// One `{"type": "model", ...}` object, or `None` when the id is not in the list.
pub fn model_get(models: &[String], id: &str) -> Option<Value> {
    models.iter().any(|m| m == id).then(|| model(id))
}

fn model(id: &str) -> Value {
    let created = 1_700_000_000 + crate::fallback::hash64(id.as_bytes()) % 50_000_000;
    let created_at = time::OffsetDateTime::from_unix_timestamp(created as i64)
        .ok()
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_else(|| "2023-11-14T22:13:20Z".into());
    json!({"type": "model", "id": id, "display_name": id, "created_at": created_at})
}

/// The body of `POST /v1/messages/count_tokens`: `{"input_tokens": n}` with `n` from
/// `tokenizer::count_request(req)`, so it agrees with the `usage.input_tokens` a real call on
/// the same body would report.
pub fn count_tokens(req: &Request) -> Value {
    json!({"input_tokens": crate::tokenizer::count_request(req)})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_shape() {
        let ms = vec!["claude-sonnet-4-5".to_owned(), "claude-opus-4-1".to_owned()];
        let l = models_list(&ms);
        assert_eq!(l["has_more"], false);
        assert_eq!(l["first_id"], "claude-sonnet-4-5");
        assert_eq!(l["last_id"], "claude-opus-4-1");
        let m = &l["data"][0];
        assert_eq!(m["type"], "model");
        assert_eq!(m["display_name"], "claude-sonnet-4-5");
        let at = m["created_at"].as_str().unwrap();
        assert!(at.ends_with('Z') && at.len() == 20, "{at}");
        assert_eq!(model_get(&ms, "claude-opus-4-1").unwrap(), l["data"][1]);
        assert!(model_get(&ms, "nope").is_none());
        let empty = models_list(&[]);
        assert!(empty["first_id"].is_null() && empty["last_id"].is_null());
    }
}
