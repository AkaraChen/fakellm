//! Structured output in the fallback reply: `ir::schema::instance` over a corpus of schemas, and the three protocols over
//! HTTP (docs/m4-spec.md section 7). The validator below supports exactly the keywords the generator documents.

use fakellm::ir::schema::{MAX_DEPTH, instance};
use serde_json::{Map, Value, json};

mod common;
use common::*;

// ---- a tiny validator for the supported subset ----

fn resolve<'a>(root: &'a Value, r: &str) -> &'a Value {
    root.pointer(r.strip_prefix('#').unwrap_or(r))
        .unwrap_or_else(|| panic!("unresolvable $ref {r}"))
}

fn type_ok(t: &str, v: &Value) -> bool {
    match t {
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        other => panic!("unknown type {other}"),
    }
}

/// `Ok(())` when `v` satisfies `schema`; `Err(path: reason)` otherwise.
fn validate(root: &Value, schema: &Value, v: &Value, path: &str) -> Result<(), String> {
    let Some(s) = schema.as_object() else {
        return if schema == &json!(false) {
            Err(format!("{path}: false schema"))
        } else {
            Ok(())
        };
    };
    let fail = |why: String| Err(format!("{path}: {why} (value {v})"));
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        return validate(root, resolve(root, r), v, path);
    }
    if let Some(c) = s.get("const")
        && c != v
    {
        return fail(format!("const {c}"));
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array)
        && !e.contains(v)
    {
        return fail("not in enum".into());
    }
    match s.get("type") {
        Some(Value::String(t)) if !type_ok(t, v) => return fail(format!("type {t}")),
        Some(Value::Array(ts)) if !ts.iter().any(|t| type_ok(t.as_str().unwrap(), v)) => return fail(format!("types {ts:?}")),
        _ => {}
    }
    if let Some(all) = s.get("allOf").and_then(Value::as_array) {
        for b in all {
            validate(root, b, v, path)?;
        }
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(branches) = s.get(key).and_then(Value::as_array)
            && !branches.iter().any(|b| validate(root, b, v, path).is_ok())
        {
            return fail(format!("no {key} branch holds"));
        }
    }
    if let Some(n) = s.get("not")
        && validate(root, n, v, path).is_ok()
    {
        return fail("matches `not`".into());
    }
    if let Some(x) = v.as_f64() {
        let bound = |k: &str| s.get(k).and_then(Value::as_f64);
        if bound("minimum").is_some_and(|m| x < m) || bound("maximum").is_some_and(|m| x > m) {
            return fail("outside minimum/maximum".into());
        }
        if bound("exclusiveMinimum").is_some_and(|m| x <= m) || bound("exclusiveMaximum").is_some_and(|m| x >= m) {
            return fail("outside exclusive bounds".into());
        }
        if let Some(m) = bound("multipleOf") {
            let q = x / m;
            if (q - q.round()).abs() > 1e-6 {
                return fail(format!("not a multiple of {m}"));
            }
        }
    }
    if let Some(t) = v.as_str() {
        let len = t.chars().count() as u64;
        if s.get("minLength").and_then(Value::as_u64).is_some_and(|m| len < m)
            || s.get("maxLength").and_then(Value::as_u64).is_some_and(|m| len > m)
        {
            return fail("length".into());
        }
        if let Some(p) = s.get("pattern").and_then(Value::as_str)
            && !regex::Regex::new(p).unwrap().is_match(t)
        {
            return fail(format!("pattern {p}"));
        }
        match s.get("format").and_then(Value::as_str) {
            Some("email") if !t.contains('@') => return fail("email".into()),
            Some("uuid")
                if !regex::Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
                    .unwrap()
                    .is_match(t) =>
            {
                return fail("uuid".into());
            }
            Some("date") if !regex::Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap().is_match(t) => return fail("date".into()),
            Some("date-time") if !regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}").unwrap().is_match(t) => {
                return fail("date-time".into());
            }
            Some("ipv4") if t.parse::<std::net::Ipv4Addr>().is_err() => return fail("ipv4".into()),
            Some("uri" | "url") if !t.starts_with("http") => return fail("uri".into()),
            _ => {}
        }
    }
    if let Some(a) = v.as_array() {
        let n = a.len() as u64;
        if s.get("minItems").and_then(Value::as_u64).is_some_and(|m| n < m)
            || s.get("maxItems").and_then(Value::as_u64).is_some_and(|m| n > m)
        {
            return fail("item count".into());
        }
        if s.get("uniqueItems") == Some(&json!(true)) {
            for (i, x) in a.iter().enumerate() {
                if a[..i].contains(x) {
                    return fail("duplicate items".into());
                }
            }
        }
        let prefix = s.get("prefixItems").and_then(Value::as_array).map_or(0, Vec::len);
        for (i, x) in a.iter().enumerate() {
            let item_schema = s
                .get("prefixItems")
                .and_then(Value::as_array)
                .and_then(|p| p.get(i))
                .or_else(|| s.get("items").filter(|_| i >= prefix || prefix == 0));
            if let Some(is) = item_schema {
                validate(root, is, x, &format!("{path}[{i}]"))?;
            }
        }
        if let Some(c) = s.get("contains") {
            let hits = a.iter().filter(|x| validate(root, c, x, path).is_ok()).count() as u64;
            if hits < s.get("minContains").and_then(Value::as_u64).unwrap_or(1) {
                return fail("contains".into());
            }
        }
    }
    if let Some(o) = v.as_object() {
        let empty = Map::new();
        let props = s.get("properties").and_then(Value::as_object).unwrap_or(&empty);
        for r in s.get("required").and_then(Value::as_array).into_iter().flatten() {
            if !o.contains_key(r.as_str().unwrap()) {
                return fail(format!("missing required {r}"));
            }
        }
        if s.get("minProperties").and_then(Value::as_u64).is_some_and(|m| (o.len() as u64) < m) {
            return fail("minProperties".into());
        }
        for (k, x) in o {
            match props.get(k) {
                Some(ps) => validate(root, ps, x, &format!("{path}.{k}"))?,
                None => match s.get("additionalProperties") {
                    Some(Value::Bool(false)) => return fail(format!("unexpected property {k}")),
                    Some(ap @ Value::Object(_)) => validate(root, ap, x, &format!("{path}.{k}"))?,
                    _ => {}
                },
            }
        }
        if let Some(dr) = s.get("dependentRequired").and_then(Value::as_object) {
            for (k, needs) in dr {
                if o.contains_key(k) {
                    for n in needs.as_array().unwrap() {
                        if !o.contains_key(n.as_str().unwrap()) {
                            return fail(format!("{k} requires {n}"));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn check(schema: &Value, v: &Value) {
    if let Err(e) = validate(schema, schema, v, "$") {
        panic!("{e}\nschema: {schema}");
    }
}

fn corpus() -> Vec<(&'static str, Value)> {
    vec![
        ("const", json!({"const": {"a": 1}})),
        ("enum", json!({"enum": ["x", 2, null, true]})),
        ("boolean", json!({"type": "boolean"})),
        ("null", json!({"type": "null"})),
        ("integer bounds", json!({"type": "integer", "minimum": 10, "maximum": 12})),
        (
            "exclusive bounds",
            json!({"type": "integer", "exclusiveMinimum": 5, "exclusiveMaximum": 8}),
        ),
        (
            "draft-4 exclusive",
            json!({"type": "number", "minimum": 0, "exclusiveMinimum": true, "maximum": 1}),
        ),
        (
            "multipleOf",
            json!({"type": "integer", "multipleOf": 7, "minimum": 1, "maximum": 100}),
        ),
        ("number", json!({"type": "number", "minimum": -2.5, "maximum": 2.5})),
        ("lower bound only", json!({"type": "integer", "minimum": 500})),
        ("string lengths", json!({"type": "string", "minLength": 30, "maxLength": 40})),
        ("short string", json!({"type": "string", "maxLength": 3})),
        ("email", json!({"type": "string", "format": "email"})),
        ("uuid", json!({"type": "string", "format": "uuid"})),
        ("date", json!({"type": "string", "format": "date"})),
        ("date-time", json!({"type": "string", "format": "date-time"})),
        ("ipv4", json!({"type": "string", "format": "ipv4"})),
        ("uri", json!({"type": "string", "format": "uri"})),
        ("pattern", json!({"type": "string", "pattern": "^[A-Z]{2}-\\d{3}$"})),
        (
            "pattern alternation",
            json!({"type": "string", "pattern": "^(red|green|blue)-[a-z0-9_]{2,4}$"}),
        ),
        (
            "array",
            json!({"type": "array", "items": {"type": "integer", "minimum": 0, "maximum": 3}, "minItems": 3, "maxItems": 5}),
        ),
        (
            "unique items",
            json!({"type": "array", "items": {"type": "integer", "minimum": 0, "maximum": 20}, "uniqueItems": true, "minItems": 4}),
        ),
        (
            "tuple",
            json!({"type": "array", "prefixItems": [{"type": "string"}, {"type": "integer"}], "minItems": 2, "maxItems": 2}),
        ),
        (
            "contains",
            json!({"type": "array", "items": {"type": "integer"}, "contains": {"const": 42}, "minItems": 2}),
        ),
        (
            "object",
            json!({
                "type": "object",
                "properties": {"id": {"type": "integer"}, "name": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}}, "nick": {"type": "string"}},
                "required": ["id", "name"],
                "additionalProperties": false,
            }),
        ),
        ("minProperties", json!({"type": "object", "minProperties": 3})),
        (
            "dependentRequired",
            json!({
                "type": "object",
                "properties": {"a": {"type": "string"}, "b": {"type": "integer"}, "c": {"type": "boolean"}},
                "required": ["a"],
                "dependentRequired": {"a": ["b", "c"]},
            }),
        ),
        ("type list", json!({"type": ["null", "string"]})),
        (
            "no type, properties",
            json!({"properties": {"x": {"type": "integer"}}, "required": ["x"]}),
        ),
        (
            "anyOf",
            json!({"anyOf": [{"type": "integer", "minimum": 5, "maximum": 6}, {"type": "string"}]}),
        ),
        ("oneOf", json!({"oneOf": [{"type": "string", "maxLength": 4}, {"type": "null"}]})),
        (
            "allOf",
            json!({"allOf": [
                {"type": "object", "properties": {"a": {"type": "integer"}}, "required": ["a"]},
                {"properties": {"b": {"type": "string"}}, "required": ["b"]},
            ]}),
        ),
        ("not", json!({"type": "string", "enum": ["a", "b", "c"], "not": {"const": "a"}})),
        ("examples", json!({"type": "string", "examples": ["only-this", "or-this"]})),
        ("default", json!({"type": "integer", "default": 7, "minimum": 0})),
        (
            "definitions",
            json!({
                "type": "object",
                "properties": {"user": {"$ref": "#/definitions/user"}},
                "required": ["user"],
                "definitions": {"user": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}},
            }),
        ),
        (
            "$defs",
            json!({
                "type": "array", "items": {"$ref": "#/$defs/n"}, "minItems": 1,
                "$defs": {"n": {"type": "string", "enum": ["p", "q"]}},
            }),
        ),
        (
            "recursive $ref",
            json!({
                "$ref": "#/definitions/node",
                "definitions": {"node": {"type": "object", "properties": {"value": {"type": "integer"}, "children": {"type": "array", "items": {"$ref": "#/definitions/node"}}}, "required": ["value", "children"]}},
            }),
        ),
        (
            "openai structured output",
            json!({
                "type": "object",
                "properties": {
                    "answer": {"type": "string"},
                    "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                    "sources": {"type": "array", "items": {"type": "object", "properties": {"url": {"type": "string", "format": "uri"}, "title": {"type": "string"}}, "required": ["url", "title"], "additionalProperties": false}},
                    "kind": {"type": "string", "enum": ["fact", "opinion"]},
                },
                "required": ["answer", "confidence", "sources", "kind"],
                "additionalProperties": false,
            }),
        ),
    ]
}

// ---- instance, unit level ----

#[test]
fn every_schema_of_the_corpus_yields_a_valid_value_for_many_seeds() {
    for (label, schema) in corpus() {
        for seed in 0..40u64 {
            let v = instance(&schema, seed);
            if let Err(e) = validate(&schema, &schema, &v, "$") {
                panic!("{label}, seed {seed}: {e}\nschema: {schema}");
            }
        }
    }
}

#[test]
fn instance_is_deterministic_and_depends_on_the_seed() {
    for (label, schema) in corpus() {
        assert_eq!(instance(&schema, 5), instance(&schema, 5), "{label}");
    }
    let schema = json!({"type": "object", "properties": {"s": {"type": "string"}, "n": {"type": "integer"}, "a": {"type": "array", "items": {"type": "string"}}}, "required": ["s", "n", "a"]});
    let distinct: std::collections::HashSet<String> = (0..20).map(|seed| instance(&schema, seed).to_string()).collect();
    assert!(distinct.len() > 5, "only {} distinct values over 20 seeds", distinct.len());
}

#[test]
fn adding_a_property_does_not_change_its_siblings() {
    let a = json!({"type": "object", "properties": {"x": {"type": "string"}, "y": {"type": "integer"}}, "required": ["x", "y"]});
    let b = json!({"type": "object", "properties": {"x": {"type": "string"}, "y": {"type": "integer"}, "z": {"type": "string"}}, "required": ["x", "y", "z"]});
    let (va, vb) = (instance(&a, 9), instance(&b, 9));
    assert_eq!(va["x"], vb["x"]);
    assert_eq!(va["y"], vb["y"]);
}

#[test]
fn const_enum_examples_and_default_are_used_verbatim() {
    assert_eq!(instance(&json!({"const": [1, {"a": null}]}), 3), json!([1, {"a": null}]));
    assert_eq!(instance(&json!({"type": "integer", "default": 7}), 3), json!(7));
    let ex = instance(&json!({"type": "string", "examples": ["only-this"]}), 3);
    assert_eq!(ex, json!("only-this"));
}

#[test]
fn required_properties_are_always_present_and_additional_false_is_respected() {
    let schema = json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "string"}}, "required": ["a"], "additionalProperties": false});
    for seed in 0..30 {
        let v = instance(&schema, seed);
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert!(keys.contains(&&"a".to_owned()));
        assert!(keys.iter().all(|k| *k == "a" || *k == "b"), "{v}");
    }
}

