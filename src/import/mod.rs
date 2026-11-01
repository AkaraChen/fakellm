//! `fakellm import`: turns rows of public datasets (Hugging Face) into cassettes or trajectories, so real
//! conversations and agent runs can be replayed without recording anything. See docs/m4-spec.md section 6.
//!
//! Every format converter maps one dataset row to a [`Conversation`] (IR messages, plus the tools the row
//! declares); [`to_cassette`] and [`to_trajectories`] then build the output documents from conversations alone, so
//! converters know nothing about outputs and outputs know nothing about datasets. Rows a converter does not
//! understand are skipped and counted, never fatal.

pub mod hermes;
pub mod hf;
pub mod openhands;
pub mod sharegpt;
pub mod swe_agent;
pub mod wildchat;
pub mod xlam;

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::str::FromStr;

use serde_json::{Map, Value};

use crate::agent::tool_map::ToolMap;
use crate::agent::trajectory::{Step, Trajectory};
use crate::cassette::format::{Cassette, CassetteOutcome, Exchange, ExchangeMeta};
use crate::ir::{FinishReason, Message, Protocol, Reply, Request, Role, ToolCall, ToolDef};
use crate::script::{ReplySpec, ToolCallSpec};

/// A dataset layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Look at the first row's keys ([`detect`]).
    Auto,
    /// `conversations: [{from: human|gpt|system|tool, value}]`.
    ShareGpt,
    /// `conversation: [{role, content, ...}]` (allenai/WildChat-1M and kin).
    WildChat,
    /// `conversations` with `<tool_call>` / `<tool_response>` XML inside the turns, and a `tools` column.
    HermesFc,
    /// nebius/SWE-agent-trajectories (`trajectory: [{role: system|user|ai, text}]`, actions as fenced blocks) and
    /// SWE-bench/SWE-smith-trajectories (`messages` with structured `tool_calls`).
    SweAgent,
    /// `events: [{source, action|observation, args, content, ...}]`.
    OpenHands,
    /// `query`, `answers`, `tools` (Salesforce/xlam-function-calling-60k): one request and its function calls.
    Xlam,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::ShareGpt => "sharegpt",
            Self::WildChat => "wildchat",
            Self::HermesFc => "hermes-fc",
            Self::SweAgent => "swe-agent",
            Self::OpenHands => "openhands",
            Self::Xlam => "xlam",
        }
    }

    /// What `--as` defaults to: cassettes for chat corpora, trajectories for agent runs.
    pub fn default_output(self) -> Output {
        match self {
            Self::SweAgent | Self::OpenHands | Self::HermesFc | Self::Xlam => Output::Trajectory,
            Self::Auto | Self::ShareGpt | Self::WildChat => Output::Cassette,
        }
    }
}

impl FromStr for Format {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Ok(match s {
            "auto" => Self::Auto,
            "sharegpt" => Self::ShareGpt,
            "wildchat" => Self::WildChat,
            "hermes-fc" => Self::HermesFc,
            "swe-agent" => Self::SweAgent,
            "openhands" => Self::OpenHands,
            "xlam" => Self::Xlam,
            _ => {
                return Err(format!(
                    "unknown format {s:?}, expected auto, sharegpt, wildchat, hermes-fc, swe-agent, openhands or xlam"
                ));
            }
        })
    }
}

/// What to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// One cassette holding an exchange per assistant turn of every conversation.
    Cassette,
    /// One trajectory per conversation (a multi-trajectory JSONL file).
    Trajectory,
}

impl FromStr for Output {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "cassette" => Ok(Self::Cassette),
            "trajectory" => Ok(Self::Trajectory),
            _ => Err(format!("unknown output {s:?}, expected cassette or trajectory")),
        }
    }
}

/// An import failed as a whole (network, bad arguments); skipped rows are not errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportError(pub String);

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ImportError {}

/// One dataset row, converted.
#[derive(Clone, Debug, PartialEq)]
pub struct Conversation {
    /// The row's own id (`instance_id`, `conversation_hash`, `id`, ...), else `row-<index>`.
    pub id: String,
    /// The model that produced the assistant turns, when the row says (WildChat `model`, SWE-smith `model`).
    pub model: Option<String>,
    /// Tools the row declares (Hermes `tools` column); otherwise derived from the calls the assistant makes.
    pub tools: Vec<ToolDef>,
    /// system / user / assistant / tool messages in order. Assistant messages carry text and `tool_calls`; a tool
    /// message carries its result and the `tool_call_id` of the call it answers (ids are made up where the row has
    /// none, `call_<n>`).
    pub messages: Vec<Message>,
    /// The format's default tool map ([`swe_agent::default_tool_map`] for SWE-agent); empty for the others.
    pub tool_map: ToolMap,
    /// Row fields worth keeping (`instance_id`, `exit_status`, `language`, ...); end up in `meta`.
    pub meta: Map<String, Value>,
}

