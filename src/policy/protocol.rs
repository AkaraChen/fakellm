//! The stdio protocol between fakellm and a policy shim: newline-delimited JSON, one object per line,
//! UTF-8, stdout only (stderr is the policy's log). Documented for shim authors in docs/m3-spec.md.
//!
//! ```text
//! shim -> fakellm   {"ready":true}                      module imported, waiting for work
//!                   {"load_error":"<message>"}          module failed to import; the shim exits
//! fakellm -> shim   {"id":7,"request":<IR Request>,"state":<JSON>,"hits":3}
//! shim -> fakellm   {"id":7,"reply":<ReplySpec>,"state":<JSON>}   `state` only when the module returned one
//!                   {"id":7,"decline":true,"state":<JSON>}        decide returned null/undefined/{decline:true}: not mine
//!                   {"id":7,"error":"<message>"}                  decide threw or rejected
//! ```
//!
//! Decisions are sent one at a time, so the shim may process lines sequentially; `id` still lets
//! fakellm discard a late answer from before a timeout.

use serde::Serialize;
use serde_json::Value;

use crate::ir::Request;
use crate::script::ReplySpec;

/// A decision request, as written to the shim.
#[derive(Serialize)]
pub struct DecideLine<'a> {
    pub id: u64,
    pub request: &'a Request,
    pub state: &'a Value,
    /// Decisions this policy made before this one.
    pub hits: u64,
}

/// A line read from the shim.
#[derive(Debug, PartialEq)]
#[allow(clippy::large_enum_variant)] // one value per decision, read once
pub enum Incoming {
    Ready,
    LoadError(String),
    Decided {
        id: u64,
        /// The reply, or the shim's error message. A `reply` that is not a valid `ReplySpec` is an `Err` here.
        /// `Ok(None)` is a decline.
        outcome: Result<Option<ReplySpec>, String>,
        state: Option<Value>,
    },
}

/// `line` plus the trailing `\n`.
pub fn encode(line: &DecideLine<'_>) -> String {
    let mut out = serde_json::to_string(line).expect("a request serializes");
    out.push('\n');
    out
}

/// Parses one stdout line; `Err` for anything that is not one of the shapes above.
pub fn parse(line: &str) -> Result<Incoming, String> {
    let mut doc: Value = serde_json::from_str(line).map_err(|e| format!("not JSON: {e}"))?;
    let obj = doc.as_object_mut().ok_or("not a JSON object")?;
    if obj.get("ready") == Some(&Value::Bool(true)) {
        return Ok(Incoming::Ready);
    }
    if let Some(message) = obj.get("load_error") {
        return Ok(Incoming::LoadError(
            message.as_str().map_or_else(|| message.to_string(), str::to_owned),
        ));
    }
    let id = obj.get("id").and_then(Value::as_u64).ok_or("no `id`")?;
    let state = obj.remove("state");
    let outcome = if let Some(error) = obj.get("error") {
        Err(error.as_str().map_or_else(|| error.to_string(), str::to_owned))
    } else if obj.get("decline") == Some(&Value::Bool(true)) {
        Ok(None)
    } else if let Some(reply) = obj.remove("reply") {
        serde_json::from_value::<ReplySpec>(reply)
            .map(Some)
            .map_err(|e| format!("invalid reply: {e}"))
    } else {
        return Err("neither `reply` nor `error`".into());
    };
    Ok(Incoming::Decided { id, outcome, state })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shim_lines() {
        assert_eq!(parse(r#"{"ready":true}"#), Ok(Incoming::Ready));
        assert_eq!(parse(r#"{"load_error":"boom"}"#), Ok(Incoming::LoadError("boom".into())));
        match parse(r#"{"id":3,"reply":{"text":"hi"},"state":{"n":1}}"#).unwrap() {
            Incoming::Decided { id, outcome, state } => {
                assert_eq!(
                    (id, outcome.unwrap().unwrap().text.as_deref(), state),
                    (3, Some("hi"), Some(serde_json::json!({"n": 1})))
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse(r#"{"id":4,"error":"bad"}"#),
            Ok(Incoming::Decided { id: 4, outcome: Err(m), .. }) if m == "bad"
        ));
        assert!(matches!(
            parse(r#"{"id":5,"reply":{"bogus":1}}"#),
            Ok(Incoming::Decided { outcome: Err(_), .. })
        ));
        assert!(matches!(
            parse(r#"{"id":6,"decline":true,"state":{"k":1}}"#),
            Ok(Incoming::Decided {
                id: 6,
                outcome: Ok(None),
                state: Some(_)
            })
        ));
        assert!(parse("hello").is_err() && parse("[1]").is_err() && parse(r#"{"id":1}"#).is_err());
    }
}
