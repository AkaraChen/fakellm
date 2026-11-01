//! A deterministic instance of a JSON Schema, for the default reply when a client asks for structured
//! output (`response_format: json_schema`, Responses `text.format`, or a tool the client forces with
//! `tool_choice`). The same schema and seed always give the same value, different seeds give different ones,
//! and the value satisfies every keyword listed in docs/m4-spec.md section 7 (keywords outside that list are
//! ignored, so a value may violate them: `if/then/else`, `patternProperties`, `propertyNames`, regexes outside the
//! supported `pattern` subset, ...).
//!
//! Only the fallback uses this; trajectories and scripts say what they want to say.

use serde_json::{Map, Number, Value};

use crate::fallback::{WORDS, hash64};

/// Nesting beyond this depth (`$ref` cycles included) produces the smallest valid value: only `required`
/// properties, `minItems` items.
pub const MAX_DEPTH: usize = 8;

/// Beyond this a required cycle that cannot be made smaller ends in `null` instead of a stack overflow.
const HARD_DEPTH: usize = MAX_DEPTH + 8;

/// Attempts at other child seeds for `uniqueItems`.
const TRIES: u64 = 8;

/// Attempts for `not`: cheap, and a narrow range needs many to dodge a forbidden value.
const NOT_TRIES: u64 = 32;

/// A value that satisfies `schema`, derived from `seed` alone (no clock, no randomness).
///
/// Rules (section 7 of docs/m4-spec.md is the contract): `const`, then `enum` (picked by seed), then `examples` and
/// `default` when declared; `$ref`
/// to `#/definitions/..` or `#/$defs/..` (any JSON pointer into the root); `oneOf`/`anyOf` take the first
/// branch, `allOf` merges `properties` and `required` of its branches; `type` may be a list (the first
/// non-`null` entry wins); booleans/null; integers and numbers honor `minimum`, `maximum`, the exclusive forms
/// (numeric or draft-4 boolean) and `multipleOf`; strings honor `minLength`, `maxLength` and `format`
/// (`date-time`, `date`, `time`, `email`, `uri`/`url`, `uuid`, `hostname`, `ipv4`) and a `pattern` from a small regex
/// subset; arrays have
/// `minItems..=max(minItems, 2)` items (capped by `maxItems`, distinct for `uniqueItems`); objects include every
/// `required` property and each optional one with a 50% chance decided by `seed` and the property name, never
/// a property the schema does not declare unless `minProperties` needs more and `additionalProperties` allows it
/// (`dependentRequired` pulls in the properties a present one requires); arrays honor `contains`; a simple `not`
/// (`const`, `enum`, `type`) is avoided by retrying other child seeds.
pub fn instance(schema: &Value, seed: u64) -> Value {
    Gen { root: schema }.gen_value(schema, seed, 0)
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The seed of a child: derived from the parent and the child's name, so siblings do not shift when one is added.
fn child(seed: u64, key: &str) -> u64 {
    let mut s = seed ^ hash64(key.as_bytes());
    splitmix(&mut s)
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        splitmix(&mut self.0)
    }

    /// Uniform-ish in `0..n` (`n > 0`).
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// In `lo..=hi`.
    fn between(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi.saturating_sub(lo) + 1)
    }
}

fn word(rng: &mut Rng) -> &'static str {
    WORDS[rng.below(WORDS.len() as u64) as usize]
}

struct Gen<'a> {
    root: &'a Value,
}

