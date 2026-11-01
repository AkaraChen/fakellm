//! Tool mapping for trajectories: the trajectory says `bash {command}`, the client offers
//! `run_shell {cmd}`. A [`ToolMap`] renames the call and rebuilds its arguments, so one recorded
//! trajectory drives agents whose tools differ. See docs/m3-spec.md ("Tool mapping").
//!
//! Arguments stay in the shape the trajectory wrote them unless a mapping lists `args`. A string
//! argument that is not a JSON object (the deliberately broken arguments scripts can send) cannot
//! be remapped and is passed through verbatim, renamed tool and all.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::script::ToolCallSpec;

/// `{"<trajectory tool name>": {name, args?, keep_rest?}}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolMap(pub std::collections::BTreeMap<String, ToolMapping>);

/// How one trajectory tool becomes a client tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolMapping {
    /// The client's name for the tool.
    pub name: String,
    /// `{"<client arg>": <template>}`. A template is a JSON value: a string `"$cmd"` copies the
    /// trajectory argument `cmd` with its JSON type (`"$a.b"` reaches into objects; a missing
    /// source omits the argument); `"${cmd}"` inside a longer string interpolates the argument as
    /// text (missing becomes empty); `"$$"` is a literal `$`; every other value is a literal.
    /// Empty means the arguments pass through unchanged.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub args: Map<String, Value>,
    /// With `args`: also keep the trajectory arguments no template copied, under their own names.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_rest: bool,
}

impl ToolMap {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The name the client knows `tool` by: the mapping's `name`, or `tool` itself when unmapped.
    pub fn client_name<'a>(&'a self, tool: &'a str) -> &'a str {
        self.0.get(tool).map_or(tool, |m| m.name.as_str())
    }

    /// `call` with its tool renamed and its arguments remapped; unmapped tools come back as they
    /// were. The call id is untouched.
    pub fn apply(&self, call: &ToolCallSpec) -> ToolCallSpec {
        let Some(mapping) = self.0.get(&call.name) else {
            return call.clone();
        };
        let arguments = if mapping.args.is_empty() {
            call.arguments.clone()
        } else {
            match &call.arguments {
                Value::String(s) => match serde_json::from_str::<Value>(s) {
                    Ok(v @ Value::Object(_)) => mapping.map_arguments(&v),
                    _ => call.arguments.clone(),
                },
                Value::Null => mapping.map_arguments(&Value::Object(Map::new())),
                v => mapping.map_arguments(v),
            }
        };
        ToolCallSpec {
            name: mapping.name.clone(),
            arguments,
            id: call.id.clone(),
        }
    }

    /// Registration-time check: every mapping has a non-empty `name`.
    pub fn validate(&self) -> Result<(), String> {
        match self.0.iter().find(|(_, m)| m.name.is_empty()) {
            Some((tool, _)) => Err(format!("tool_map: {tool:?} maps to an empty name")),
            None => Ok(()),
        }
    }
}

impl ToolMapping {
    /// The client arguments for the trajectory arguments `source` (a JSON object; any other value
    /// is returned unchanged).
    pub fn map_arguments(&self, source: &Value) -> Value {
        if self.args.is_empty() || !source.is_object() {
            return source.clone();
        }
        let mut out = Map::new();
        let mut used = Vec::new();
        for (key, template) in &self.args {
            collect_roots(template, &mut used);
            if let Some(v) = expand_template(template, source) {
                out.insert(key.clone(), v);
            }
        }
        if self.keep_rest
            && let Some(obj) = source.as_object()
        {
            for (k, v) in obj {
                if !used.iter().any(|u| u == k) && !out.contains_key(k) {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
        Value::Object(out)
    }
}

/// Top-level argument names a template reads, for `keep_rest`.
fn collect_roots(template: &Value, out: &mut Vec<String>) {
    let Value::String(s) = template else { return };
    if let Some(path) = s.strip_prefix('$')
        && !path.is_empty()
        && path.chars().all(is_path_char)
    {
        out.push(path.split('.').next().unwrap_or_default().to_string());
        return;
    }
    for token in tokens(s) {
        if let Token::Path(p) = token {
            out.push(p.split('.').next().unwrap_or_default().to_string());
        }
    }
}

enum Token<'a> {
    Lit(String),
    Path(&'a str),
}

fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// Splits a template string into literal runs (`$$` unescaped) and `${path}` references.
fn tokens(s: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        lit.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        if let Some(r) = rest.strip_prefix('$') {
            lit.push('$');
            rest = r;
        } else if let Some(r) = rest.strip_prefix('{')
            && let Some(end) = r.find('}')
        {
            if !lit.is_empty() {
                out.push(Token::Lit(std::mem::take(&mut lit)));
            }
            out.push(Token::Path(&r[..end]));
            rest = &r[end + 1..];
        } else {
            lit.push('$');
        }
    }
    lit.push_str(rest);
    if !lit.is_empty() {
        out.push(Token::Lit(lit));
    }
    out
}

fn lookup<'a>(source: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(source, |v, seg| match v {
        Value::Object(o) => o.get(seg),
        Value::Array(a) => seg.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    })
}