/// The result of converting a batch of rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Converted {
    pub conversations: Vec<Conversation>,
    /// Rows no converter could use.
    pub skipped: usize,
    /// Why, by reason (`"no conversation field"`, `"no assistant turn"`, ...), counts summing to `skipped`.
    pub reasons: BTreeMap<String, usize>,
}

/// Guesses the format of a row from its keys: `events` is OpenHands; `trajectory` or `messages` with `tool_calls` or
/// `role: ai` is SWE-agent; `conversations` with `from` is ShareGPT, or Hermes when turns hold `<tool_call>` or
/// the row has `tools`; `conversation` with `role` is WildChat. `None` when nothing fits.
pub fn detect(row: &Value) -> Option<Format> {
    if array_field(row, "events").is_some() {
        return Some(Format::OpenHands);
    }
    if swe_agent::matches(row) {
        return Some(Format::SweAgent);
    }
    if hermes::matches(row) {
        return Some(Format::HermesFc);
    }
    if sharegpt::matches(row) {
        return Some(Format::ShareGpt);
    }
    if wildchat::matches(row) {
        return Some(Format::WildChat);
    }
    if openhands::matches(row) {
        return Some(Format::OpenHands);
    }
    if xlam::matches(row) {
        return Some(Format::Xlam);
    }
    None
}

/// Converts rows with the converter of `format` (for `Auto`: [`detect`] per row). A row that fails to convert
/// or yields no assistant turn is counted in `skipped` with its reason. Never fails.
pub fn convert_rows(rows: &[Value], format: Format) -> Converted {
    let mut out = Converted::default();
    for (index, row) in rows.iter().enumerate() {
        let format = match format {
            Format::Auto => detect(row),
            f => Some(f),
        };
        let result = match format {
            Some(Format::ShareGpt) => sharegpt::convert(row, index),
            Some(Format::WildChat) => wildchat::convert(row, index),
            Some(Format::HermesFc) => hermes::convert(row, index),
            Some(Format::SweAgent) => swe_agent::convert(row, index),
            Some(Format::OpenHands) => openhands::convert(row, index),
            Some(Format::Xlam) => xlam::convert(row, index),
            Some(Format::Auto) | None => Err("unrecognized row shape".to_owned()),
        };
        match result {
            Ok(c) => out.conversations.push(c),
            Err(reason) => {
                out.skipped += 1;
                *out.reasons.entry(reason).or_default() += 1;
            }
        }
    }
    out
}

