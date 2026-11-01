//! Converter for SWE-agent trajectories: nebius/SWE-agent-trajectories (`trajectory: [{role: system|user|ai, text}]`,
//! the action is a fenced code block in the `ai` text, the rest of the text is the "discussion") and
//! SWE-bench/SWE-smith-trajectories (`messages: [{role, content, tool_calls?}]` with structured calls and `tool`
//! results). Both end up as `bash` / `str_replace_editor` calls. The mapping tables are in docs/m4-spec.md section 6.2.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::agent::tool_map::{ToolMap, ToolMapping};
use crate::import::{Builder, CallSpec, Conversation, arguments_string, array_field, copy_meta, row_id, str_of, value_text};
use crate::ir::Role;

static FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)```[^\n`]*\n(.*?)\n?```").expect("static regex"));

/// The tool map every SWE-agent conversation carries: `bash {command}` and `str_replace_editor {command, path,
/// file_text, old_str, new_str, insert_line, view_range}`, each mapped to a tool of the same name with every argument
/// spelled out, so the file shows what to edit to point the trajectory at a client's own tools. Arguments a call does
/// not have are left out (see `$field` in docs/m3-spec.md section 3.5).
pub fn default_tool_map() -> ToolMap {
    let identity = |name: &str, fields: &[&str]| ToolMapping {
        name: name.into(),
        args: fields.iter().map(|f| ((*f).to_owned(), json!(format!("${f}")))).collect(),
        keep_rest: false,
    };
    ToolMap(
        [
            ("bash".to_owned(), identity("bash", &["command"])),
            (
                "str_replace_editor".to_owned(),
                identity(
                    "str_replace_editor",
                    &["command", "path", "file_text", "old_str", "new_str", "insert_line", "view_range"],
                ),
            ),
        ]
        .into_iter()
        .collect(),
    )
}

/// Whether the row has a `trajectory` list of `{role, text}` or a `messages` list holding `tool_calls`.
pub fn matches(row: &Value) -> bool {
    if let Some(items) = array_field(row, "trajectory")
        && items.iter().any(|i| str_of(i, "role") == Some("ai") || i.get("text").is_some())
    {
        return true;
    }
    array_field(row, "messages").is_some_and(|items| items.iter().any(|i| i.get("tool_calls").is_some_and(|c| !c.is_null())))
}

/// Converts one row. `Err` carries the skip reason.
pub fn convert(row: &Value, index: usize) -> Result<Conversation, String> {
    let mut b = Builder::default();
    if let Some(items) = array_field(row, "messages") {
        smith(&items, &mut b);
    } else if let Some(items) = array_field(row, "trajectory") {
        nebius(&items, &mut b);
    } else {
        return Err("no trajectory field".to_owned());
    }
    let mut meta = Map::new();
    copy_meta(
        row,
        &["instance_id", "traj_id", "model_name", "model", "exit_status", "target", "resolved"],
        &mut meta,
    );
    if let Some(patch) = str_of(row, "generated_patch") {
        meta.insert("generated_patch_len".into(), Value::from(patch.len()));
    }
    let model = str_of(row, "model_name")
        .or_else(|| str_of(row, "model"))
        .filter(|m| !m.is_empty())
        .map(str::to_owned);
    b.finish(
        row_id(row, &["instance_id", "traj_id"], index),
        model,
        Vec::new(),
        default_tool_map(),
        meta,
    )
}

/// `trajectory: [{role: system|user|ai, text}]`: an `ai` turn is a discussion and one fenced action.
fn nebius(items: &[Value], b: &mut Builder) {
    for item in items {
        let role = str_of(item, "role");
        let keys: &[&str] = if role == Some("system") {
            &["system_prompt", "text", "content"]
        } else {
            &["text", "content"]
        };
        let text = keys
            .iter()
            .filter_map(|k| item.get(*k))
            .filter(|v| !v.is_null())
            .map(value_text)
            .find(|t| !t.is_empty())
            .unwrap_or_default();
        match role {
            Some("system") => b.text(Role::System, &text),
            Some("user") if b.has_pending() => {
                b.tool(None, &text);
            }
            Some("user") => b.text(Role::User, &text),
            Some("ai" | "assistant") => {
                let (discussion, call) = split_action(&text);
                b.assistant(&discussion, None, call.into_iter().collect());
            }
            _ => {}
        }
    }
}

/// The text outside the first fenced block and the call that block stands for.
fn split_action(text: &str) -> (String, Option<CallSpec>) {
    let Some(m) = FENCE.captures(text) else {
        return (text.trim().to_owned(), None);
    };
    let whole = m.get(0).expect("group 0");
    let command = m[1].trim().to_owned();
    let discussion = [&text[..whole.start()], &text[whole.end()..]]
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let name = if command.split_whitespace().next() == Some("str_replace_editor") {
        "str_replace_editor"
    } else {
        "bash"
    };
    let call = CallSpec {
        id: None,
        name: name.to_owned(),
        arguments: json!({ "command": command }).to_string(),
    };
    (discussion, Some(call))
}

/// `messages: [{role, content, tool_calls?, tool_call_id?}]` with structured calls.
fn smith(items: &[Value], b: &mut Builder) {
    for item in items {
        let text = item.get("content").map(value_text).unwrap_or_default();
        match str_of(item, "role") {
            Some("system") => b.text(Role::System, &text),
            Some("user") => b.text(Role::User, &text),
            Some("assistant") => {
                let calls = array_field(item, "tool_calls")
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|c| {
                        let f = c.get("function").filter(|f| f.is_object()).unwrap_or(c);
                        Some(CallSpec {
                            id: str_of(c, "id").map(str::to_owned),
                            name: str_of(f, "name")?.to_owned(),
                            arguments: arguments_string(f.get("arguments")),
                        })
                    })
                    .collect();
                b.assistant(&text, None, calls);
            }
            Some("tool") => {
                b.tool(str_of(item, "tool_call_id"), &text);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_map_spells_out_the_arguments() {
        let map = default_tool_map();
        assert_eq!(map.client_name("bash"), "bash");
        assert_eq!(map.0["bash"].args["command"], "$command");
        assert!(map.0["str_replace_editor"].args.contains_key("old_str"));
        assert!(map.0.values().all(|m| !m.name.is_empty()));
    }
}