impl Gen<'_> {
    fn gen_value(&self, schema: &Value, seed: u64, depth: usize) -> Value {
        let not = schema.get("not").filter(|n| n.is_object());
        let Some(not) = not else {
            return self.core(schema, seed, depth);
        };
        let mut last = Value::Null;
        for attempt in 0..NOT_TRIES {
            let s = if attempt == 0 {
                seed
            } else {
                child(seed, &format!("not{attempt}"))
            };
            last = self.core(schema, s, depth);
            if !violates(&last, not) {
                return last;
            }
        }
        last
    }

    fn resolve<'s>(&'s self, mut schema: &'s Value) -> &'s Value {
        for _ in 0..16 {
            let Some(r) = schema.get("$ref").and_then(Value::as_str) else {
                break;
            };
            let Some(pointer) = r.strip_prefix('#') else {
                return &Value::Bool(true);
            };
            match self.root.pointer(pointer) {
                Some(target) => schema = target,
                None => return &Value::Bool(true),
            }
        }
        schema
    }

    fn core(&self, schema: &Value, seed: u64, depth: usize) -> Value {
        let Some(m) = schema.as_object() else {
            return if schema == &Value::Bool(false) {
                Value::Null
            } else {
                lorem_word(seed)
            };
        };
        if depth >= HARD_DEPTH {
            return Value::Null;
        }
        // Merge the branches first, so a `const` or `enum` on either side is intersected, not just the outer one.
        if let Some(merged) = self.combine(m) {
            return self.core(&Value::Object(merged), seed, depth);
        }
        if let Some(v) = m.get("const") {
            return v.clone();
        }
        let mut rng = Rng::new(seed);
        if let Some(Value::Array(a)) = m.get("enum")
            && !a.is_empty()
        {
            return a[rng.below(a.len() as u64) as usize].clone();
        }
        if let Some(Value::Array(a)) = m.get("examples")
            && !a.is_empty()
        {
            return a[rng.below(a.len() as u64) as usize].clone();
        }
        if let Some(v) = m.get("default") {
            return v.clone();
        }
        if m.contains_key("$ref") {
            return self.gen_value(self.resolve(schema), seed, depth + 1);
        }
        self.typed(m, seed, depth)
    }

    /// `allOf` merged and the first `oneOf`/`anyOf` branch merged into the schema's own keywords; `None` when the schema
    /// has none of the three.
    fn combine(&self, m: &Map<String, Value>) -> Option<Map<String, Value>> {
        if !["allOf", "oneOf", "anyOf"].iter().any(|k| m.contains_key(*k)) {
            return None;
        }
        let mut base = m.clone();
        let mut branches: Vec<Value> = Vec::new();
        if let Some(Value::Array(a)) = base.remove("allOf") {
            branches.extend(a.iter().map(|b| self.resolve(b).clone()));
        }
        for key in ["oneOf", "anyOf"] {
            if let Some(Value::Array(a)) = base.remove(key)
                && let Some(first) = a.first()
            {
                branches.push(self.resolve(first).clone());
            }
        }
        let mut out = base;
        for b in &branches {
            if let Some(bm) = b.as_object() {
                out = merge(out, bm);
            }
        }
        Some(out)
    }

    fn typed(&self, m: &Map<String, Value>, seed: u64, depth: usize) -> Value {
        match schema_type(m) {
            Some("object") => self.object(m, seed, depth),
            Some("array") => self.array(m, seed, depth),
            Some("string") => string(m, seed),
            Some("integer") => integer(m, seed),
            Some("number") => number(m, seed),
            Some("boolean") => Value::Bool(Rng::new(seed).below(2) == 1),
            Some("null") => Value::Null,
            _ => lorem_word(seed),
        }
    }

    fn object(&self, m: &Map<String, Value>, seed: u64, depth: usize) -> Value {
        let minimal = depth >= MAX_DEPTH;
        let empty = Map::new();
        let props = m.get("properties").and_then(Value::as_object).unwrap_or(&empty);
        let required: Vec<&str> = m
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let additional = m.get("additionalProperties");
        let gen_prop = |name: &str, depth: usize| -> Value {
            match props.get(name) {
                Some(s) => self.gen_value(s, child(seed, name), depth + 1),
                None => match additional {
                    Some(a) if a.is_object() => self.gen_value(a, child(seed, name), depth + 1),
                    _ => lorem_word(child(seed, name)),
                },
            }
        };
        let mut included: Vec<String> = Vec::new();
        for name in props.keys() {
            let want = required.contains(&name.as_str()) || (!minimal && child(seed, name) & 1 == 0);
            if want {
                included.push(name.clone());
            }
        }
        for r in &required {
            if !included.iter().any(|i| i == r) {
                included.push((*r).to_string());
            }
        }
        // dependentRequired (and the draft-7 array form of `dependencies`) to a fixpoint.
        let deps: Vec<(&String, &Value)> = ["dependentRequired", "dependencies"]
            .iter()
            .filter_map(|k| m.get(*k).and_then(Value::as_object))
            .flatten()
            .collect();
        loop {
            let mut added = false;
            for (name, needs) in &deps {
                if !included.contains(name) {
                    continue;
                }
                for need in needs.as_array().into_iter().flatten().filter_map(Value::as_str) {
                    if !included.iter().any(|i| i == need) {
                        included.push(need.to_string());
                        added = true;
                    }
                }
            }
            if !added {
                break;
            }
        }
        let mut out = Map::new();
        for name in &included {
            out.insert(name.clone(), gen_prop(name, depth));
        }
        let min = m.get("minProperties").and_then(Value::as_u64).unwrap_or(0) as usize;
        if additional != Some(&Value::Bool(false)) {
            let mut n = 1;
            while out.len() < min {
                let name = format!("extra_{n}");
                n += 1;
                if !out.contains_key(&name) {
                    let v = gen_prop(&name, depth);
                    out.insert(name, v);
                }
            }
        }
        Value::Object(out)
    }

    fn array(&self, m: &Map<String, Value>, seed: u64, depth: usize) -> Value {
        let minimal = depth >= MAX_DEPTH;
        let min = m.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
        let max = m.get("maxItems").and_then(Value::as_u64).map(|v| v as usize);
        let prefix: Vec<&Value> = match (m.get("prefixItems"), m.get("items")) {
            (Some(Value::Array(a)), _) | (None, Some(Value::Array(a))) => a.iter().collect(),
            _ => Vec::new(),
        };
        let items = m.get("items").filter(|i| i.is_object() || i.is_boolean());
        let contains = m.get("contains").filter(|c| c.is_object());
        let min_contains = m.get("minContains").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
        let mut rng = Rng::new(seed);
        let mut count = if minimal {
            min
        } else {
            rng.between(min.max(1) as u64, min.max(2) as u64) as usize
        };
        if !minimal {
            count = count.max(prefix.len());
            if contains.is_some() {
                count = count.max(min_contains);
            }
        }
        if let Some(max) = max {
            count = count.min(max);
        }
        let mut out: Vec<Value> = Vec::with_capacity(count);
        let unique = m.get("uniqueItems") == Some(&Value::Bool(true));
        for i in 0..count {
            let schema = if let Some(p) = prefix.get(i) {
                Some(*p)
            } else if let (Some(c), true) = (contains, i < min_contains) {
                Some(c)
            } else {
                items
            };
            let gen_at = |tag: &str| match schema {
                Some(s) => self.gen_value(s, child(seed, &format!("{i}{tag}")), depth + 1),
                None => lorem_word(child(seed, &format!("{i}{tag}"))),
            };
            let mut v = gen_at("");
            if unique {
                for t in 1..TRIES {
                    if !out.contains(&v) {
                        break;
                    }
                    v = gen_at(&format!("u{t}"));
                }
            }
            out.push(v);
        }
        Value::Array(out)
    }
}

