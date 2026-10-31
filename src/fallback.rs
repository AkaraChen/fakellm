//! The deterministic default reply, used when no script rule matches and strict mode is off.
//! Text is pseudo-random lorem seeded by a hash of the request, so the same request always gets
//! the same reply and different requests get different ones. Also home to the other derived
//! identifiers (completion ids, system fingerprints) so they are stable across runs too.

use crate::ir::{FinishReason, Reply, Request, ResponseFormat, ToolCall, ToolChoice};
use crate::tokenizer;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FallbackConfig {
    /// Length of the generated text, in tokens, before `max_tokens` truncation.
    pub tokens: u32,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self { tokens: 32 }
    }
}

const WORDS: [&str; 64] = [
    "lorem",
    "ipsum",
    "dolor",
    "sit",
    "amet",
    "consectetur",
    "adipiscing",
    "elit",
    "sed",
    "do",
    "eiusmod",
    "tempor",
    "incididunt",
    "ut",
    "labore",
    "et",
    "dolore",
    "magna",
    "aliqua",
    "enim",
    "ad",
    "minim",
    "veniam",
    "quis",
    "nostrud",
    "exercitation",
    "ullamco",
    "laboris",
    "nisi",
    "aliquip",
    "ex",
    "ea",
    "commodo",
    "consequat",
    "duis",
    "aute",
    "irure",
    "in",
    "reprehenderit",
    "voluptate",
    "velit",
    "esse",
    "cillum",
    "eu",
    "fugiat",
    "nulla",
    "pariatur",
    "excepteur",
    "sint",
    "occaecat",
    "cupidatat",
    "non",
    "proident",
    "sunt",
    "culpa",
    "qui",
    "officia",
    "deserunt",
    "mollit",
    "anim",
    "id",
    "est",
    "laborum",
    "vitae",
];

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Builds the default reply for `req`. Honors `max_tokens` (truncating to `finish: Length`),
/// `response_format` (`json_object`/`json_schema` get `{"message": "<lorem>"}`), and a
/// `tool_choice` that forces a tool call (`required` or a named tool: one call with `{}`
/// arguments). Never calls a tool otherwise, so an agent loop ends instead of spinning.
/// `usage` is left zeroed; the engine fills it in.
pub fn reply(req: &Request, cfg: &FallbackConfig) -> Reply {
    let seed = request_seed(req);

    let forced = match &req.tool_choice {
        Some(ToolChoice::Tool { name }) => Some(name.clone()),
        Some(ToolChoice::Required) => req.tools.first().map(|t| t.name.clone()),
        _ => None,
    };
    if let Some(name) = forced {
        return Reply {
            text: None,
            tool_calls: vec![ToolCall {
                id: call_id(seed),
                name,
                arguments: "{}".into(),
            }],
            finish: FinishReason::ToolCalls,
            ..Reply::default()
        };
    }

    let json_mode = matches!(
        req.response_format,
        Some(ResponseFormat::JsonObject | ResponseFormat::JsonSchema { .. })
    );
    let wrap = |t: &str| serde_json::json!({ "message": t }).to_string();
    let mut text = lorem(seed, cfg.tokens);
    let mut finish = FinishReason::Stop;
    if let Some(max) = req.max_tokens {
        // Reserve the JSON wrapper so the truncated reply stays valid JSON and within `max`.
        let budget = if json_mode {
            max.saturating_sub(tokenizer::count(&wrap("")))
        } else {
            max
        };
        if budget < cfg.tokens {
            text = tokenizer::truncate(&text, budget).0;
            finish = FinishReason::Length;
        }
    }
    if json_mode {
        text = wrap(&text);
        if let Some(max) = req.max_tokens
            && tokenizer::count(&text) > max
        {
            text = tokenizer::truncate(&text, max).0;
            finish = FinishReason::Length;
        }
    }
    Reply {
        text: Some(text),
        finish,
        ..Reply::default()
    }
}

/// Lorem text of exactly `tokens` tokens (by [`tokenizer::count`]).
fn lorem(seed: u64, tokens: u32) -> String {
    let mut rng = Xorshift::new(seed);
    let mut text = String::new();
    let mut counted = 0u32;
    let mut sentence_len = 0;
    let mut sentence_target = 8 + rng.next() % 7;
    while counted < tokens {
        let word = WORDS[(rng.next() % WORDS.len() as u64) as usize];
        let start = text.is_empty() || text.ends_with(". ");
        let piece = if start {
            let mut cs = word.chars();
            let first = cs.next().map(|c| c.to_ascii_uppercase()).unwrap_or_default();
            format!("{first}{}", cs.as_str())
        } else {
            word.to_string()
        };
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&piece);
        sentence_len += 1;
        if sentence_len as u64 >= sentence_target {
            text.push('.');
            sentence_len = 0;
            sentence_target = 8 + rng.next() % 7;
        }
        counted = tokenizer::count(&text);
    }
    tokenizer::truncate(&text, tokens).0
}

struct Xorshift(u64);

