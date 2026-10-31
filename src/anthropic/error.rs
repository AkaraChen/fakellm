//! Anthropic's error body: `{"type": "error", "error": {"type", "message"}}`. Only those two keys
//! under `error`; `code` and `param` of the IR error are dropped. Extra headers and
//! `retry-after` are handled by `crate::error_response`.

use serde_json::{Value, json};

use crate::Response;
use crate::ir::ApiError;

pub fn body(err: &ApiError) -> Value {
    json!({"type": "error", "error": {"type": err.kind, "message": err.message}})
}

/// Status, JSON body from [`body`], `retry-after`, `err.headers`.
pub fn render(err: &ApiError) -> Response {
    crate::error_response(err, &body(err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Protocol;

    #[test]
    fn body_shape() {
        let err = ApiError::for_status_in(Protocol::AnthropicMessages, 401);
        assert_eq!(
            body(&err),
            json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}})
        );
        let r = render(&ApiError::for_status_in(Protocol::AnthropicMessages, 529).header("x-should-retry", "true"));
        assert_eq!(r.status().as_u16(), 529);
        assert_eq!(r.headers()["x-should-retry"], "true");
    }
}