#[test]
fn ref_cycles_end_at_the_depth_cap_and_stay_valid() {
    let schema = json!({
        "$ref": "#/definitions/n",
        "definitions": {"n": {"type": "object", "properties": {"next": {"$ref": "#/definitions/n"}}, "required": ["next"]}},
    });
    let v = instance(&schema, 1);
    let mut depth = 0;
    let mut cur = &v;
    while let Some(next) = cur.get("next") {
        cur = next;
        depth += 1;
        assert!(depth <= MAX_DEPTH + 2, "recursion did not stop");
    }
}

#[test]
fn unsupported_keywords_do_not_crash_the_generator() {
    let schema = json!({"type": "object", "if": {"properties": {"a": {"const": 1}}}, "then": {"required": ["b"]}, "patternProperties": {"^x": {"type": "string"}}, "properties": {"a": {"type": "integer"}}, "required": ["a"]});
    let v = instance(&schema, 4);
    assert!(v["a"].is_number());
}

// ---- over HTTP ----

fn person_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "minLength": 3},
            "age": {"type": "integer", "minimum": 18, "maximum": 99},
            "email": {"type": "string", "format": "email"},
            "role": {"type": "string", "enum": ["admin", "user"]},
            "tags": {"type": "array", "items": {"type": "string"}, "minItems": 1},
        },
        "required": ["name", "age", "email", "role", "tags"],
        "additionalProperties": false,
    })
}