impl Xorshift {
    fn new(seed: u64) -> Self {
        let mut s = seed;
        let state = splitmix64(&mut s);
        Self(if state == 0 { 0x9e37_79b9_7f4a_7c15 } else { state })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11
    }
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn fnv_update(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// FNV-1a over `bytes`.
pub fn hash64(bytes: &[u8]) -> u64 {
    fnv_update(FNV_OFFSET, bytes)
}

/// A hash of the request's model and messages, the seed for everything derived from it. Tools are
/// left out so a retry of the same conversation gets the same text.
pub fn request_seed(req: &Request) -> u64 {
    // Each field is length-prefixed so ("ab","c") and ("a","bc") hash differently.
    fn field(h: u64, bytes: &[u8]) -> u64 {
        fnv_update(fnv_update(h, &(bytes.len() as u64).to_le_bytes()), bytes)
    }
    let mut h = field(FNV_OFFSET, req.model.as_bytes());
    for m in &req.messages {
        h = fnv_update(h, &[m.role as u8]);
        h = field(h, m.text_content().as_bytes());
        for c in &m.tool_calls {
            h = field(h, c.arguments.as_bytes());
        }
    }
    h
}

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

fn base62(seed: u64, salt: u64, len: usize) -> String {
    let mut state = seed ^ salt;
    (0..len).map(|_| BASE62[(splitmix64(&mut state) % 62) as usize] as char).collect()
}

/// `chatcmpl-` followed by 29 base62 characters derived from `seed`.
pub fn completion_id(seed: u64) -> String {
    format!("chatcmpl-{}", base62(seed, 0x6368_6174, 29))
}

/// `call_` followed by 24 base62 characters derived from `seed`.
pub fn call_id(seed: u64) -> String {
    format!("call_{}", base62(seed, 0x6361_6c6c, 24))
}

/// `fp_` followed by 10 hex characters derived from the model name.
pub fn fingerprint(model: &str) -> String {
    format!("fp_{:010x}", hash64(model.as_bytes()) & 0xff_ffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Message, Role, ToolDef};

    fn req(text: &str) -> Request {
        Request::new("gpt-4o", vec![Message::text(Role::User, text)])
    }

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: None,
            parameters: None,
            strict: None,
        }
    }

    #[test]
    fn deterministic_and_input_sensitive() {
        let cfg = FallbackConfig::default();
        assert_eq!(reply(&req("a"), &cfg), reply(&req("a"), &cfg));
        assert_ne!(reply(&req("a"), &cfg).text, reply(&req("b"), &cfg).text);
        let mut with_tools = req("a");
        with_tools.tools.push(tool("x"));
        assert_eq!(request_seed(&req("a")), request_seed(&with_tools));
    }

    #[test]
    fn token_count_exact() {
        for n in [0, 1, 7, 32, 100, 500] {
            let r = reply(&req("hello"), &FallbackConfig { tokens: n });
            let text = r.text.unwrap();
            assert_eq!(tokenizer::count(&text), n, "{text}");
            assert_eq!(r.finish, FinishReason::Stop);
        }
    }

    #[test]
    fn text_looks_like_sentences() {
        let text = reply(&req("hello"), &FallbackConfig { tokens: 60 }).text.unwrap();
        assert!(text.chars().next().unwrap().is_uppercase());
        assert!(text.contains(". "));
    }

    #[test]
    fn max_tokens_truncates() {
        let mut r = req("hello");
        r.max_tokens = Some(5);
        let out = reply(&r, &FallbackConfig { tokens: 32 });
        assert_eq!(out.finish, FinishReason::Length);
        assert_eq!(tokenizer::count(&out.text.unwrap()), 5);
        r.max_tokens = Some(32);
        assert_eq!(reply(&r, &FallbackConfig { tokens: 32 }).finish, FinishReason::Stop);
    }

    #[test]
    fn json_mode_is_valid_json() {
        for max in [None, Some(12), Some(3)] {
            let mut r = req("hello");
            r.response_format = Some(ResponseFormat::JsonObject);
            r.max_tokens = max;
            let out = reply(&r, &FallbackConfig::default());
            let text = out.text.unwrap();
            assert!(max.is_none_or(|m| tokenizer::count(&text) <= m));
            // Below the wrapper's own size the cap wins over validity, like a real length stop.
            if max != Some(3) {
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert!(v["message"].is_string());
            }
            assert_eq!(out.finish, if max.is_some() { FinishReason::Length } else { FinishReason::Stop });
        }
    }

    #[test]
    fn forced_tool_choice() {
        let mut r = req("hello");
        r.tools = vec![tool("first"), tool("second")];
        r.tool_choice = Some(ToolChoice::Required);
        let out = reply(&r, &FallbackConfig::default());
        assert_eq!(out.finish, FinishReason::ToolCalls);
        assert_eq!(out.text, None);
        assert_eq!(out.tool_calls[0].name, "first");
        assert_eq!(out.tool_calls[0].arguments, "{}");
        assert_eq!(out.tool_calls[0].id, call_id(request_seed(&r)));
        r.tool_choice = Some(ToolChoice::Tool { name: "second".into() });
        assert_eq!(reply(&r, &FallbackConfig::default()).tool_calls[0].name, "second");
        r.tool_choice = Some(ToolChoice::Auto);
        assert!(reply(&r, &FallbackConfig::default()).tool_calls.is_empty());
    }

    #[test]
    fn identifiers() {
        let id = completion_id(42);
        assert!(id.starts_with("chatcmpl-") && id.len() == 9 + 29);
        assert!(id[9..].chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(id, completion_id(42));
        assert_ne!(id, completion_id(43));
        let c = call_id(42);
        assert!(c.starts_with("call_") && c.len() == 5 + 24);
        let fp = fingerprint("gpt-4o");
        assert_eq!(fp.len(), 13);
        assert!(fp[3..].chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        assert_ne!(fp, fingerprint("gpt-4"));
        assert_eq!(hash64(b""), FNV_OFFSET);
    }
}