/// `b` merged into `a`: `properties` unioned (a property in both becomes an `allOf` of the two), `required`
/// unioned, other keywords from `a` unless only `b` sets them.
fn merge(mut a: Map<String, Value>, b: &Map<String, Value>) -> Map<String, Value> {
    for (k, v) in b {
        match (k.as_str(), a.get_mut(k)) {
            ("properties", Some(Value::Object(ap))) => {
                if let Value::Object(bp) = v {
                    for (name, schema) in bp {
                        match ap.get(name) {
                            Some(existing) => {
                                let both = serde_json::json!({ "allOf": [existing, schema] });
                                ap.insert(name.clone(), both);
                            }
                            None => {
                                ap.insert(name.clone(), schema.clone());
                            }
                        }
                    }
                }
            }
            ("required" | "dependentRequired", Some(Value::Array(ar))) => {
                for r in v.as_array().into_iter().flatten() {
                    if !ar.contains(r) {
                        ar.push(r.clone());
                    }
                }
            }
            ("minimum" | "exclusiveMinimum" | "minLength" | "minItems" | "minProperties" | "minContains", Some(slot)) => {
                tighten(slot, v, true)
            }
            ("maximum" | "exclusiveMaximum" | "maxLength" | "maxItems" | "maxProperties" | "maxContains", Some(slot)) => {
                tighten(slot, v, false)
            }
            ("enum", Some(Value::Array(ae))) => {
                if let Value::Array(be) = v {
                    ae.retain(|x| be.contains(x));
                }
            }
            ("uniqueItems", Some(slot)) => {
                if v == &Value::Bool(true) {
                    *slot = Value::Bool(true);
                }
            }
            ("items" | "additionalProperties" | "contains" | "propertyNames", Some(slot)) => {
                if slot != v && !slot.is_boolean() && !v.is_boolean() {
                    *slot = serde_json::json!({ "allOf": [slot.clone(), v.clone()] });
                } else if slot == &Value::Bool(true) {
                    *slot = v.clone();
                }
            }
            (_, Some(_)) => {}
            (_, None) => {
                a.insert(k.clone(), v.clone());
            }
        }
    }
    a
}

/// Keeps the tighter of two bounds: the larger for a lower bound (`lower`), the smaller for an upper one.
fn tighten(slot: &mut Value, other: &Value, lower: bool) {
    if let (Some(x), Some(y)) = (slot.as_f64(), other.as_f64())
        && (if lower { y > x } else { y < x })
    {
        *slot = other.clone();
    }
}

/// The `type` keyword (first non-`null` of a list), or the type implied by the other keywords.
fn schema_type(m: &Map<String, Value>) -> Option<&str> {
    match m.get("type") {
        Some(Value::String(s)) => return Some(s),
        Some(Value::Array(a)) => {
            let names: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            return names.iter().find(|n| **n != "null").or(names.first()).copied();
        }
        _ => {}
    }
    let has = |keys: &[&str]| keys.iter().any(|k| m.contains_key(*k));
    if has(&[
        "properties",
        "required",
        "additionalProperties",
        "minProperties",
        "dependentRequired",
    ]) {
        Some("object")
    } else if has(&["items", "prefixItems", "minItems", "maxItems", "contains", "uniqueItems"]) {
        Some("array")
    } else if has(&["pattern", "format", "minLength", "maxLength"]) {
        Some("string")
    } else if has(&["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "multipleOf"]) {
        Some("number")
    } else {
        None
    }
}

fn violates(v: &Value, not: &Value) -> bool {
    if let Some(c) = not.get("const") {
        return v == c;
    }
    if let Some(Value::Array(a)) = not.get("enum") {
        return a.contains(v);
    }
    let names: Vec<&str> = match not.get("type") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => return false,
    };
    names.iter().any(|t| match *t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        _ => false,
    })
}

