//! OpenAI's error body: `{"error": {"message", "type", "param", "code"}}`, all four keys always
//! present (`null` when unset). `retry-after` goes on 429s that ask for it.

use serde_json::json;

use crate::Response;
use crate::ir::ApiError;

pub fn body(err: &ApiError) -> serde_json::Value {
    json!({"error": {"message": err.message, "type": err.kind, "param": err.param, "code": err.code}})
}

/// The full HTTP response: status, JSON body, `retry-after` when set, then `err.headers`.
pub fn render(err: &ApiError) -> Response {
    crate::error_response(err, &body(err))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn body_has_all_four_keys() {
        let v = body(&ApiError::server_error());
        assert_eq!(v["error"]["type"], "server_error");
        assert!(v["error"]["param"].is_null() && v["error"]["code"].is_null());
        let v = body(&ApiError::missing_parameter("messages"));
        assert_eq!(v["error"]["param"], "messages");
        assert_eq!(v["error"]["code"], "missing_required_parameter");
    }

    #[test]
    fn render_sets_headers() {
        let r = render(&ApiError::rate_limited().retry_after(Duration::from_millis(1500)));
        assert_eq!(r.status(), 429);
        assert_eq!(r.headers()["content-type"], "application/json");
        assert_eq!(r.headers()["retry-after"], "2");
        assert!(r.headers().contains_key("content-length"));
        let r = render(&ApiError::for_status(529));
        assert_eq!(r.status().as_u16(), 529);
        assert!(!r.headers().contains_key("retry-after"));
    }

    #[test]
    fn render_adds_custom_headers() {
        let r = render(
            &ApiError::overloaded()
                .header("x-should-retry", "false")
                .header("retry-after-ms", "250"),
        );
        assert_eq!(r.headers()["x-should-retry"], "false");
        assert_eq!(r.headers()["retry-after-ms"], "250");
        let r = render(&ApiError::overloaded().header("bad name", "x"));
        assert_eq!(r.status(), 503);
    }
}