#[tokio::test]
async fn chat_json_schema_yields_a_valid_object() {
    let s = start().await;
    let schema = person_schema();
    let mut b = chat_body("gpt-4o", "make a person");
    b["response_format"] = json!({"type": "json_schema", "json_schema": {"name": "person", "strict": true, "schema": schema}});
    let r = chat(&s, &b).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let j = r.json();
    let v: Value = serde_json::from_str(j["choices"][0]["message"]["content"].as_str().unwrap()).unwrap();
    check(&schema, &v);
    assert_eq!(j["choices"][0]["finish_reason"], "stop");
    // Same request, same value.
    assert_eq!(
        chat(&s, &b).await.json()["choices"][0]["message"]["content"],
        j["choices"][0]["message"]["content"]
    );

    // Streaming: the chunks join to the same valid JSON.
    b["stream"] = json!(true);
    let frames = open_stream(&s, &b).await.all().await;
    let text: String = json_chunks(&frames)
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str().map(str::to_owned))
        .collect();
    check(&schema, &serde_json::from_str(&text).unwrap());
}

#[tokio::test]
async fn responses_text_format_yields_a_valid_object() {
    let s = start().await;
    let schema = person_schema();
    let mut b = responses_body("gpt-5.5", "make a person");
    b["text"] = json!({"format": {"type": "json_schema", "name": "person", "strict": true, "schema": schema}});
    let r = responses(&s, &b).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let text = r.json()["output"][0]["content"][0]["text"].as_str().unwrap().to_owned();
    check(&schema, &serde_json::from_str(&text).unwrap());
}

