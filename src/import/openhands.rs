//! Converter for OpenHands event streams: `events` (or `history`, `trajectory`): `[{id, source, action, args, observation, content, cause}]`.
//! Actions become tool calls, observations their results. The mapping tables are in docs/m4-spec.md section 6.2.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::import::{Builder, CallSpec, Conversation, array_field, copy_meta, row_id, str_of, value_text};
use crate::ir::Role;

const KEYS: [&str; 3] = ["events", "history", "trajectory"];

fn events(row: &Value) -> Option<Vec<Value>> {
    KEYS.iter().find_map(|k| array_field(row, k))
}

/// Whether the row has this format's shape (used by `import::detect`).
pub fn matches(row: &Value) -> bool {
    array_field(row, "events").is_some()
        || ["history", "trajectory"].iter().any(|k| {
            array_field(row, k).is_some_and(|items| {
                items
                    .iter()
                    .any(|i| i.get("source").is_some() && (i.get("action").is_some() || i.get("observation").is_some()))
            })
        })
}

/// Converts one row. `Err` carries the skip reason (`"no events field"`, ...).
pub fn convert(row: &Value, index: usize) -> Result<Conversation, String> {
    let events = events(row).ok_or("no events field")?;
    let mut b = Builder::default();
    // Event id -> the call that action made, and the last call, for observations without a `cause`.
    let mut call_of: HashMap<String, String> = HashMap::new();
    let mut last_call: Option<String> = None;
    let mut thought: Option<String> = None;
    let mut answered: Vec<String> = Vec::new();
    for event in &events {
        let id = event.get("id").map(|i| i.to_string().trim_matches('"').to_owned());
        let empty = json!({});
        let args = event.get("args").filter(|a| a.is_object()).unwrap_or(&empty);
        let source = str_of(event, "source").unwrap_or_default();
        if let Some(action) = str_of(event, "action").filter(|a| *a != "null") {
            let message = || {
                ["content", "message"]
                    .iter()
                    .find_map(|k| str_of(args, k).filter(|t| !t.is_empty()))
                    .or_else(|| str_of(event, "message"))
                    .or_else(|| str_of(event, "content"))
                    .unwrap_or_default()
                    .to_owned()
            };
            let call = |name: &str, arguments: Value| CallSpec {
                id: None,
                name: name.to_owned(),
                arguments: arguments.to_string(),
            };
            let (text, call) = match action {
                "system" => {
                    b.text(Role::System, &message());
                    continue;
                }
                "message" if source == "user" => {
                    b.text(Role::User, &message());
                    continue;
                }
                "message" => {
                    let reasoning = thought.take();
                    b.assistant(&message(), reasoning, Vec::new());
                    continue;
                }
                "finish" => {
                    let text = ["final_thought", "message", "content"]
                        .iter()
                        .find_map(|k| str_of(args, k).filter(|t| !t.is_empty()))
                        .unwrap_or_default();
                    let reasoning = thought.take();
                    b.assistant(text, reasoning, Vec::new());
                    continue;
                }
                "think" => {
                    if let Some(t) = str_of(args, "thought")
                        .or_else(|| str_of(args, "content"))
                        .filter(|t| !t.is_empty())
                    {
                        thought = Some(match thought.take() {
                            Some(prev) => format!("{prev}\n{t}"),
                            None => t.to_owned(),
                        });
                    }
                    continue;
                }
                "run" => (str_of(args, "thought"), call("execute_bash", pick(args, &["command"]))),
                "edit" => (
                    str_of(args, "thought"),
                    call(
                        "str_replace_editor",
                        pick(args, &["command", "path", "old_str", "new_str", "file_text", "insert_line"]),
                    ),
                ),
                "read" => {
                    let path = str_of(args, "path").unwrap_or_default();
                    (
                        str_of(args, "thought"),
                        call("str_replace_editor", json!({ "command": "view", "path": path })),
                    )
                }
                other => (
                    str_of(args, "thought"),
                    call(other, Value::Object(args.as_object().cloned().unwrap_or_default())),
                ),
            };
            let reasoning = thought.take();
            let before = b.messages.len();
            b.assistant(text.unwrap_or_default(), reasoning, vec![call]);
            if b.messages.len() > before
                && let Some(made) = b.messages.last().and_then(|m| m.tool_calls.last()).map(|c| c.id.clone())
            {
                if let Some(id) = id {
                    call_of.insert(id, made.clone());
                }
                last_call = Some(made);
            }
        } else if event.get("observation").is_some_and(|o| o.as_str() != Some("null")) {
            let cause = event.get("cause").map(|c| c.to_string().trim_matches('"').to_owned());
            let target = cause.and_then(|c| call_of.get(&c).cloned()).or_else(|| last_call.clone());
            let Some(target) = target else { continue };
            if answered.contains(&target) {
                continue;
            }
            let text = event.get("content").map(value_text).unwrap_or_default();
            if b.tool(Some(&target), &text) {
                answered.push(target);
            }
        }
    }
    let mut meta = Map::new();
    copy_meta(row, &["instance_id"], &mut meta);
    b.finish(
        row_id(row, &["instance_id", "id"], index),
        None,
        Vec::new(),
        Default::default(),
        meta,
    )
}

/// The listed arguments that are present.
fn pick(args: &Value, keys: &[&str]) -> Value {
    let mut out = Map::new();
    for key in keys {
        if let Some(v) = args.get(*key).filter(|v| !v.is_null()) {
            out.insert((*key).to_owned(), v.clone());
        }
    }
    Value::Object(out)
}
