//! What matching compares and what it first rewrites: the ignore list (request fields that do not take part) and
//! the regex normalizers (text that varies between runs: timestamps, ids). Both sides of a comparison, the recorded
//! request and the live one, go through the same functions.

use regex::Regex;
use serde_json::Value;

use crate::cassette::CassetteError;
use crate::cassette::format::{DEFAULT_IGNORE, Normalizer};

/// The normalizers applied before a cassette's own, unless `normalize_defaults` is false: ISO 8601 timestamps
/// to `<timestamp>`, bare ISO dates (`2026-01-01`, as clients put in system prompts) to `<date>`, UUIDs to `<uuid>`,
/// id-like `req_...` tokens (16 or more alphanumerics, as servers emit them) to `<req_id>`. In that order. All of
/// them are off with `normalize_defaults: false`.
pub fn default_normalizers() -> Vec<Normalizer> {
    let n = |pattern: &str, replace: &str| Normalizer {
        pattern: pattern.into(),
        replace: replace.into(),
    };
    vec![
        n(
            r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2})?",
            "<timestamp>",
        ),
        n(r"\b\d{4}-\d{2}-\d{2}(?:\b|T)", "<date>"),
        n(
            r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
            "<uuid>",
        ),
        n(r"\breq_[A-Za-z0-9]{16,}\b", "<req_id>"),
    ]
}

/// Compiled normalizers, applied in order.
#[derive(Clone, Debug, Default)]
pub struct Normalizers {
    rules: Vec<(Regex, String)>,
}

impl Normalizers {
    /// The defaults (when `defaults`) followed by `own`. A pattern that does not compile is an error naming it.
    pub fn compile(own: &[Normalizer], defaults: bool) -> Result<Self, CassetteError> {
        let defaults = if defaults { default_normalizers() } else { Vec::new() };
        let rules = defaults
            .iter()
            .chain(own)
            .map(|n| {
                Regex::new(&n.pattern)
                    .map(|re| (re, n.replace.clone()))
                    .map_err(|e| CassetteError(format!("bad normalize pattern {:?}: {e}", n.pattern)))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { rules })
    }

    /// `text` with every substitution applied in order.
    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (re, replace) in &self.rules {
            if let std::borrow::Cow::Owned(s) = re.replace_all(&out, replace.as_str()) {
                out = s;
            }
        }
        out
    }
}

/// Request fields an `ignore` entry may name besides `extra.<key>`.
const IGNORABLE: &[&str] = &[
    "temperature",
    "top_p",
    "seed",
    "user",
    "metadata",
    "stream",
    "include_usage",
    "store",
    "max_tokens",
    "n",
    "stop",
    "parallel_tool_calls",
    "reasoning_effort",
    "thinking",
    "response_format",
    "previous_response_id",
    "tool_choice",
    "system",
    "tools",
];

/// The request fields left out of a comparison: [`DEFAULT_IGNORE`] plus a cassette's and an exchange's entries,
/// minus the defaults they remove with `!field`.
#[derive(Clone, Debug, Default)]
pub struct IgnoreSet {
    fields: Vec<String>,
}

fn known_field(name: &str) -> bool {
    IGNORABLE.contains(&name) || name.strip_prefix("extra.").is_some_and(|k| !k.is_empty())
}

impl IgnoreSet {
    /// Builds the set from the lists in order (cassette, then exchange). Unknown field names, and fields that
    /// cannot be ignored (`model`, `messages`), are errors; the accepted names are listed in docs/m4-spec.md section 3.2.
    pub fn compile(lists: &[&[String]]) -> Result<Self, CassetteError> {
        let mut set = Self {
            fields: DEFAULT_IGNORE.iter().map(|s| s.to_string()).collect(),
        };
        for list in lists {
            set.extend(list)?;
        }
        Ok(set)
    }

    /// A copy with `list` applied on top.
    pub fn with(&self, list: &[String]) -> Result<Self, CassetteError> {
        let mut set = self.clone();
        set.extend(list)?;
        Ok(set)
    }

    fn extend(&mut self, list: &[String]) -> Result<(), CassetteError> {
        for entry in list {
            if let Some(field) = entry.strip_prefix('!') {
                if !known_field(field) {
                    return Err(CassetteError(format!("unknown ignore entry {entry:?}")));
                }
                self.fields.retain(|f| f.split('=').next() != Some(field));
                continue;
            }
            let name = entry.split('=').next().unwrap_or_default();
            if name.is_empty() || (!known_field(name) && name != "extra.*") {
                return Err(CassetteError(format!(
                    "unknown ignore entry {entry:?} (model and messages cannot be ignored)"
                )));
            }
            if !self.fields.contains(entry) {
                self.fields.push(entry.clone());
            }
        }
        Ok(())
    }