/// Expands one template value against `source`; `None` means "omit this argument".
pub fn expand_template(template: &Value, source: &Value) -> Option<Value> {
    let Value::String(s) = template else {
        return Some(template.clone());
    };
    if let Some(path) = s.strip_prefix('$')
        && !path.is_empty()
        && path.chars().all(is_path_char)
    {
        return lookup(source, path).cloned();
    }
    if !s.contains('$') {
        return Some(template.clone());
    }
    let mut text = String::new();
    for token in tokens(s) {
        match token {
            Token::Lit(l) => text.push_str(&l),
            Token::Path(p) => match lookup(source, p) {
                Some(Value::String(v)) => text.push_str(v),
                Some(v) => text.push_str(&v.to_string()),
                None => {}
            },
        }
    }
    Some(Value::String(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> ToolMap {
        serde_json::from_value(v).unwrap()
    }

    fn call(name: &str, args: Value) -> ToolCallSpec {
        ToolCallSpec {
            name: name.into(),
            arguments: args,
            id: Some("c1".into()),
        }
    }

    #[test]
    fn renames_and_keeps_arguments_without_args() {
        let m = map(json!({"bash": {"name": "run_shell"}}));
        let out = m.apply(&call("bash", json!({"command": "ls"})));
        assert_eq!(
            (out.name.as_str(), out.arguments, out.id.as_deref()),
            ("run_shell", json!({"command": "ls"}), Some("c1"))
        );
        assert_eq!(m.apply(&call("read", json!({"p": 1}))), call("read", json!({"p": 1})));
    }

    #[test]
    fn templates_copy_type_and_interpolate() {
        let src = json!({"command": "ls", "n": 3, "opts": {"deep": true}});
        assert_eq!(expand_template(&json!("$n"), &src), Some(json!(3)));
        assert_eq!(expand_template(&json!("$opts.deep"), &src), Some(json!(true)));
        assert_eq!(expand_template(&json!("$missing"), &src), None);
        assert_eq!(
            expand_template(&json!("run ${command} x${n}${nope}"), &src),
            Some(json!("run ls x3"))
        );
        assert_eq!(expand_template(&json!("$$"), &src), Some(json!("$")));
        assert_eq!(expand_template(&json!("$$HOME"), &src), Some(json!("$HOME")));
        assert_eq!(expand_template(&json!(7), &src), Some(json!(7)));
        assert_eq!(expand_template(&json!("plain"), &src), Some(json!("plain")));
        assert_eq!(expand_template(&json!("cost $ 5"), &src), Some(json!("cost $ 5")));
    }

    #[test]
    fn rebuilds_arguments_in_order_and_keeps_rest() {
        let m = map(json!({"bash": {"name": "sh", "args": {"cmd": "$command", "cwd": "/tmp", "gone": "$nope"}}}));
        let out = m.apply(&call("bash", json!({"command": "ls", "timeout": 5})));
        assert_eq!(out.arguments, json!({"cmd": "ls", "cwd": "/tmp"}));
        assert_eq!(out.arguments.as_object().unwrap().keys().collect::<Vec<_>>(), ["cmd", "cwd"]);

        let m = map(json!({"bash": {"name": "sh", "args": {"cmd": "$command"}, "keep_rest": true}}));
        let out = m.apply(&call("bash", json!({"command": "ls", "timeout": 5})));
        assert_eq!(out.arguments, json!({"cmd": "ls", "timeout": 5}));
    }

    #[test]
    fn string_arguments_are_parsed_or_passed_verbatim() {
        let m = map(json!({"bash": {"name": "sh", "args": {"cmd": "$command"}}}));
        assert_eq!(
            m.apply(&call("bash", json!("{\"command\":\"ls\"}"))).arguments,
            json!({"cmd": "ls"})
        );
        let out = m.apply(&call("bash", json!("{broken")));
        assert_eq!((out.name.as_str(), out.arguments), ("sh", json!("{broken")));
    }

    #[test]
    fn validate_rejects_empty_names() {
        assert!(map(json!({"a": {"name": ""}})).validate().is_err());
        assert!(map(json!({"a": {"name": "b"}})).validate().is_ok());
    }
}