fn lorem_word(seed: u64) -> Value {
    Value::String(word(&mut Rng::new(seed)).to_string())
}

fn float_value(x: f64) -> Value {
    Number::from_f64(x).map(Value::Number).unwrap_or_else(|| Value::from(0))
}

/// `(lo, lo_exclusive, hi, hi_exclusive)` with the default range `0..=100` (100 from the one bound given).
fn bounds(m: &Map<String, Value>) -> (f64, bool, f64, bool) {
    let flag = |k: &str| m.get(k) == Some(&Value::Bool(true));
    let mut lo = m.get("minimum").and_then(Value::as_f64).map(|v| (v, flag("exclusiveMinimum")));
    if let Some(e) = m.get("exclusiveMinimum").and_then(Value::as_f64)
        && lo.is_none_or(|(v, _)| e >= v)
    {
        lo = Some((e, true));
    }
    let mut hi = m.get("maximum").and_then(Value::as_f64).map(|v| (v, flag("exclusiveMaximum")));
    if let Some(e) = m.get("exclusiveMaximum").and_then(Value::as_f64)
        && hi.is_none_or(|(v, _)| e <= v)
    {
        hi = Some((e, true));
    }
    match (lo, hi) {
        (Some((l, le)), Some((h, he))) => (l, le, h, he),
        (Some((l, le)), None) => (l, le, l + 100.0, false),
        (None, Some((h, he))) => (h - 100.0, false, h, he),
        (None, None) => (0.0, false, 100.0, false),
    }
}

fn multiple_of(m: &Map<String, Value>) -> Option<f64> {
    m.get("multipleOf").and_then(Value::as_f64).filter(|v| *v > 0.0)
}

