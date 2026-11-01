//! Converter for WildChat rows (allenai/WildChat-1M and kin): `conversation: [{role, content, ...}]`, `model`,
//! `conversation_hash` as the id, and `language`, `toxic`, `redacted`, `timestamp` kept in `meta`.
//! The mapping tables are in docs/m4-spec.md section 6.2.

use serde_json::Value;

use crate::import::{Builder, Conversation, array_field, copy_meta, row_id, str_of, value_text};
use crate::ir::Role;

/// Whether the row has this format's shape (used by `import::detect`).
pub fn matches(row: &Value) -> bool {
    array_field(row, "conversation").is_some_and(|turns| turns.iter().any(|t| t.get("role").is_some()))
}

/// Converts one row. `Err` carries the skip reason (`"no conversation field"`, ...).
pub fn convert(row: &Value, index: usize) -> Result<Conversation, String> {
    let turns = array_field(row, "conversation").ok_or("no conversation field")?;
    let mut b = Builder::default();
    for turn in &turns {
        let text = turn.get("content").map(value_text).unwrap_or_default();
        match str_of(turn, "role").map(str::to_ascii_lowercase).as_deref() {
            Some("system") => b.text(Role::System, &text),
            Some("developer") => b.text(Role::Developer, &text),
            Some("user") => b.text(Role::User, &text),
            Some("assistant") => b.assistant(&text, None, Vec::new()),
            _ => {}
        }
    }
    let mut meta = serde_json::Map::new();
    copy_meta(row, &["language", "toxic", "redacted", "timestamp"], &mut meta);
    let model = str_of(row, "model").filter(|m| !m.is_empty()).map(str::to_owned);
    b.finish(
        row_id(row, &["conversation_hash", "id"], index),
        model,
        Vec::new(),
        Default::default(),
        meta,
    )
}