    /// Whether `field` (`temperature`, `extra.foo`, ...) is left out. `value` is only looked at for entries of the
    /// form `field=value`, e.g. `tool_choice=auto`.
    pub fn ignores(&self, field: &str, value: Option<&str>) -> bool {
        self.fields.iter().any(|entry| match entry.split_once('=') {
            Some((f, v)) => f == field && value == Some(v),
            None => entry == field || (entry == "extra.*" && field.starts_with("extra.")),
        })
    }
}

/// `value` with object keys sorted at every level, serialized compactly: equal documents give equal strings.
pub fn canonical(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
        other => out.push_str(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_normalizers_rewrite_volatile_text() {
        let n = Normalizers::compile(&[], true).unwrap();
        assert_eq!(
            n.apply("at 2026-11-01T10:00:00Z id 123e4567-e89b-12d3-a456-426614174000 req_AbC123 req_01ABCdef23456789zz done"),
            "at <timestamp> id <uuid> req_AbC123 <req_id> done"
        );
        assert_eq!(n.apply("Today is 2025-03-14."), "Today is <date>.");
        assert_eq!(n.apply("explain req_body_size"), "explain req_body_size");
    }

    #[test]
    fn own_normalizers_follow_defaults_and_defaults_can_be_off() {
        let own = [Normalizer {
            pattern: r"(\w+)@example\.com".into(),
            replace: "<$1>".into(),
        }];
        let n = Normalizers::compile(&own, true).unwrap();
        assert_eq!(n.apply("bob@example.com 2026-11-01 10:00:00"), "<bob> <timestamp>");
        let n = Normalizers::compile(&own, false).unwrap();
        assert_eq!(n.apply("bob@example.com 2026-11-01 10:00:00"), "<bob> 2026-11-01 10:00:00");
        let bad = [Normalizer {
            pattern: "(".into(),
            replace: String::new(),
        }];
        assert!(Normalizers::compile(&bad, true).unwrap_err().0.contains("\"(\""));
    }

    #[test]
    fn ignore_defaults_additions_and_removals() {
        let set = IgnoreSet::compile(&[]).unwrap();
        assert!(set.ignores("temperature", None) && set.ignores("extra.foo", None));
        assert!(set.ignores("tool_choice", Some("auto")) && !set.ignores("tool_choice", Some("required")));
        assert!(!set.ignores("stop", None) && !set.ignores("system", None));
        let c = list(&["system", "!max_tokens", "extra.k"]);
        let e = list(&["tools", "tool_choice=required"]);
        let set = IgnoreSet::compile(&[&c, &e]).unwrap();
        assert!(set.ignores("system", None) && set.ignores("tools", None));
        assert!(!set.ignores("max_tokens", None) && set.ignores("tool_choice", Some("required")));
        let set = IgnoreSet::compile(&[&list(&["!tool_choice"])]).unwrap();
        assert!(!set.ignores("tool_choice", Some("auto")));
    }

    #[test]
    fn ignore_rejects_unknown_names() {
        for bad in ["model", "messages", "bogus", "extra.", "!model", "=x"] {
            let l = list(&[bad]);
            assert!(IgnoreSet::compile(&[&l]).is_err(), "{bad}");
        }
    }

    #[test]
    fn canonical_sorts_keys_at_every_level() {
        let a = json!({"b": [{"y": 1, "x": "q\"z"}], "a": null});
        let b = json!({"a": null, "b": [{"x": "q\"z", "y": 1}]});
        assert_eq!(canonical(&a), canonical(&b));
        assert_eq!(canonical(&a), r#"{"a":null,"b":[{"x":"q\"z","y":1}]}"#);
    }
}

#[cfg(test)]
mod default_time_tests {
    use super::*;

    #[test]
    fn times_without_seconds_and_dates_glued_to_t_are_normalized() {
        let n = Normalizers::compile(&[], true).unwrap();
        assert_eq!(n.apply("hi 2026-11-01 10:00"), n.apply("hi 2026-11-02 11:30"));
        assert_eq!(n.apply("hi 2026-11-01T10:00Z"), n.apply("hi 2026-12-02T11:30:05+01:00"));
    }
}