fn integer(m: &Map<String, Value>, seed: u64) -> Value {
    let (lo, lo_ex, hi, hi_ex) = bounds(m);
    let lo_i = if lo_ex { lo.floor() + 1.0 } else { lo.ceil() };
    let hi_i = if hi_ex { hi.ceil() - 1.0 } else { hi.floor() };
    let mut rng = Rng::new(seed);
    if let Some(step) = multiple_of(m) {
        let k_lo = (lo_i / step).ceil();
        let k_hi = (hi_i / step).floor();
        let k = if k_lo > k_hi {
            k_lo
        } else {
            k_lo + rng.below((k_hi - k_lo) as u64 + 1) as f64
        };
        return Value::from((k * step).round() as i64);
    }
    let (lo_i, hi_i) = (lo_i as i128, hi_i as i128);
    let v = if lo_i > hi_i {
        lo_i
    } else {
        lo_i + (rng.next() as i128) % (hi_i - lo_i + 1)
    };
    Value::from(v.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

fn number(m: &Map<String, Value>, seed: u64) -> Value {
    let (lo, lo_ex, hi, hi_ex) = bounds(m);
    let mut rng = Rng::new(seed);
    if let Some(step) = multiple_of(m) {
        let mut k_lo = (lo / step).ceil();
        if lo_ex && k_lo * step <= lo {
            k_lo += 1.0;
        }
        let mut k_hi = (hi / step).floor();
        if hi_ex && k_hi * step >= hi {
            k_hi -= 1.0;
        }
        let k = if k_lo > k_hi {
            k_lo
        } else {
            k_lo + rng.below((k_hi - k_lo) as u64 + 1) as f64
        };
        return float_value((k * step * 1e10).round() / 1e10);
    }
    let mut lo_c = (lo * 100.0).ceil();
    if lo_ex && lo_c / 100.0 <= lo {
        lo_c += 1.0;
    }
    let mut hi_c = (hi * 100.0).floor();
    if hi_ex && hi_c / 100.0 >= hi {
        hi_c -= 1.0;
    }
    if lo_c > hi_c {
        // Narrower than 0.01: the midpoint, unrounded.
        return float_value((lo + hi) / 2.0);
    }
    float_value((lo_c + rng.below((hi_c - lo_c) as u64 + 1) as f64) / 100.0)
}

fn string(m: &Map<String, Value>, seed: u64) -> Value {
    let min = m.get("minLength").and_then(Value::as_u64).map(|v| v as usize);
    let max = m.get("maxLength").and_then(Value::as_u64).map(|v| v as usize);
    if let Some(p) = m.get("pattern").and_then(Value::as_str)
        && let Some(re) = Pattern::parse(p)
    {
        let fits = |s: &str| {
            let n = s.chars().count();
            min.is_none_or(|m| n >= m) && max.is_none_or(|m| n <= m)
        };
        let mut out = re.generate(&mut Rng::new(seed));
        for t in 1..TRIES {
            if fits(&out) {
                break;
            }
            out = re.generate(&mut Rng::new(child(seed, &format!("p{t}"))));
        }
        return Value::String(out);
    }
    if let Some(f) = m.get("format").and_then(Value::as_str)
        && let Some(s) = formatted(f, seed)
    {
        return Value::String(s);
    }
    let lo = min.unwrap_or_else(|| 4.min(max.unwrap_or(4)));
    let hi = max.unwrap_or_else(|| 24.max(lo)).max(lo);
    let mut rng = Rng::new(seed);
    let target = rng.between(lo as u64, hi as u64) as usize;
    let mut s = String::new();
    while s.len() < target {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(word(&mut rng));
    }
    s.truncate(target);
    if s.ends_with(' ') {
        s.pop();
        s.push('x');
    }
    Value::String(s)
}

fn formatted(format: &str, seed: u64) -> Option<String> {
    let mut r = Rng::new(seed);
    let date = |r: &mut Rng| format!("{:04}-{:02}-{:02}", 2020 + r.below(10), 1 + r.below(12), 1 + r.below(28));
    let time = |r: &mut Rng| format!("{:02}:{:02}:{:02}", r.below(24), r.below(60), r.below(60));
    Some(match format {
        "date-time" => format!("{}T{}Z", date(&mut r), time(&mut r)),
        "date" => date(&mut r),
        "time" => format!("{}Z", time(&mut r)),
        "email" => format!("{}{}@example.com", word(&mut r), r.below(100)),
        "uri" | "url" => format!("https://example.com/{}", word(&mut r)),
        "hostname" => format!("{}.example.com", word(&mut r)),
        "ipv4" => format!("{}.{}.{}.{}", 1 + r.below(223), r.below(256), r.below(256), 1 + r.below(254)),
        "uuid" => {
            let hex = format!("{:016x}{:016x}", r.next(), r.next());
            let variant = ['8', '9', 'a', 'b'][r.below(4) as usize];
            format!(
                "{}-{}-4{}-{}{}-{}",
                &hex[0..8],
                &hex[8..12],
                &hex[13..16],
                variant,
                &hex[17..20],
                &hex[20..32]
            )
        }
        _ => return None,
    })
}

/// A regex from the supported subset, as a tree the generator walks.
enum Node {
    Lit(char),
    /// `(ranges, negated)`.
    Class(Vec<(char, char)>, bool),
    Group(Vec<Vec<Node>>),
    Repeat(Box<Node>, u32, u32),
}

struct Pattern(Vec<Vec<Node>>);

impl Pattern {
    /// `None` for anything outside the subset: lookaround, backreferences, lazy quantifiers, Unicode classes, flags.
    fn parse(src: &str) -> Option<Pattern> {
        let chars: Vec<char> = src.chars().collect();
        let mut pos = 0;
        let alts = parse_alts(&chars, &mut pos, false)?;
        (pos == chars.len()).then_some(Pattern(alts))
    }

    fn generate(&self, rng: &mut Rng) -> String {
        let mut out = String::new();
        gen_alts(&self.0, rng, &mut out);
        out
    }
}

const DIGIT: [(char, char); 1] = [('0', '9')];
const WORD: [(char, char); 4] = [('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')];
const SPACE: [(char, char); 1] = [(' ', ' ')];

fn parse_alts(c: &[char], pos: &mut usize, in_group: bool) -> Option<Vec<Vec<Node>>> {
    let mut alts = vec![Vec::new()];
    while *pos < c.len() {
        let ch = c[*pos];
        match ch {
            ')' if in_group => break,
            ')' => return None,
            '|' => {
                *pos += 1;
                alts.push(Vec::new());
                continue;
            }
            '^' if *pos == 0 => {
                *pos += 1;
                continue;
            }
            '$' if *pos + 1 == c.len() => {
                *pos += 1;
                continue;
            }
            '^' | '$' => return None,
            _ => {}
        }
        let atom = match ch {
            '(' => {
                *pos += 1;
                if c.get(*pos) == Some(&'?') {
                    if c.get(*pos + 1) == Some(&':') {
                        *pos += 2;
                    } else {
                        return None;
                    }
                }
                let inner = parse_alts(c, pos, true)?;
                if c.get(*pos) != Some(&')') {
                    return None;
                }
                *pos += 1;
                Node::Group(inner)
            }
            '[' => {
                *pos += 1;
                parse_class(c, pos)?
            }
            '.' => {
                *pos += 1;
                Node::Class(vec![('a', 'z'), ('0', '9')], false)
            }
            '\\' => {
                *pos += 1;
                let e = *c.get(*pos)?;
                *pos += 1;
                escape(e)?
            }
            '*' | '+' | '?' | '{' => return None,
            _ => {
                *pos += 1;
                Node::Lit(ch)
            }
        };
        let atom = quantify(c, pos, atom)?;
        alts.last_mut()?.push(atom);
    }
    Some(alts)
}

fn quantify(c: &[char], pos: &mut usize, atom: Node) -> Option<Node> {
    let (min, max) = match c.get(*pos) {
        Some('?') => (0, 1),
        Some('*' | '+') => (1, 3),
        Some('{') => {
            let close = c[*pos..].iter().position(|x| *x == '}')? + *pos;
            let body: String = c[*pos + 1..close].iter().collect();
            *pos = close;
            let (a, b) = match body.split_once(',') {
                Some((a, "")) => {
                    let a: u32 = a.parse().ok()?;
                    (a, a + 2)
                }
                Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
                None => {
                    let a: u32 = body.parse().ok()?;
                    (a, a)
                }
            };
            if a > b || b > 100 {
                return None;
            }
            (a, b)
        }
        _ => return Some(atom),
    };
    *pos += 1;
    // Lazy (`*?`) and possessive forms are outside the subset.
    if matches!(c.get(*pos), Some('?' | '+')) {
        return None;
    }
    Some(Node::Repeat(Box::new(atom), min, max))
}

fn escape(e: char) -> Option<Node> {
    Some(match e {
        'd' => Node::Class(DIGIT.to_vec(), false),
        'D' => Node::Class(DIGIT.to_vec(), true),
        'w' => Node::Class(WORD.to_vec(), false),
        'W' => Node::Class(WORD.to_vec(), true),
        's' => Node::Class(SPACE.to_vec(), false),
        'S' => Node::Class(SPACE.to_vec(), true),
        'n' => Node::Lit('\n'),
        't' => Node::Lit('\t'),
        'r' => Node::Lit('\r'),
        c if c.is_ascii_alphanumeric() => return None,
        c => Node::Lit(c),
    })
}

fn parse_class(c: &[char], pos: &mut usize) -> Option<Node> {
    let negated = c.get(*pos) == Some(&'^');
    if negated {
        *pos += 1;
    }
    let mut ranges: Vec<(char, char)> = Vec::new();
    let mut first = true;
    loop {
        let ch = *c.get(*pos)?;
        *pos += 1;
        if ch == ']' && !first {
            break;
        }
        first = false;
        let lo = if ch == '\\' {
            let e = *c.get(*pos)?;
            *pos += 1;
            match escape(e)? {
                Node::Lit(l) => l,
                Node::Class(r, false) => {
                    ranges.extend(r);
                    continue;
                }
                _ => return None,
            }
        } else if ch == '[' {
            return None;
        } else {
            ch
        };
        if c.get(*pos) == Some(&'-') && c.get(*pos + 1).is_some_and(|x| *x != ']') {
            let mut hi = c[*pos + 1];
            *pos += 2;
            if hi == '\\' {
                let e = *c.get(*pos)?;
                *pos += 1;
                match escape(e)? {
                    Node::Lit(l) => hi = l,
                    _ => return None,
                }
            }
            if hi < lo {
                return None;
            }
            ranges.push((lo, hi));
        } else {
            ranges.push((lo, lo));
        }
    }
    Some(Node::Class(ranges, negated))
}

fn gen_alts(alts: &[Vec<Node>], rng: &mut Rng, out: &mut String) {
    let alt = &alts[rng.below(alts.len() as u64) as usize];
    for n in alt {
        gen_node(n, rng, out);
    }
}

fn gen_node(n: &Node, rng: &mut Rng, out: &mut String) {
    match n {
        Node::Lit(c) => out.push(*c),
        Node::Group(alts) => gen_alts(alts, rng, out),
        Node::Repeat(inner, min, max) => {
            for _ in 0..rng.between(u64::from(*min), u64::from(*max)) {
                gen_node(inner, rng, out);
            }
        }
        Node::Class(ranges, false) => {
            let total: u32 = ranges.iter().map(|(a, b)| *b as u32 - *a as u32 + 1).sum();
            let mut k = rng.below(u64::from(total.max(1))) as u32;
            for (a, b) in ranges {
                let size = *b as u32 - *a as u32 + 1;
                if k < size {
                    out.push(char::from_u32(*a as u32 + k).unwrap_or('x'));
                    return;
                }
                k -= size;
            }
            out.push('x');
        }
        Node::Class(ranges, true) => {
            let allowed: Vec<char> = ('a'..='z')
                .chain('A'..='Z')
                .chain('0'..='9')
                .chain(' '..='~')
                .filter(|c| !ranges.iter().any(|(a, b)| a <= c && c <= b))
                .collect();
            // The letters come first, so most picks are letters and digits.
            let pool = &allowed[..allowed.len().min(62)];
            out.push(if pool.is_empty() {
                'x'
            } else {
                pool[rng.below(pool.len() as u64) as usize]
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn many(schema: &Value) -> Vec<Value> {
        (0..40).map(|seed| instance(schema, seed)).collect()
    }

    #[test]
    fn deterministic_and_seed_dependent() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "string"}}, "required": ["a", "b"]});
        assert_eq!(instance(&schema, 7), instance(&schema, 7));
        assert_ne!(instance(&schema, 7), instance(&schema, 8));
    }

    #[test]
    fn siblings_do_not_shift_when_a_property_is_added() {
        let one = json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]});
        let two = json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "string"}}, "required": ["a", "b"]});
        assert_eq!(instance(&one, 3)["a"], instance(&two, 3)["a"]);
    }

    #[test]
    fn const_enum_examples_default() {
        assert_eq!(instance(&json!({"const": {"x": 1}}), 1), json!({"x": 1}));
        for v in many(&json!({"enum": ["a", "b", "c"]})) {
            assert!(["a", "b", "c"].contains(&v.as_str().unwrap()));
        }
        assert_eq!(instance(&json!({"type": "string", "examples": ["hello"]}), 1), json!("hello"));
        assert_eq!(instance(&json!({"type": "integer", "default": 42}), 1), json!(42));
    }

    #[test]
    fn numbers_stay_inside_their_bounds() {
        for v in many(&json!({"type": "integer", "minimum": 5, "maximum": 9})) {
            assert!((5..=9).contains(&v.as_i64().unwrap()), "{v}");
        }
        for v in many(&json!({"type": "integer", "exclusiveMinimum": 5, "exclusiveMaximum": 7})) {
            assert_eq!(v, json!(6));
        }
        for v in many(&json!({"type": "integer", "minimum": 1, "exclusiveMinimum": true, "maximum": 3})) {
            assert!((2..=3).contains(&v.as_i64().unwrap()));
        }
        for v in many(&json!({"type": "integer", "multipleOf": 7, "minimum": 10, "maximum": 50})) {
            let n = v.as_i64().unwrap();
            assert!(n % 7 == 0 && (10..=50).contains(&n), "{n}");
        }
        for v in many(&json!({"type": "number", "minimum": 0.5, "maximum": 0.6})) {
            let n = v.as_f64().unwrap();
            assert!((0.5..=0.6).contains(&n), "{n}");
        }
        for v in many(&json!({"type": "number", "multipleOf": 0.25, "maximum": 3})) {
            let n = v.as_f64().unwrap();
            assert!((n / 0.25).fract() == 0.0 && n <= 3.0, "{n}");
        }
        for v in many(&json!({"type": "integer", "minimum": 1000})) {
            assert!(v.as_i64().unwrap() >= 1000);
        }
        // An empty range returns the lower bound.
        assert_eq!(instance(&json!({"type": "integer", "minimum": 5, "maximum": 3}), 1), json!(5));
    }

    #[test]
    fn strings_honor_length_and_format() {
        for v in many(&json!({"type": "string", "minLength": 3, "maxLength": 5})) {
            assert!((3..=5).contains(&v.as_str().unwrap().len()), "{v}");
        }
        for v in many(&json!({"type": "string", "minLength": 40})) {
            assert!(v.as_str().unwrap().len() >= 40);
        }
        for v in many(&json!({"type": "string", "maxLength": 0})) {
            assert_eq!(v, json!(""));
        }
        let re = |p: &str| regex::Regex::new(p).unwrap();
        let cases = [
            ("date-time", r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$"),
            ("date", r"^\d{4}-\d\d-\d\d$"),
            ("time", r"^\d\d:\d\d:\d\dZ$"),
            ("email", r"^\w+@example\.com$"),
            ("uri", r"^https://example\.com/\w+$"),
            ("uuid", r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"),
            ("hostname", r"^\w+\.example\.com$"),
            ("ipv4", r"^\d+\.\d+\.\d+\.\d+$"),
        ];
        for (format, pattern) in cases {
            for v in many(&json!({"type": "string", "format": format})) {
                assert!(re(pattern).is_match(v.as_str().unwrap()), "{format}: {v}");
            }
        }
    }

    #[test]
    fn patterns_from_the_subset() {
        for pattern in [
            r"^[A-Z]{2}-\d{3}$",
            r"^(foo|bar)_[a-z0-9]+$",
            r"^\w+@\w+\.(com|org)$",
            r"^[^0-9]{4}$",
            r"^v?\d+(\.\d+)?$",
            r"^a.c$",
            r"^(?:ab)+$",
        ] {
            let re = regex::Regex::new(pattern).unwrap();
            for v in many(&json!({"type": "string", "pattern": pattern})) {
                assert!(re.is_match(v.as_str().unwrap()), "{pattern}: {v}");
            }
        }
        // Outside the subset: ignored, a plain string comes back.
        for pattern in [r"^(?=a)a", r"(a)\1", r"a+?", r"\p{L}+"] {
            assert!(instance(&json!({"type": "string", "pattern": pattern}), 1).is_string());
        }
    }

    #[test]
    fn arrays() {
        let schema = json!({"type": "array", "items": {"type": "integer"}, "minItems": 3});
        for v in many(&schema) {
            let n = v.as_array().unwrap().len();
            assert!((3..=3).contains(&n));
        }
        for v in many(&json!({"type": "array", "items": {"type": "string"}, "maxItems": 1})) {
            assert!(v.as_array().unwrap().len() <= 1);
        }
        for v in many(&json!({"type": "array", "items": {"enum": [1, 2, 3, 4, 5, 6]}, "minItems": 4, "uniqueItems": true})) {
            let a = v.as_array().unwrap();
            for (i, x) in a.iter().enumerate() {
                assert!(!a[..i].contains(x), "{v}");
            }
        }
        let tuple = instance(
            &json!({"type": "array", "prefixItems": [{"type": "string"}, {"type": "integer"}]}),
            1,
        );
        assert!(tuple[0].is_string() && tuple[1].is_i64());
        for v in many(&json!({"type": "array", "items": {"type": "string"}, "contains": {"const": "needle"}})) {
            assert!(v.as_array().unwrap().contains(&json!("needle")));
        }
    }

    #[test]
    fn objects() {
        let schema = json!({
            "type": "object",
            "properties": {"id": {"type": "integer"}, "name": {"type": "string"}, "tag": {"type": "string"}, "extra": {"type": "boolean"}},
            "required": ["id", "name"],
            "additionalProperties": false,
            "dependentRequired": {"tag": ["extra"]}
        });
        let vs = many(&schema);
        for v in &vs {
            let o = v.as_object().unwrap();
            assert!(o.contains_key("id") && o.contains_key("name"));
            assert!(o.keys().all(|k| ["id", "name", "tag", "extra"].contains(&k.as_str())));
            if o.contains_key("tag") {
                assert!(o.contains_key("extra"));
            }
        }
        assert!(vs.iter().any(|v| v.get("tag").is_some()) && vs.iter().any(|v| v.get("tag").is_none()));
        for v in many(&json!({"type": "object", "minProperties": 2})) {
            assert!(v.as_object().unwrap().len() >= 2);
        }
        // No `type` but `properties`.
        assert!(instance(&json!({"properties": {"a": {"const": 1}}, "required": ["a"]}), 1).is_object());
    }

    #[test]
    fn refs_and_combinators() {
        let schema = json!({
            "$defs": {"point": {"type": "object", "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}}, "required": ["x", "y"]}},
            "type": "object",
            "properties": {"p": {"$ref": "#/$defs/point"}, "q": {"anyOf": [{"type": "string"}, {"type": "integer"}]}},
            "required": ["p", "q"]
        });
        for v in many(&schema) {
            assert!(v["p"]["x"].is_i64() && v["p"]["y"].is_i64());
            assert!(v["q"].is_string());
        }
        let all = json!({"allOf": [
            {"type": "object", "properties": {"a": {"type": "integer"}}, "required": ["a"]},
            {"properties": {"b": {"type": "string"}}, "required": ["b"]}
        ]});
        let v = instance(&all, 1);
        assert!(v["a"].is_i64() && v["b"].is_string());
        let defs = json!({"definitions": {"n": {"type": "integer", "minimum": 3, "maximum": 3}}, "$ref": "#/definitions/n"});
        assert_eq!(instance(&defs, 1), json!(3));
    }

    #[test]
    fn all_of_keeps_the_tighter_bound_of_both_sides() {
        let s = json!({"type": "integer", "minimum": 0, "maximum": 100, "allOf": [{"minimum": 5, "maximum": 10}]});
        for v in many(&s) {
            assert!((5..=10).contains(&v.as_i64().unwrap()), "{v}");
        }
        let s = json!({"enum": ["a", "b", "c"], "allOf": [{"enum": ["c", "d"]}]});
        assert_eq!(instance(&s, 3), json!("c"));
        let s = json!({"type": "array", "items": {"type": "integer"}, "minItems": 1, "maxItems": 9, "allOf": [{"maxItems": 2, "items": {"minimum": 7, "maximum": 8}}]});
        for v in many(&s) {
            let a = v.as_array().unwrap();
            assert!(a.len() <= 2 && a.iter().all(|x| (7..=8).contains(&x.as_i64().unwrap())), "{v}");
        }
    }

    #[test]
    fn cycles_terminate_with_the_smallest_value() {
        let tree = json!({
            "$ref": "#/definitions/node",
            "definitions": {"node": {"type": "object", "properties": {
                "value": {"type": "integer"},
                "children": {"type": "array", "items": {"$ref": "#/definitions/node"}, "minItems": 1}
            }, "required": ["value", "children"]}}
        });
        let v = instance(&tree, 1);
        let mut depth = 0;
        let mut cur = &v;
        while let Some(c) = cur.get("children").and_then(|c| c.get(0)) {
            depth += 1;
            cur = c;
            assert!(depth < 40);
        }
        let loose = json!({"$ref": "#/definitions/n", "definitions": {"n": {"type": "object", "properties": {"next": {"$ref": "#/definitions/n"}}}}});
        assert!(instance(&loose, 1).is_object());
    }

    #[test]
    fn not_is_avoided() {
        for v in many(&json!({"type": "integer", "minimum": 1, "maximum": 3, "not": {"enum": [1, 2]}})) {
            assert_eq!(v, json!(3));
        }
    }

    #[test]
    fn untyped_schemas_give_a_word() {
        assert!(instance(&json!({}), 1).is_string());
        assert!(instance(&json!(true), 1).is_string());
        assert!(instance(&json!({"type": ["null", "boolean"]}), 1).is_boolean());
        assert_eq!(instance(&json!({"type": "null"}), 1), Value::Null);
    }
}
