//! Converter for Salesforce/xlam-function-calling-60k rows: `id`, `query` (the user request), `answers` (a JSON string:
//! `[{name, arguments}]`, the calls to make) and `tools` (a JSON string: `[{name, description, parameters}]` where
//! `parameters` maps each argument name to `{type, description, default?}` instead of a JSON schema). Single turn: one
//! user message and one assistant message holding all the calls.
//!
//! Written from the dataset card's schema: the dataset is gated, and no token was available when this was added, so no
//! real row has been seen. `tests/fixtures/hf/xlam.jsonl` is synthetic.

use serde_json::{Map, Value, json};

use crate::import::{Builder, CallSpec, Conversation, arguments_string, array_field, row_id, str_of};
use crate::ir::{Role, ToolDef};

/// Whether the row has a `query` string and an `answers` list.
pub fn matches(row: &Value) -> bool {
    row.get("query").is_some_and(Value::is_string) && array_field(row, "answers").is_some()
}

/// Converts one row. `Err` carries the skip reason.
pub fn convert(row: &Value, index: usize) -> Result<Conversation, String> {
    let query = str_of(row, "query").ok_or("no query field")?;
    let answers = array_field(row, "answers").ok_or("no answers field")?;
    let calls: Vec<CallSpec> = answers
        .iter()
        .filter_map(|a| {
            Some(CallSpec {
                id: None,
                name: str_of(a, "name")?.to_owned(),
                arguments: arguments_string(a.get("arguments")),
            })
        })
        .collect();
    let mut b = Builder::default();
    b.text(Role::User, query);
    b.assistant("", None, calls);
    b.finish(row_id(row, &["id"], index), None, tool_defs(row), Default::default(), Map::new())
}

/// The `tools` column, with xlam's `parameters` map turned into a JSON schema object. A parameter whose `type`
/// mentions `optional` (`"str, optional"`) or that has a `default` is not required.
fn tool_defs(row: &Value) -> Vec<ToolDef> {
    array_field(row, "tools")
        .unwrap_or_default()
        .iter()
        .filter_map(|t| {
            let mut properties = Map::new();
            let mut required = Vec::new();
            if let Some(Value::Object(params)) = t.get("parameters") {
                for (name, p) in params {
                    let ty = str_of(p, "type").unwrap_or("string");
                    let mut prop = Map::new();
                    prop.insert("type".into(), json!(json_type(ty)));
                    if let Some(d) = str_of(p, "description") {
                        prop.insert("description".into(), json!(d));
                    }
                    if let Some(d) = p.get("default").filter(|d| !d.is_null() && **d != json!("")) {
                        prop.insert("default".into(), d.clone());
                    } else if !ty.contains("optional") {
                        required.push(json!(name));
                    }
                    properties.insert(name.clone(), Value::Object(prop));
                }
            }
            Some(ToolDef {
                name: str_of(t, "name")?.to_owned(),
                description: str_of(t, "description").map(str::to_owned),
                parameters: Some(json!({"type": "object", "properties": properties, "required": required})),
                strict: None,
            })
        })
        .collect()
}

/// Python-flavoured type names (`str`, `int`, `List[str]`, `float, optional`) to JSON schema types.
fn json_type(ty: &str) -> &'static str {
    let t = ty.split(',').next().unwrap_or("").trim().to_ascii_lowercase();
    match t.as_str() {
        "int" | "integer" => "integer",
        "float" | "number" | "double" => "number",
        "bool" | "boolean" => "boolean",
        t if t.starts_with("list") || t.starts_with("array") || t.starts_with("tuple") || t.starts_with("set") => "array",
        t if t.starts_with("dict") || t.starts_with("object") => "object",
        _ => "string",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_the_documented_shape() {
        let row = json!({
            "id": 7,
            "query": "Where can I find live giveaways?",
            "answers": "[{\"name\": \"live_giveaways_by_type\", \"arguments\": {\"type\": \"beta\"}}]",
            "tools": "[{\"name\": \"live_giveaways_by_type\", \"description\": \"Retrieve giveaways.\", \"parameters\": {\"type\": {\"type\": \"str\", \"description\": \"The type.\", \"default\": \"game\"}, \"limit\": {\"type\": \"int, optional\", \"description\": \"max\", \"default\": \"\"}, \"q\": {\"type\": \"List[str]\", \"description\": \"x\"}}}]"
        });
        assert!(matches(&row));
        let c = convert(&row, 0).unwrap();
        assert_eq!(c.id, "7");
        assert_eq!(c.messages.len(), 2);
        let schema = c.tools[0].parameters.clone().unwrap();
        assert_eq!(schema["properties"]["limit"]["type"], "integer");
        assert_eq!(schema["properties"]["q"]["type"], "array");
        assert_eq!(schema["required"], json!(["q"]));
        assert_eq!(c.messages[1].tool_calls[0].name, "live_giveaways_by_type");
    }
}
