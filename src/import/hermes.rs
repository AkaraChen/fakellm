//! Converter for Hermes function-calling rows (NousResearch/hermes-function-calling-v1): `conversations: [{from, value}]` where `gpt` turns hold `<tool_call>{json}</tool_call>` blocks and `tool` turns hold `<tool_response>...</tool_response>`; the `tools` column (a JSON string) gives the tool definitions. Id: `id`.
//! The mapping tables are in docs/m4-spec.md section 6.2.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::import::{Builder, CallSpec, Conversation, arguments_string, array_field, copy_meta, row_id, str_of, value_text};
use crate::ir::{Role, ToolDef};

static TOOL_CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<tool_call>(.*?)</tool_call>").expect("static regex"));
static TOOL_RESPONSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<tool_response>(.*?)</tool_response>").expect("static regex"));
static THINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<think>(.*?)</think>").expect("static regex"));
static TOOLS_BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<tools>.*?</tools>").expect("static regex"));

/// Whether the row has this format's shape (used by `import::detect`): `from`-style turns, and a `tools` column or a
/// `gpt` turn with a `<tool_call>` block.
pub fn matches(row: &Value) -> bool {
    let Some(turns) = array_field(row, "conversations") else {
        return false;
    };
    if !turns.iter().any(|t| t.get("from").is_some()) {
        return false;
    }
    row.get("tools").is_some_and(|t| !t.is_null())
        || turns
            .iter()
            .any(|t| str_of(t, "from") == Some("gpt") && t.get("value").map(value_text).is_some_and(|v| v.contains("<tool_call>")))
}

/// Converts one row. `Err` carries the skip reason (`"no conversation field"`, ...).
pub fn convert(row: &Value, index: usize) -> Result<Conversation, String> {
    let turns = array_field(row, "conversations").ok_or("no conversations field")?;
    let mut b = Builder::default();
    for turn in &turns {
        let Some(from) = str_of(turn, "from") else { continue };
        let text = turn.get("value").map(value_text).unwrap_or_default();
        match from.to_ascii_lowercase().as_str() {
            "system" if b.messages.is_empty() => {
                let text = TOOLS_BLOCK.replace_all(&text, "");
                b.text(Role::System, text.trim());
            }
            "human" | "user" => b.text(Role::User, &text),
            "gpt" | "assistant" => {
                let (text, reasoning) = split_think(&text);
                let (text, calls) = split_calls(&text);
                b.assistant(text.trim(), reasoning, calls);
            }
            "tool" | "observation" => {
                let blocks: Vec<String> = TOOL_RESPONSE.captures_iter(&text).map(|c| c[1].trim().to_owned()).collect();
                if blocks.is_empty() {
                    b.tool(None, text.trim());
                }
                for block in blocks {
                    b.tool(None, &block);
                }
            }
            _ => {}
        }
    }
    let mut meta = serde_json::Map::new();
    copy_meta(row, &["category", "subcategory", "task"], &mut meta);
    b.finish(row_id(row, &["id"], index), None, tool_defs(row), Default::default(), meta)
}

/// Text without its `<think>` blocks, and the blocks' content.
fn split_think(text: &str) -> (String, Option<String>) {
    let thoughts: Vec<String> = THINK
        .captures_iter(text)
        .map(|c| c[1].trim().to_owned())
        .filter(|t| !t.is_empty())
        .collect();
    let rest = THINK.replace_all(text, "").into_owned();
    (rest, (!thoughts.is_empty()).then(|| thoughts.join("\n")))
}

/// Text without its parseable `<tool_call>` blocks, and the calls. A block that is not a call stays in the text.
fn split_calls(text: &str) -> (String, Vec<CallSpec>) {
    let mut out = String::new();
    let mut calls = Vec::new();
    let mut last = 0;
    for m in TOOL_CALL.find_iter(text) {
        out.push_str(&text[last..m.start()]);
        last = m.end();
        let inner = &text[m.start() + "<tool_call>".len()..m.end() - "</tool_call>".len()];
        match lenient_json(inner).and_then(|v| call_of(&v)) {
            Some(call) => calls.push(call),
            None => out.push_str(m.as_str()),
        }
    }
    out.push_str(&text[last..]);
    (out, calls)
}

fn call_of(v: &Value) -> Option<CallSpec> {
    let name = str_of(v, "name")?.to_owned();
    Some(CallSpec {
        id: None,
        name,
        arguments: arguments_string(v.get("arguments")),
    })
}

/// The `tools` column: OpenAI function tools or bare `{name, description, parameters}`, in an array or a JSON string.
fn tool_defs(row: &Value) -> Vec<ToolDef> {
    array_field(row, "tools")
        .unwrap_or_default()
        .iter()
        .filter_map(|t| {
            let f = t.get("function").filter(|f| f.is_object()).unwrap_or(t);
            Some(ToolDef {
                name: str_of(f, "name")?.to_owned(),
                description: str_of(f, "description").map(str::to_owned),
                parameters: f.get("parameters").filter(|p| !p.is_null()).cloned(),
                strict: None,
            })
        })
        .collect()
}

/// JSON, or the Python-dict spelling of it (single quotes, `True` / `False` / `None`).
pub(crate) fn lenient_json(s: &str) -> Option<Value> {
    let s = s.trim();
    if let Ok(v) = serde_json::from_str(s) {
        return Some(v);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                out.push('"');
                loop {
                    match chars.next()? {
                        '\\' => match chars.next()? {
                            '\'' => out.push('\''),
                            e => {
                                out.push('\\');
                                out.push(e);
                            }
                        },
                        '"' if c == '\'' => out.push_str("\\\""),
                        '\n' => out.push_str("\\n"),
                        '\t' => out.push_str("\\t"),
                        q if q == c => break,
                        other => out.push(other),
                    }
                }
                out.push('"');
            }
            c if c.is_ascii_alphabetic() => {
                let mut word = String::from(c);
                while let Some(n) = chars.peek().filter(|n| n.is_ascii_alphanumeric() || **n == '_') {
                    word.push(*n);
                    chars.next();
                }
                out.push_str(match word.as_str() {
                    "True" => "true",
                    "False" => "false",
                    "None" => "null",
                    other => other,
                });
            }
            other => out.push(other),
        }
    }
    serde_json::from_str(&out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_dicts_parse_like_json() {
        let v = lenient_json("{'name': 'f', 'arguments': {'a': True, 'b': None, 'c': \"it's\", 'd': 'say \"x\"'}}").unwrap();
        assert_eq!(v["arguments"]["a"], true);
        assert!(v["arguments"]["b"].is_null());
        assert_eq!(v["arguments"]["c"], "it's");
        assert_eq!(v["arguments"]["d"], "say \"x\"");
        assert!(lenient_json("not a dict").is_none());
    }
}
