//! Converter for ShareGPT rows: `conversations: [{from, value}]` with `from` one of `system`, `human`/`user`, `gpt`/`assistant`, `tool`/`observation`, `function_call`. Id: `id`.
//! The mapping tables are in docs/m4-spec.md section 6.2.

use serde_json::Value;

use crate::import::{Builder, CallSpec, Conversation, arguments_string, array_field, row_id, str_of, value_text};
use crate::ir::Role;

/// Whether the row has this format's shape (used by `import::detect`).
pub fn matches(row: &Value) -> bool {
    array_field(row, "conversations").is_some_and(|turns| turns.iter().any(|t| t.get("from").is_some()))
}

/// Converts one row. `Err` carries the skip reason (`"no conversation field"`, ...).
pub fn convert(row: &Value, index: usize) -> Result<Conversation, String> {
    let turns = array_field(row, "conversations").ok_or("no conversations field")?;
    let mut b = Builder::default();
    for turn in &turns {
        let Some(from) = str_of(turn, "from") else { continue };
        let text = turn.get("value").map(value_text).unwrap_or_default();
        match from.to_ascii_lowercase().as_str() {
            "system" if b.messages.is_empty() => b.text(Role::System, &text),
            "human" | "user" => b.text(Role::User, &text),
            "gpt" | "assistant" | "chatgpt" | "bing" | "bard" | "model" => b.assistant(&text, None, Vec::new()),
            "function_call" => match serde_json::from_str::<Value>(&text) {
                Ok(call) if call.get("name").is_some_and(Value::is_string) => {
                    let name = str_of(&call, "name").unwrap_or_default().to_owned();
                    let arguments = arguments_string(call.get("arguments"));
                    b.assistant("", None, vec![CallSpec { id: None, name, arguments }]);
                }
                _ => b.assistant(&text, None, Vec::new()),
            },
            "tool" | "observation" | "function_response" | "function" => {
                b.tool(None, &text);
            }
            _ => {}
        }
    }
    b.finish(
        row_id(row, &["id"], index),
        None,
        Vec::new(),
        Default::default(),
        Default::default(),
    )
}