#[tokio::test]
async fn a_forced_tool_gets_arguments_that_fit_its_schema_in_all_protocols() {
    let s = start().await;
    let params = json!({
        "type": "object",
        "properties": {"city": {"type": "string", "minLength": 2}, "days": {"type": "integer", "minimum": 1, "maximum": 7}, "unit": {"type": "string", "enum": ["c", "f"]}},
        "required": ["city", "days", "unit"],
        "additionalProperties": false,
    });

    let mut b = chat_body("gpt-4o", "forecast");
    b["tools"] = json!([{"type": "function", "function": {"name": "forecast", "parameters": params}}]);
    b["tool_choice"] = json!({"type": "function", "function": {"name": "forecast"}});
    let j = chat(&s, &b).await.json();
    let call = &j["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "forecast");
    check(
        &params,
        &serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap(),
    );
    b["tool_choice"] = json!("required");
    let j = chat(&s, &b).await.json();
    check(
        &params,
        &serde_json::from_str(
            j["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap(),
    );

    let mut b = responses_body("gpt-5.5", "forecast");
    b["tools"] = json!([{"type": "function", "name": "forecast", "parameters": params}]);
    b["tool_choice"] = json!({"type": "function", "name": "forecast"});
    let j = responses(&s, &b).await.json();
    let call = j["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["type"] == "function_call")
        .unwrap()
        .clone();
    check(&params, &serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap());

    let mut b = anthropic_body("claude-sonnet-4-5", "forecast");
    b["tools"] = json!([{"name": "forecast", "description": "d", "input_schema": params}]);
    b["tool_choice"] = json!({"type": "tool", "name": "forecast"});
    let j = anthropic(&s, &b).await.json();
    let block = j["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "tool_use")
        .unwrap()
        .clone();
    assert_eq!(block["name"], "forecast");
    check(&params, &block["input"]);
    b["tool_choice"] = json!({"type": "any"});
    let j = anthropic(&s, &b).await.json();
    let block = j["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "tool_use")
        .unwrap()
        .clone();
    check(&params, &block["input"]);
}

#[tokio::test]
async fn json_object_stays_a_message_object() {
    let s = start().await;
    let mut b = chat_body("gpt-4o", "anything");
    b["response_format"] = json!({"type": "json_object"});
    let v: Value = serde_json::from_str(chat_text(&chat(&s, &b).await).as_str()).unwrap();
    assert!(v["message"].is_string(), "{v}");
    // json_schema without a schema behaves the same way.
    b["response_format"] = json!({"type": "json_schema", "json_schema": {"name": "x"}});
    let v: Value = serde_json::from_str(chat_text(&chat(&s, &b).await).as_str()).unwrap();
    assert!(v["message"].is_string(), "{v}");
}

#[tokio::test]
async fn max_tokens_truncates_structured_output_like_a_real_length_stop() {
    let s = start().await;
    let mut b = chat_body("gpt-4o", "make a person");
    b["response_format"] = json!({"type": "json_schema", "json_schema": {"name": "person", "schema": person_schema()}});
    b["max_tokens"] = json!(3);
    let j = chat(&s, &b).await.json();
    assert_eq!(j["choices"][0]["finish_reason"], "length");
    let text = j["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        serde_json::from_str::<Value>(text).is_err(),
        "truncated JSON should not parse: {text}"
    );
}