/// Rows from the text of a local dump: a JSON array, `{"rows": [...]}` (items may be datasets-server
/// `{"row_idx", "row"}` wrappers), a single object, or JSONL with one row per line.
pub fn parse_rows(text: &str) -> Result<Vec<Value>, ImportError> {
    let rows = match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(rows)) => rows,
        Ok(Value::Object(mut o)) => match o.remove("rows") {
            Some(Value::Array(rows)) => rows,
            Some(other) => {
                o.insert("rows".into(), other);
                vec![Value::Object(o)]
            }
            None => vec![Value::Object(o)],
        },
        Ok(_) => return Err(ImportError("expected a JSON array, an object with `rows`, or JSONL".into())),
        Err(_) => {
            let mut rows = Vec::new();
            for (i, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let row = serde_json::from_str(line).map_err(|e| ImportError(format!("line {}: invalid JSON: {e}", i + 1)))?;
                rows.push(row);
            }
            rows
        }
    };
    Ok(rows
        .into_iter()
        .map(|r| match r {
            Value::Object(mut o) if o.contains_key("row_idx") && matches!(o.get("row"), Some(Value::Object(_))) => {
                o.remove("row").unwrap_or(Value::Null)
            }
            other => other,
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------------------------
// Helpers shared by the converters.

/// An array-valued field; a string holding a JSON array counts too (the datasets-server delivers some columns as
/// JSON strings).
pub(crate) fn array_field(row: &Value, key: &str) -> Option<Vec<Value>> {
    match row.get(key)? {
        Value::Array(a) => Some(a.clone()),
        Value::String(s) => match serde_json::from_str(s) {
            Ok(Value::Array(a)) => Some(a),
            _ => None,
        },
        _ => None,
    }
}

/// The text of a content value: a string, or an array of parts whose text parts are joined with newlines.
pub(crate) fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p {
                Value::String(s) => Some(s.as_str()),
                Value::Object(_) => p.get("text").and_then(Value::as_str),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(crate) fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The first of `keys` holding a non-empty string or a number, else `row-<index>`.
pub(crate) fn row_id(row: &Value, keys: &[&str], index: usize) -> String {
    for key in keys {
        match row.get(*key) {
            Some(Value::String(s)) if !s.is_empty() => return s.clone(),
            Some(Value::Number(n)) => return n.to_string(),
            _ => {}
        }
    }
    format!("row-{index}")
}

/// Copies the listed row fields (when present and not null) into `meta`.
pub(crate) fn copy_meta(row: &Value, keys: &[&str], meta: &mut Map<String, Value>) {
    for key in keys {
        if let Some(v) = row.get(*key).filter(|v| !v.is_null()) {
            meta.insert((*key).to_owned(), v.clone());
        }
    }
}

/// One call an assistant message makes.
pub(crate) struct CallSpec {
    pub id: Option<String>,
    pub name: String,
    pub arguments: String,
}

/// Accumulates the messages of a conversation and pairs tool results with the calls they answer.
#[derive(Default)]
pub(crate) struct Builder {
    pub messages: Vec<Message>,
    calls: usize,
    pending: VecDeque<String>,
}

impl Builder {
    /// Adds a message with text; empty text adds nothing.
    pub fn text(&mut self, role: Role, text: &str) {
        if !text.trim().is_empty() {
            self.messages.push(Message::text(role, text));
        }
    }

    /// Adds an assistant message. Nothing is added without text and without calls.
    pub fn assistant(&mut self, text: &str, reasoning: Option<String>, calls: Vec<CallSpec>) {
        if text.trim().is_empty() && calls.is_empty() {
            return;
        }
        let mut message = Message::text(Role::Assistant, text);
        if text.trim().is_empty() {
            message.content.clear();
        }
        message.reasoning = reasoning.filter(|r| !r.trim().is_empty());
        for call in calls {
            self.calls += 1;
            let id = call.id.filter(|i| !i.is_empty()).unwrap_or_else(|| format!("call_{}", self.calls));
            self.pending.push_back(id.clone());
            message.tool_calls.push(ToolCall {
                id,
                name: call.name,
                arguments: call.arguments,
            });
        }
        self.messages.push(message);
    }

    /// Adds a tool result answering call `id`, or the oldest unanswered call when `id` is `None`. A result with no
    /// call to answer is dropped (`false`).
    pub fn tool(&mut self, id: Option<&str>, text: &str) -> bool {
        let id = match id {
            Some(id) => match self.pending.iter().position(|p| p == id) {
                Some(at) => self.pending.remove(at),
                None => None,
            },
            None => self.pending.pop_front(),
        };
        let Some(id) = id else { return false };
        let mut message = Message::text(Role::Tool, text);
        message.tool_call_id = Some(id);
        self.messages.push(message);
        true
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The conversation; `Err("no assistant turn")` when no assistant message came out.
    pub fn finish(
        self,
        id: String,
        model: Option<String>,
        tools: Vec<ToolDef>,
        tool_map: ToolMap,
        meta: Map<String, Value>,
    ) -> Result<Conversation, String> {
        if !self.messages.iter().any(|m| m.role == Role::Assistant) {
            return Err("no assistant turn".to_owned());
        }
        Ok(Conversation {
            id,
            model,
            tools,
            messages: self.messages,
            tool_map,
            meta,
        })
    }
}

/// Arguments as the string of a call: objects are serialized compactly, strings kept verbatim, null is `{}`.
pub(crate) fn arguments_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "{}".to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Naming and defaults for the documents built from conversations.
#[derive(Clone, Debug)]
pub struct BuildOptions {
    /// The cassette's name, or the prefix of trajectory names (`<prefix>-<conversation id>`, made unique and free
    /// of `/`).
    pub name: String,
    /// Dataset or file the rows came from, for `meta.source` (e.g. `hf:allenai/WildChat-1M`).
    pub source: String,
    /// `meta.model` of exchanges whose conversation names none.
    pub model: String,
}

/// One cassette with an exchange per assistant message of every conversation: the request holds the messages before
/// it (and the conversation's tools as name-only `ToolDef`s unless it declares them), the outcome the assistant
/// message as a reply (`finish: tool_calls` when it calls tools). Cassette `meta` records `source` and the row count;
/// exchange `meta` the row id.
pub fn to_cassette(conversations: &[Conversation], opts: &BuildOptions) -> Cassette {
    let mut cassette = Cassette::new(opts.name.clone());
    cassette.meta.insert("source".into(), Value::String(opts.source.clone()));
    cassette.meta.insert("rows".into(), Value::from(conversations.len()));
    for conversation in conversations {
        let model = conversation.model.clone().unwrap_or_else(|| opts.model.clone());
        let tools = request_tools(conversation);
        for (at, message) in conversation.messages.iter().enumerate() {
            if message.role != Role::Assistant {
                continue;
            }
            let mut request = Request::new(model.clone(), conversation.messages[..at].to_vec());
            request.tools = tools.clone();
            let text = message.text_content();
            let reply = Reply {
                text: (!text.is_empty()).then_some(text),
                tool_calls: message.tool_calls.clone(),
                reasoning: message.reasoning.clone(),
                finish: if message.tool_calls.is_empty() {
                    FinishReason::Stop
                } else {
                    FinishReason::ToolCalls
                },
                ..Reply::default()
            };
            let mut extra = Map::new();
            extra.insert("row".into(), Value::String(conversation.id.clone()));
            cassette.exchanges.push(Exchange {
                request,
                outcome: CassetteOutcome::Reply { reply },
                protocol: Protocol::OpenaiChat,
                meta: ExchangeMeta {
                    model: model.clone(),
                    extra,
                    ..ExchangeMeta::default()
                },
                ignore: Vec::new(),
            });
        }
    }
    cassette
}

/// The declared tools, else one name-only definition per tool name the conversation calls.
fn request_tools(conversation: &Conversation) -> Vec<ToolDef> {
    if !conversation.tools.is_empty() {
        return conversation.tools.clone();
    }
    let mut names: Vec<&str> = Vec::new();
    for call in conversation.messages.iter().flat_map(|m| &m.tool_calls) {
        if !names.contains(&call.name.as_str()) {
            names.push(&call.name);
        }
    }
    names
        .into_iter()
        .map(|name| ToolDef {
            name: name.to_owned(),
            description: None,
            parameters: None,
            strict: None,
        })
        .collect()
}

/// One trajectory per conversation with at least one assistant message: a step per assistant message (text,
/// tool calls with their ids, no `expect`), the conversation's `tool_map`, `meta` from the row plus `source`.
pub fn to_trajectories(conversations: &[Conversation], opts: &BuildOptions) -> Vec<Trajectory> {
    let mut used: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for conversation in conversations {
        let steps: Vec<Step> = conversation
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .map(|m| {
                let text = m.text_content();
                let calls: Vec<ToolCallSpec> = m
                    .tool_calls
                    .iter()
                    .map(|c| ToolCallSpec {
                        name: c.name.clone(),
                        arguments: serde_json::from_str(&c.arguments)
                            .ok()
                            .filter(Value::is_object)
                            .unwrap_or_else(|| Value::String(c.arguments.clone())),
                        id: Some(c.id.clone()),
                    })
                    .collect();
                Step {
                    reply: ReplySpec {
                        text: (!text.is_empty()).then_some(text),
                        tool_calls: (!calls.is_empty()).then_some(calls),
                        reasoning: m.reasoning.clone(),
                        ..ReplySpec::default()
                    },
                    expect: None,
                }
            })
            .collect();
        if steps.is_empty() {
            continue;
        }
        let base = format!("{}-{}", opts.name, conversation.id.replace('/', "-"));
        let mut name = base.clone();
        let mut n = 1;
        while !used.insert(name.clone()) {
            n += 1;
            name = format!("{base}-{n}");
        }
        let mut meta = Map::new();
        meta.insert("source".into(), Value::String(opts.source.clone()));
        meta.insert("id".into(), Value::String(conversation.id.clone()));
        for (k, v) in &conversation.meta {
            meta.insert(k.clone(), v.clone());
        }
        out.push(Trajectory {
            kind: Default::default(),
            name,
            steps,
            on_divergence: Default::default(),
            stop_text: None,
            cursor: Default::default(),
            dedupe: true,
            tool_map: conversation.tool_map.clone(),
            unknown_tool: Default::default(),
            check_pairing: false,
            meta,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<Value> {
        let text = std::fs::read_to_string(format!("{}/tests/fixtures/hf/{name}.jsonl", env!("CARGO_MANIFEST_DIR"))).unwrap();
        parse_rows(&text).unwrap()
    }

    fn exchanges(c: &Converted) -> usize {
        to_cassette(
            &c.conversations,
            &BuildOptions {
                name: "t".into(),
                source: "s".into(),
                model: "m".into(),
            },
        )
        .exchanges
        .len()
    }

    #[test]
    fn every_fixture_converts_with_its_totals() {
        for (file, format, converted, exch) in [
            ("sharegpt", Format::ShareGpt, 2, 3),
            ("wildchat", Format::WildChat, 2, 3),
            ("hermes", Format::HermesFc, 2, 4),
            ("swe_agent", Format::SweAgent, 2, 3),
            ("openhands", Format::OpenHands, 2, 3),
        ] {
            for f in [format, Format::Auto] {
                let c = convert_rows(&fixture(file), f);
                assert_eq!(c.conversations.len(), converted, "{file} {f:?}");
                assert_eq!(c.reasons.get("no assistant turn"), Some(&1), "{file} {f:?}");
                assert_eq!(exchanges(&c), exch, "{file} {f:?}");
            }
        }
    }

    #[test]
    fn hermes_pairs_calls_and_keeps_reasoning() {
        let c = convert_rows(&fixture("hermes"), Format::Auto);
        let first = &c.conversations[0];
        assert_eq!(first.messages[0].text_content(), "You are a function calling AI.");
        let call = &first.messages[2].tool_calls[0];
        assert_eq!(
            (call.id.as_str(), call.name.as_str(), call.arguments.as_str()),
            ("call_1", "get_weather", r#"{"city":"Paris"}"#)
        );
        assert_eq!(first.messages[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(first.messages[3].text_content(), r#"{"temp": 18}"#);
        assert_eq!(first.tools.len(), 1);
        assert_eq!(first.meta["category"], "Weather");
        let second = &c.conversations[1];
        assert_eq!(second.messages[2].reasoning.as_deref(), Some("plan"));
        assert_eq!(second.messages[2].tool_calls[0].arguments, r#"{"city":"Rome"}"#);
    }

    #[test]
    fn swe_agent_shapes() {
        let c = convert_rows(&fixture("swe_agent"), Format::Auto);
        let nebius = &c.conversations[0];
        assert_eq!(nebius.messages[2].text_content(), "Let me look.");
        assert_eq!(nebius.messages[2].tool_calls[0].arguments, r#"{"command":"ls"}"#);
        assert_eq!(nebius.messages[3].role, Role::Tool);
        assert_eq!(nebius.messages[4].tool_calls[0].id, "call_2");
        assert_eq!(nebius.tool_map, swe_agent::default_tool_map());
        let smith = &c.conversations[1];
        assert_eq!(smith.messages[1].text_content(), "Fix it");
        assert_eq!(smith.messages[2].tool_calls[0].id, "c1");
        assert_eq!(smith.messages[3].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn openhands_thinks_into_reasoning() {
        let c = convert_rows(&fixture("openhands"), Format::OpenHands);
        let first = &c.conversations[0];
        assert_eq!(first.messages[1].tool_calls[0].name, "execute_bash");
        assert_eq!(first.messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(first.messages[3].text_content(), "Done");
        assert_eq!(c.conversations[1].messages[1].reasoning.as_deref(), Some("I should run date"));
    }

    #[test]
    fn outputs_name_and_shape() {
        let c = convert_rows(&fixture("hermes"), Format::Auto);
        let opts = BuildOptions {
            name: "h".into(),
            source: "hf:x/y".into(),
            model: "m".into(),
        };
        let cassette = to_cassette(&c.conversations, &opts);
        assert_eq!(cassette.meta["rows"], 2);
        assert_eq!(cassette.exchanges[0].request.tools[0].name, "get_weather");
        assert_eq!(cassette.exchanges[0].meta.extra["row"], "h1");
        let CassetteOutcome::Reply { reply } = &cassette.exchanges[0].outcome else {
            panic!("reply")
        };
        assert_eq!(reply.finish, FinishReason::ToolCalls);
        let t = to_trajectories(&c.conversations, &opts);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].name, "h-h1");
        t[0].validate().unwrap();
        let same = to_trajectories(&[c.conversations[0].clone(), c.conversations[0].clone()], &opts);
        assert_eq!(same[1].name, "h-h1-2");
    }

    #[test]
    fn unknown_rows_and_parse_shapes() {
        let c = convert_rows(&[serde_json::json!({"x": 1})], Format::Auto);
        assert_eq!(c.reasons["unrecognized row shape"], 1);
        assert_eq!(
            parse_rows(r#"{"rows":[{"row_idx":0,"row":{"a":1}}]}"#).unwrap(),
            vec![serde_json::json!({"a": 1})]
        );
        assert_eq!(parse_rows("{\"a\":1}\n{\"a\":2}\n").unwrap().len(), 2);
        assert!(parse_rows("{\"a\":1}\nnot json").is_err());
    }
}
