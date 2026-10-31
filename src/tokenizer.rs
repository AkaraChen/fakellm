//! A deterministic approximate tokenizer. It does not match any model's vocabulary; it only has
//! to be stable (same text, same count, always) and to split text into pieces the size of real
//! tokens, so streamed chunks and `usage` look like the real thing. The split is also what the
//! stream renderer emits one chunk at a time, so the pieces must concatenate back to the input.
//!
//! Rules: a run of letters/digits with its leading space is one token (long runs are split every
//! few characters), each CJK character is one token, each punctuation character is one token,
//! other whitespace runs are one token.

use crate::ir::Request;

/// Chunk sizes for words longer than [`MAX_WORD`], cycled so splits do not look mechanical.
const LONG_WORD_CHUNKS: [usize; 4] = [4, 5, 6, 5];
const MAX_WORD: usize = 6;

fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30ff}' // Hiragana, Katakana
        | '\u{3400}'..='\u{4dbf}'
        | '\u{4e00}'..='\u{9fff}'
        | '\u{ac00}'..='\u{d7af}' // Hangul syllables
        | '\u{1100}'..='\u{11ff}' // Hangul Jamo
        | '\u{f900}'..='\u{faff}'
        | '\u{ff66}'..='\u{ff9f}' // half-width Katakana
        | '\u{20000}'..='\u{2fa1f}')
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() && !is_cjk(c)
}

/// Splits `text` into token-sized pieces; `pieces.concat() == text`.
pub fn tokenize(text: &str) -> Vec<&str> {
    let mut out = Vec::with_capacity(text.len() / 4 + 1);
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let mut chars = rest.char_indices();
        if is_word(c) || (c == ' ' && rest[1..].chars().next().is_some_and(is_word)) {
            let lead = if c == ' ' { 1 } else { 0 };
            let body = &rest[lead..];
            let end = body.find(|ch: char| !is_word(ch)).unwrap_or(body.len());
            let n_chars = body[..end].chars().count();
            if n_chars <= MAX_WORD {
                out.push(&rest[..lead + end]);
            } else {
                let mut start = 0;
                let mut taken = 0;
                let mut i = 0;
                while taken < n_chars {
                    let left = n_chars - taken;
                    let mut size = LONG_WORD_CHUNKS[i % LONG_WORD_CHUNKS.len()];
                    // Avoid a dangling one-character tail.
                    if left <= size + 1 {
                        size = left;
                    }
                    let stop = body[start..].char_indices().nth(size).map_or(body.len(), |(b, _)| start + b);
                    let from = if taken == 0 { 0 } else { lead + start };
                    out.push(&rest[from..lead + stop]);
                    start = stop;
                    taken += size;
                    i += 1;
                }
            }
            rest = &rest[lead + end..];
        } else if c.is_whitespace() {
            let end = rest.find(|ch: char| !ch.is_whitespace()).unwrap_or(rest.len());
            let mut stop = end;
            // The last plain space belongs to the word that follows.
            if stop < rest.len() && rest[..stop].ends_with(' ') && rest[stop..].chars().next().is_some_and(is_word) {
                stop -= 1;
            }
            if stop == 0 {
                // Unreachable for a lone space (handled above), kept as a guard against empty pieces.
                stop = c.len_utf8();
            }
            out.push(&rest[..stop]);
            rest = &rest[stop..];
        } else {
            let len = chars.nth(1).map_or(rest.len(), |(b, _)| b);
            out.push(&rest[..len]);
            rest = &rest[len..];
        }
    }
    out
}

pub fn count(text: &str) -> u32 {
    tokenize(text).len() as u32
}

/// Prompt tokens for a request: every message's text (plus a fixed per-message overhead, as the
/// OpenAI cookbook formula does), tool call arguments, tool results, and the tool definitions.
pub fn count_request(req: &Request) -> u32 {
    let mut total = 3;
    for m in &req.messages {
        total += 4 + count(&m.text_content());
        for c in &m.tool_calls {
            total += count(&c.name) + count(&c.arguments);
        }
        if let Some(r) = &m.reasoning {
            total += count(r);
        }
    }
    for t in &req.tools {
        if let Ok(json) = serde_json::to_string(t) {
            total += count(&json);
        }
    }
    total
}

/// The first `max_tokens` tokens of `text`, and whether anything was cut.
pub fn truncate(text: &str, max_tokens: u32) -> (String, bool) {
    let pieces = tokenize(text);
    let keep = max_tokens as usize;
    if pieces.len() <= keep {
        return (text.to_string(), false);
    }
    (pieces[..keep].concat(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Message, Role, ToolCall, ToolDef};

    #[test]
    fn pieces_concat_to_input() {
        for s in [
            "",
            "Hello, world!",
            "  leading and   multiple   spaces\n\n\tnewline",
            "extraordinarily internationalization 1234567890123",
            "你好，世界 hello 안녕하세요 こんにちは",
            "naïve café — “quoted” 😀 emoji",
            "trailing space ",
            " ",
        ] {
            let pieces = tokenize(s);
            assert_eq!(pieces.concat(), s);
            assert!(pieces.iter().all(|p| !p.is_empty()), "{s:?}");
        }
    }

    #[test]
    fn word_with_leading_space_is_one_token() {
        assert_eq!(tokenize("Hello world"), vec!["Hello", " world"]);
        assert_eq!(tokenize("Hi, you"), vec!["Hi", ",", " you"]);
        assert_eq!(tokenize("a  b"), vec!["a", " ", " b"]);
    }

    #[test]
    fn long_words_split() {
        let pieces = tokenize(" internationalization");
        assert!(pieces.len() >= 3, "{pieces:?}");
        assert!(pieces[0].starts_with(' '));
        assert!(pieces.iter().all(|p| p.trim_start().chars().count() <= 6));
        assert_eq!(count("abcdef"), 1);
    }

    #[test]
    fn cjk_and_punctuation_one_each() {
        assert_eq!(count("你好世界"), 4);
        assert_eq!(count("안녕"), 2);
        assert_eq!(count("!?"), 2);
    }

    #[test]
    fn truncate_keeps_prefix() {
        let (t, cut) = truncate("one two three four", 2);
        assert_eq!(t, "one two");
        assert!(cut);
        let (t, cut) = truncate("one two", 5);
        assert_eq!(t, "one two");
        assert!(!cut);
        assert_eq!(truncate("one", 0), (String::new(), true));
    }

    #[test]
    fn request_count() {
        let mut req = Request::new("m", vec![Message::text(Role::User, "hi there")]);
        assert_eq!(count_request(&req), 3 + 4 + 2);
        let mut a = Message::text(Role::Assistant, "");
        a.tool_calls.push(ToolCall {
            id: "c".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        req.messages.push(a);
        let before = count_request(&req);
        assert!(before > 9 + 4);
        req.tools.push(ToolDef {
            name: "bash".into(),
            description: None,
            parameters: None,
            strict: None,
        });
        assert!(count_request(&req) > before);
    }
}
