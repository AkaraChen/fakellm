//! The provider-neutral representation of one model round trip.
//!
//! Every protocol frontend (OpenAI Chat Completions, OpenAI Responses, Anthropic Messages)
//! parses its request into [`Request`] and renders an [`Outcome`] back into its own wire format,
//! so scripts, the request log, cassettes and the fake agent all work on one shape. Nothing in this
//! module knows about HTTP or JSON layouts of any particular API.
//!
//! Tool call arguments stay a raw string end to end: clients are tested on how they reassemble
//! streamed fragments, including fragments that never add up to valid JSON.

pub mod schema;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Which API the request came in on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// `POST /v1/chat/completions`.
    #[default]
    #[serde(rename = "openai.chat")]
    OpenaiChat,
    /// `POST /v1/responses`.
    #[serde(rename = "openai.responses")]
    OpenaiResponses,
    /// `POST /v1/messages`.
    #[serde(rename = "anthropic.messages")]
    AnthropicMessages,
    /// `POST /v1/embeddings`. The request is logged with one user message per input element;
    /// nothing is generated, so scripts only ever contribute errors.
    #[serde(rename = "openai.embeddings")]
    OpenaiEmbeddings,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai.chat",
            Self::OpenaiResponses => "openai.responses",
            Self::AnthropicMessages => "anthropic.messages",
            Self::OpenaiEmbeddings => "openai.embeddings",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    /// OpenAI's newer name for the system role; kept distinct so the log shows what was sent.
    Developer,
    User,
    Assistant,
    Tool,
}

/// One piece of message content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
    },
    /// `image_url` parts; `url` may be a data URL. The bytes are not decoded.
    Image {
        url: String,
        detail: Option<String>,
    },
    /// `input_audio` parts; `data` is kept base64-encoded as sent.
    Audio {
        data: String,
        format: Option<String>,
    },
    File {
        file_id: Option<String>,
        filename: Option<String>,
        file_data: Option<String>,
    },
    Refusal {
        text: String,
    },
    /// Anthropic `thinking` block replayed by the client; `signature` is echoed as sent.
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Anthropic `redacted_thinking` block; `data` is opaque.
    RedactedThinking {
        data: String,
    },
    /// Anything not recognized, kept verbatim so the log never loses information.
    Other {
        value: Value,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The JSON arguments exactly as a string, valid or not.
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub content: Vec<Part>,
    /// Assistant messages only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Tool messages only: the call this message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Reasoning text replayed by the client (`reasoning_content`, Anthropic thinking text,
    /// Responses reasoning summary text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// The opaque blob that goes with `reasoning`: Anthropic's thinking `signature`, Responses'
    /// reasoning `encrypted_content`. Clients replay it; the server never inspects it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_opaque: Option<String>,
    /// Tool messages only: the client flagged the result as a failure (Anthropic `is_error`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

impl Message {
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            name: None,
            content: vec![Part::Text { text: text.into() }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning: None,
            reasoning_opaque: None,
            is_error: false,
        }
    }

    /// All text parts joined with newlines; empty when there is no text.
    pub fn text_content(&self) -> String {
        let mut out = String::new();
        for p in &self.content {
            if let Part::Text { text } = p {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema of the arguments, as sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    /// A specific function the client demands.
    Tool {
        name: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema {
        name: Option<String>,
        schema: Option<Value>,
        strict: Option<bool>,
    },
}

/// Anthropic's `thinking` request field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingConfig {
    /// `enabled`, `adaptive` or `disabled`, as sent.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

/// A parsed request. Fields the frontend understands are typed; everything else lands in
/// `extra` so the log can still show it and scripts can still match on it later.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Defaults to Chat so hand-written cassette exchanges can leave it out (the exchange has its own `protocol`).
    #[serde(default)]
    pub protocol: Protocol,
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub stream: bool,
    /// `stream_options.include_usage` (Chat Completions). Meaningless when `stream` is false.
    #[serde(default)]
    pub include_usage: bool,
    /// `max_completion_tokens`, falling back to `max_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Number of choices requested; 1 unless the client asked for more.
    #[serde(default = "one")]
    pub n: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Anthropic `thinking` / extended-thinking settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// Responses `previous_response_id`, as sent. The stored conversation has already been
    /// prepended to `messages` by the time the request is parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
    /// Top-level fields the frontend did not interpret, verbatim.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

fn one() -> u32 {
    1
}

/// `text` without its `<system-reminder>...</system-reminder>` blocks (non-greedy, across lines), trimmed.
/// An opening tag with no closing tag is left as it is.
pub fn strip_system_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    if !text.contains(OPEN) {
        return text.trim().to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let Some(len) = rest[start + OPEN.len()..].find(CLOSE) else { break };
        out.push_str(&rest[..start]);
        rest = &rest[start + OPEN.len() + len + CLOSE.len()..];
    }
    out.push_str(rest);
    out.trim().to_owned()
}

impl Request {
    /// A request with just a model and messages, for tests and seeds.
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            protocol: Protocol::OpenaiChat,
            model: model.into(),
            messages,
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: None,
            stream: false,
            include_usage: false,
            max_tokens: None,
            n: 1,
            temperature: None,
            top_p: None,
            seed: None,
            stop: Vec::new(),
            response_format: None,
            reasoning_effort: None,
            thinking: None,
            previous_response_id: None,
            store: None,
            user: None,
            metadata: Map::new(),
            extra: Map::new(),
        }
    }

    /// Text of all system and developer messages, joined with newlines.
    pub fn system_text(&self) -> String {
        let mut out = String::new();
        for m in &self.messages {
            if matches!(m.role, Role::System | Role::Developer) {
                let t = m.text_content();
                if !t.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&t);
                }
            }
        }
        out
    }

    pub fn last_message(&self) -> Option<&Message> {
        self.messages.last()
    }

    /// Text of the most recent user message, exactly as sent. This is what `Match.last_user_raw`
    /// matches; [`Request::last_user_text`] is the one for ordinary use.
    pub fn last_user_text_raw(&self) -> Option<String> {
        self.messages.iter().rev().find(|m| m.role == Role::User).map(Message::text_content)
    }

    /// Text of the most recent user message that still has text once `<system-reminder>...</system-reminder>`
    /// blocks are removed (Claude Code wraps environment notes and skill lists in them and may send a
    /// user message that is nothing but reminders). The IR keeps the raw text; only this view strips.
    /// The stripped text is trimmed. `Match.last_user` matches this.
    pub fn last_user_text(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .filter(|m| m.role == Role::User)
            .map(|m| strip_system_reminders(&m.text_content()))
            .find(|t| !t.is_empty())
    }

    /// Text of the most recent user message, only if nothing followed it (the usual shape of a
    /// fresh turn). `None` when the last message is a tool result or an assistant message.
    pub fn last_message_user_text(&self) -> Option<String> {
        self.last_message()
            .filter(|m| m.role == Role::User)
            .map(|m| strip_system_reminders(&m.text_content()))
            .filter(|t| !t.is_empty())
    }

    pub fn tool_names(&self) -> impl Iterator<Item = &str> {
        self.tools.iter().map(|t| t.name.as_str())
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tool_names().any(|n| n == name)
    }

    /// The tool results of the current turn: every tool message after the last assistant
    /// message, each with the name of the function it answers, resolved through the assistant
    /// message that issued the call. Empty when no tool message follows the last assistant message.
    ///
    /// Tool messages need not be the very last messages: Anthropic clients send user text
    /// (system reminders) in the same user message as the tool results, which the Anthropic
    /// frontend splits into tool messages followed by a user message.
    pub fn trailing_tool_results(&self) -> Vec<ToolResult<'_>> {
        let start = self.messages.iter().rposition(|m| m.role == Role::Assistant).map_or(0, |i| i + 1);
        self.messages[start..]
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| {
                let name = m
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| self.tool_call_by_id(id))
                    .map(|c| c.name.as_str())
                    .or(m.name.as_deref());
                ToolResult {
                    tool_call_id: m.tool_call_id.as_deref(),
                    name,
                    content: m.text_content(),
                    parts: &m.content,
                    is_error: m.is_error,
                }
            })
            .collect()
    }

    /// Whether the current turn (see [`Request::trailing_tool_results`]) contains tool results.
    pub fn ends_with_tool_result(&self) -> bool {
        let start = self.messages.iter().rposition(|m| m.role == Role::Assistant).map_or(0, |i| i + 1);
        self.messages[start..].iter().any(|m| m.role == Role::Tool)
    }

    /// Finds the assistant tool call with this id anywhere in the conversation.
    pub fn tool_call_by_id(&self, id: &str) -> Option<&ToolCall> {
        self.messages.iter().rev().flat_map(|m| m.tool_calls.iter()).find(|c| c.id == id)
    }

    /// Number of assistant turns so far; `0` on the first request of a conversation.
    pub fn assistant_turns(&self) -> usize {
        self.messages.iter().filter(|m| m.role == Role::Assistant).count()
    }
}

/// A tool result at the tail of a request, see [`Request::trailing_tool_results`].
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult<'a> {
    pub tool_call_id: Option<&'a str>,
    /// Unknown when the client never sent the matching assistant tool call.
    pub name: Option<&'a str>,
    /// Text parts joined with newlines, see [`Message::text_content`].
    pub content: String,
    /// All parts of the result (Anthropic `tool_result` content arrays may mix text and images).
    pub parts: &'a [Part],
    pub is_error: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    #[default]
    Stop,
    /// Cut off by `max_tokens`.
    Length,
    ToolCalls,
    ContentFilter,
}

impl FinishReason {
    /// The Chat Completions spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::ContentFilter => "content_filter",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Counted inside `completion_tokens`.
    #[serde(default)]
    pub reasoning_tokens: u32,
    /// Counted inside `prompt_tokens`.
    #[serde(default)]
    pub cached_tokens: u32,
    /// Tokens written to the prompt cache, counted inside `prompt_tokens` (Anthropic
    /// `cache_creation_input_tokens`). Only scripts and cassettes ever make this non-zero.
    #[serde(default)]
    pub cache_creation_tokens: u32,
}

impl Usage {
    pub fn total_tokens(self) -> u32 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// What the model "said". Rendered by a frontend into a sync body or a stream.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    /// Assistant text. `None` renders as `content: null` (a pure tool-call turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Reasoning text, for protocols that stream it (`reasoning_content`, thinking blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// What goes in Anthropic's thinking `signature` / Responses' reasoning `encrypted_content`.
    /// `None` lets the frontend derive one from the request seed (`fallback::reasoning_opaque`)
    /// whenever `reasoning` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_opaque: Option<String>,
    pub finish: FinishReason,
    pub usage: Usage,
}

/// How a reply is delivered over time and over the wire. All times are server-clock durations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    /// Delay before the first byte of the body: the role chunk and first token when streaming,
    /// the whole body when not. `0` means "as fast as possible".
    pub ttft: Duration,
    /// Gap between consecutive content chunks when streaming. Ignored for sync responses, where
    /// the body is sent after `ttft + token_interval * (tokens - 1)` to match the real latency.
    pub token_interval: Duration,
    /// Tokens per content chunk; 1 mirrors the real API.
    pub chunk_tokens: usize,
    /// Bytes of tool call arguments per streamed fragment.
    pub args_chunk_bytes: usize,
    /// Close the connection after this many content chunks without a finish chunk or `[DONE]`,
    /// to test stream-recovery code. `None` delivers everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_after_chunks: Option<usize>,
    /// Like `cut_after_chunks`, but the connection dies abruptly (no terminating chunk).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_after_chunks: Option<usize>,
    /// Abort the connection mid-body: sync responses send headers and half the body; streams reset at
    /// the first non-head frame (like `reset_after_chunks: 0`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset: bool,
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            ttft: Duration::ZERO,
            token_interval: Duration::ZERO,
            chunk_tokens: 1,
            args_chunk_bytes: 16,
            cut_after_chunks: None,
            reset_after_chunks: None,
            reset: false,
        }
    }
}

/// An API error, provider-neutral. Each frontend renders it in its own body format; the status
/// and `retry_after` go on the HTTP response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    pub status: u16,
    /// OpenAI's `error.type` / Anthropic's `error.type`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    /// Sent as a `retry-after` header (whole seconds, rounded up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<Duration>,
    /// Extra response headers (`retry-after-ms`, `x-should-retry`, `x-codex-active-limit`, ...).
    /// Names are lower-cased by the frontend when rendering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// For streaming requests: deliver the error inside the event stream (HTTP 200) instead of
    /// as an HTTP error. Ignored for sync requests. Shapes: Chat sends one `{"error":{...}}`
    /// frame then `[DONE]`; Anthropic sends `message_start` then an `error` event; Responses
    /// sends `response.created` then `response.failed`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_stream: bool,
}

impl ApiError {
    pub fn new(status: u16, kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            kind: kind.into(),
            code: None,
            message: message.into(),
            param: None,
            retry_after: None,
            headers: Vec::new(),
            in_stream: false,
        }
    }

    pub fn code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn param(mut self, param: impl Into<String>) -> Self {
        self.param = Some(param.into());
        self
    }

    pub fn retry_after(mut self, d: Duration) -> Self {
        self.retry_after = Some(d);
        self
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn in_stream(mut self) -> Self {
        self.in_stream = true;
        self
    }

    // Constructors for the errors clients are tested against. Messages follow the real API's
    // wording where it is public, because some clients (opencode's compaction trigger, for one)
    // match on the text.

    /// 400 `invalid_request_error`.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request_error", message)
    }

    /// 400 `missing_required_parameter`.
    pub fn missing_parameter(param: &str) -> Self {
        Self::invalid_request(format!("Missing required parameter: '{param}'."))
            .code("missing_required_parameter")
            .param(param)
    }

    /// 400 `context_length_exceeded`, with OpenAI's wording.
    pub fn context_length_exceeded(limit: u32, requested: u32) -> Self {
        Self::invalid_request(format!(
            "This model's maximum context length is {limit} tokens. However, your messages resulted in {requested} tokens. \
             Please reduce the length of the messages."
        ))
        .code("context_length_exceeded")
        .param("messages")
    }

    /// 401 `invalid_api_key`.
    pub fn invalid_api_key() -> Self {
        Self::invalid_request("Incorrect API key provided. You can find your API key at https://platform.openai.com/account/api-keys.")
            .with_status(401)
            .code("invalid_api_key")
    }

    /// 404 `model_not_found`.
    pub fn model_not_found(model: &str) -> Self {
        Self::invalid_request(format!("The model `{model}` does not exist or you do not have access to it."))
            .with_status(404)
            .code("model_not_found")
    }

    /// 429 `rate_limit_exceeded`.
    pub fn rate_limited() -> Self {
        Self::new(429, "requests", "Rate limit reached for requests. Please try again later.").code("rate_limit_exceeded")
    }

    /// 429 `insufficient_quota`.
    pub fn insufficient_quota() -> Self {
        Self::new(
            429,
            "insufficient_quota",
            "You exceeded your current quota, please check your plan and billing details.",
        )
        .code("insufficient_quota")
    }

    /// 500 `server_error`.
    pub fn server_error() -> Self {
        Self::new(
            500,
            "server_error",
            "The server had an error while processing your request. Sorry about that!",
        )
    }

    /// 503 `server_error` (OpenAI's "engine overloaded").
    pub fn overloaded() -> Self {
        Self::new(503, "server_error", "The engine is currently overloaded, please try again later.")
    }

    /// 400 `fakellm_no_matching_rule`: strict mode and nothing matched. 400 rather than 500 so
    /// SDKs do not retry it and the test fails fast.
    pub fn no_matching_rule(model: &str) -> Self {
        Self::new(
            400,
            "fakellm_error",
            format!("fakellm: no script rule matched this request (model {model:?}) and --strict is on"),
        )
        .code("fakellm_no_matching_rule")
    }

    // The fake agent's errors. All are `fakellm_error` with a `fakellm_*` code; 400 unless noted
    // so SDKs do not retry them and the test fails at the first wrong step.

    /// 400 `fakellm_trajectory_diverged`: a strict trajectory step's `expect` did not hold.
    /// `detail` lists expected against got.
    pub fn trajectory_diverged(trajectory: &str, step: &str, detail: &str) -> Self {
        Self::new(
            400,
            "fakellm_error",
            format!("fakellm: trajectory {trajectory:?} diverged at step {step}: {detail}"),
        )
        .code("fakellm_trajectory_diverged")
    }

    /// 400 `fakellm_trajectory_exhausted`: a strict trajectory was asked for a step past its end.
    pub fn trajectory_exhausted(trajectory: &str, steps: usize, asked: &str) -> Self {
        Self::new(
            400,
            "fakellm_error",
            format!("fakellm: trajectory {trajectory:?} is exhausted: it has {steps} steps and the client asked for {asked}"),
        )
        .code("fakellm_trajectory_exhausted")
    }

    /// 400 `fakellm_unknown_tool`: a step calls a tool the client did not offer.
    pub fn unknown_tool(trajectory: &str, step: usize, tool: &str, offered: &[&str]) -> Self {
        Self::new(
            400,
            "fakellm_error",
            format!(
                "fakellm: trajectory {trajectory:?} step {step} calls tool {tool:?}, which the client did not offer (offered: [{}])",
                offered.join(", ")
            ),
        )
        .code("fakellm_unknown_tool")
    }

    /// 400 `fakellm_trajectory_not_found`: a rule names a trajectory that is not registered.
    pub fn trajectory_not_found(name: &str) -> Self {
        Self::new(400, "fakellm_error", format!("fakellm: no trajectory named {name:?} is registered")).code("fakellm_trajectory_not_found")
    }

    /// 400 `fakellm_policy_not_found`: a rule names a policy that is not registered.
    pub fn policy_not_found(name: &str) -> Self {
        Self::new(400, "fakellm_error", format!("fakellm: no policy named {name:?} is registered")).code("fakellm_policy_not_found")
    }

    /// 400 `fakellm_policy_error`: the policy threw, returned an unusable reply, or answered with garbage.
    pub fn policy_error(policy: &str, message: &str) -> Self {
        Self::new(400, "fakellm_error", format!("fakellm: policy {policy:?} failed: {message}")).code("fakellm_policy_error")
    }

    /// 500 `fakellm_policy_timeout`: no decision within the policy timeout (host clock).
    pub fn policy_timeout(policy: &str, millis: u64) -> Self {
        Self::new(
            500,
            "fakellm_error",
            format!("fakellm: policy {policy:?} did not decide within {millis} ms"),
        )
        .code("fakellm_policy_timeout")
    }

    /// 500 `fakellm_policy_crashed`: the policy process died while deciding; the next request starts a new one.
    pub fn policy_crashed(policy: &str) -> Self {
        Self::new(500, "fakellm_error", format!("fakellm: policy {policy:?} exited while deciding")).code("fakellm_policy_crashed")
    }

    /// 502 `fakellm_upstream_unreachable`: the `--upstream` server could not be reached or did not answer in time.
    pub fn upstream_unreachable(upstream: &str, detail: &str) -> Self {
        Self::new(
            502,
            "fakellm_error",
            format!("fakellm: upstream {upstream} did not answer: {detail}"),
        )
        .code("fakellm_upstream_unreachable")
    }

    /// 508 `fakellm_upstream_loop`: the request was already relayed by this fakellm instance (or has passed through too
    /// many instances), i.e. `--upstream` leads back here. Refused instead of relayed again.
    pub fn upstream_loop(upstream: &str) -> Self {
        Self::new(
            508,
            "fakellm_error",
            format!("fakellm: upstream {upstream} leads back to this fakellm instance (request loop); refusing to relay it again"),
        )
        .code("fakellm_upstream_loop")
    }

    /// 502 `fakellm_upstream_incomplete`: the upstream response ended or broke before its body was complete, so no
    /// outcome could be reassembled (logged only; the client saw the bytes as they came).
    pub fn upstream_incomplete(detail: &str) -> Self {
        Self::new(502, "fakellm_error", format!("fakellm: upstream response incomplete: {detail}")).code("fakellm_upstream_incomplete")
    }

    /// Replaces the status, keeping everything else.
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    /// [`ApiError::for_status`] in the style of `protocol`. Anthropic has its own `type`
    /// vocabulary; Chat and Responses share OpenAI's.
    pub fn for_status_in(protocol: Protocol, status: u16) -> Self {
        if protocol != Protocol::AnthropicMessages {
            return Self::for_status(status);
        }
        let (kind, message) = match status {
            400 => ("invalid_request_error", "Invalid request."),
            401 => ("authentication_error", "invalid x-api-key"),
            403 => (
                "permission_error",
                "Your API key does not have permission to use the specified resource.",
            ),
            404 => ("not_found_error", "Not found."),
            408 => ("timeout_error", "Request timed out."),
            409 => ("api_error", "Conflict."),
            413 => ("request_too_large", "Request exceeds the maximum allowed number of bytes."),
            429 => ("rate_limit_error", "This request would exceed your organization's rate limit."),
            504 => ("timeout_error", "Request timed out."),
            529 => ("overloaded_error", "Overloaded"),
            s if s >= 500 => ("api_error", "Internal server error"),
            _ => ("invalid_request_error", "Request failed."),
        };
        Self::new(status, kind, message)
    }

    /// The default `type`, `code` and `message` the real API uses for a bare status code. Scripts
    /// that only give a status get these.
    pub fn for_status(status: u16) -> Self {
        match status {
            400 => Self::invalid_request("Invalid request."),
            401 => Self::invalid_api_key(),
            403 => Self::new(403, "invalid_request_error", "You do not have access to this resource."),
            404 => Self::model_not_found("unknown"),
            408 => Self::new(408, "timeout", "Request timed out."),
            409 => Self::new(409, "server_error", "Conflict."),
            413 => Self::invalid_request("Request too large.")
                .with_status(413)
                .code("request_too_large"),
            429 => Self::rate_limited(),
            500 => Self::server_error(),
            502 => Self::new(502, "server_error", "Bad gateway."),
            503 => Self::overloaded(),
            529 => Self::new(529, "overloaded_error", "Overloaded"),
            s if s >= 500 => Self::new(s, "server_error", "The server had an error while processing your request."),
            s => Self::new(s, "invalid_request_error", format!("Request failed with status {s}.")),
        }
    }
}

/// Where an outcome came from, for the request log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// A script rule matched.
    Script { index: usize, name: Option<String> },
    /// A script rule matched and its reply came from step `step` (0-based) of a trajectory.
    Trajectory {
        index: usize,
        name: Option<String>,
        trajectory: String,
        step: usize,
    },
    /// A script rule matched and a policy script decided the reply.
    Policy {
        index: usize,
        name: Option<String>,
        policy: String,
    },
    /// A recorded exchange of a cassette answered: `name` is the cassette, `index` the exchange in it.
    Cassette { name: String, index: usize },
    /// The request was relayed to the `--upstream` server; the outcome is what it answered.
    Upstream,
    /// Nothing matched; the deterministic default answered.
    Fallback,
    /// Nothing matched and strict mode is on.
    Strict,
    /// Rejected before scripts ran: bad JSON, missing fields, bad API key.
    Rejected,
}

/// What the server decided to do with a request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Reply {
        reply: Reply,
        delivery: Delivery,
        source: Source,
    },
    /// `delivery.ttft` delays the error response; the other fields are ignored.
    Error {
        error: ApiError,
        delivery: Delivery,
        source: Source,
    },
    /// Hold the request without answering until the server clock is `until` past the arrival
    /// time, then deliver `then` (which is never itself a `Hang`). `until: None` holds forever
    /// (the client's own timeout or disconnect ends it) and `then` is never delivered.
    Hang {
        until: Option<Duration>,
        then: Box<Outcome>,
    },
}

impl Source {
    /// `(index, name)` of the script rule behind this source, for `Script`, `Trajectory` and `Policy`.
    pub fn rule(&self) -> Option<(usize, Option<&str>)> {
        match self {
            Self::Script { index, name } | Self::Trajectory { index, name, .. } | Self::Policy { index, name, .. } => {
                Some((*index, name.as_deref()))
            }
            Self::Cassette { .. } | Self::Upstream | Self::Fallback | Self::Strict | Self::Rejected => None,
        }
    }
}

impl Outcome {
    /// The source of the underlying reply or error; a `Hang` reports its `then`.
    pub fn source(&self) -> &Source {
        match self {
            Self::Reply { source, .. } | Self::Error { source, .. } => source,
            Self::Hang { then, .. } => then.source(),
        }
    }

    /// The reply or error that is (eventually) delivered, looking through `Hang`.
    pub fn inner(&self) -> &Outcome {
        match self {
            Self::Hang { then, .. } => then.inner(),
            other => other,
        }
    }

    pub fn rejected(error: ApiError) -> Self {
        Self::Error {
            error,
            delivery: Delivery::default(),
            source: Source::Rejected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        }
    }

    #[test]
    fn trailing_tool_results_resolve_names() {
        let mut assistant = Message::text(Role::Assistant, "");
        assistant.content.clear();
        assistant.tool_calls = vec![tool_call("c1", "read_file"), tool_call("c2", "bash")];
        let mut r1 = Message::text(Role::Tool, "contents");
        r1.tool_call_id = Some("c1".into());
        let mut r2 = Message::text(Role::Tool, "ok");
        r2.tool_call_id = Some("c2".into());
        let req = Request::new("m", vec![Message::text(Role::User, "hi"), assistant, r1, r2]);
        let results = req.trailing_tool_results();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, Some("read_file"));
        assert_eq!(results[1].name, Some("bash"));
        assert_eq!(results[1].content, "ok");
        assert!(req.ends_with_tool_result());
        assert_eq!(req.last_user_text().as_deref(), Some("hi"));
        assert_eq!(req.last_message_user_text(), None);
        assert_eq!(req.assistant_turns(), 1);
    }

    #[test]
    fn last_user_text_skips_system_reminders() {
        assert_eq!(
            strip_system_reminders("a <system-reminder>x\ny</system-reminder> b<system-reminder>z</system-reminder>"),
            "a  b"
        );
        assert_eq!(strip_system_reminders("a <system-reminder>open"), "a <system-reminder>open");
        let req = Request::new(
            "m",
            vec![
                Message::text(Role::User, "fix the bug"),
                Message::text(Role::Assistant, "ok"),
                Message::text(Role::User, "<system-reminder>\nnote\n</system-reminder>"),
            ],
        );
        assert_eq!(req.last_user_text().as_deref(), Some("fix the bug"));
        assert_eq!(req.last_message_user_text(), None);
        assert_eq!(
            req.last_user_text_raw().as_deref(),
            Some("<system-reminder>\nnote\n</system-reminder>")
        );
        let req = Request::new("m", vec![Message::text(Role::User, "<system-reminder>a</system-reminder>\n hi \n")]);
        assert_eq!(req.last_user_text().as_deref(), Some("hi"));
        assert_eq!(req.last_message_user_text().as_deref(), Some("hi"));
        assert_eq!(
            Request::new("m", vec![Message::text(Role::User, "<system-reminder>a</system-reminder>")]).last_user_text(),
            None
        );
    }

    #[test]
    fn system_text_joins_system_and_developer() {
        let req = Request::new(
            "m",
            vec![
                Message::text(Role::System, "a"),
                Message::text(Role::Developer, "b"),
                Message::text(Role::User, "c"),
            ],
        );
        assert_eq!(req.system_text(), "a\nb");
        assert!(req.trailing_tool_results().is_empty());
    }

    #[test]
    fn error_defaults_by_status() {
        assert_eq!(ApiError::for_status(429).code.as_deref(), Some("rate_limit_exceeded"));
        assert_eq!(ApiError::for_status(529).kind, "overloaded_error");
        assert_eq!(ApiError::for_status(418).status, 418);
        assert_eq!(ApiError::for_status(413).status, 413);
        assert_eq!(ApiError::invalid_api_key().status, 401);
    }

    #[test]
    fn tool_results_survive_trailing_user_text() {
        let mut assistant = Message::text(Role::Assistant, "");
        assistant.content.clear();
        assistant.tool_calls = vec![tool_call("c1", "read_file")];
        let mut result = Message::text(Role::Tool, "contents");
        result.tool_call_id = Some("c1".into());
        result.is_error = true;
        let req = Request::new(
            "m",
            vec![
                Message::text(Role::User, "hi"),
                assistant,
                result,
                Message::text(Role::User, "<system-reminder>"),
            ],
        );
        assert!(req.ends_with_tool_result());
        let results = req.trailing_tool_results();
        assert_eq!(results.len(), 1);
        assert_eq!(
            (results[0].name, results[0].is_error, results[0].parts.len()),
            (Some("read_file"), true, 1)
        );
        let fresh = Request::new("m", vec![Message::text(Role::User, "hi")]);
        assert!(!fresh.ends_with_tool_result());
    }

    #[test]
    fn hang_outcome_looks_through_to_its_inner_outcome() {
        let inner = Outcome::rejected(ApiError::server_error());
        let hang = Outcome::Hang {
            until: Some(Duration::from_secs(5)),
            then: Box::new(inner.clone()),
        };
        assert_eq!(hang.inner(), &inner);
        assert_eq!(hang.source(), &Source::Rejected);
        let json = serde_json::to_value(&hang).unwrap();
        assert_eq!(json["outcome"], "hang");
        assert_eq!(json["then"]["outcome"], "error");
        assert_eq!(serde_json::from_value::<Outcome>(json).unwrap(), hang);
    }

    #[test]
    fn new_protocols_keep_their_wire_names() {
        for (p, name) in [
            (Protocol::OpenaiChat, "openai.chat"),
            (Protocol::OpenaiResponses, "openai.responses"),
            (Protocol::AnthropicMessages, "anthropic.messages"),
        ] {
            assert_eq!(p.as_str(), name);
            assert_eq!(serde_json::to_value(p).unwrap(), name);
        }
        assert_eq!(ApiError::for_status_in(Protocol::AnthropicMessages, 529).kind, "overloaded_error");
        assert_eq!(ApiError::for_status_in(Protocol::AnthropicMessages, 429).kind, "rate_limit_error");
        assert_eq!(
            ApiError::for_status_in(Protocol::OpenaiResponses, 429).code.as_deref(),
            Some("rate_limit_exceeded")
        );
    }

    #[test]
    fn cassette_and_upstream_sources() {
        let c = Source::Cassette {
            name: "weather".into(),
            index: 3,
        };
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            serde_json::json!({"kind": "cassette", "name": "weather", "index": 3})
        );
        assert_eq!(
            serde_json::to_value(Source::Upstream).unwrap(),
            serde_json::json!({"kind": "upstream"})
        );
        assert_eq!((c.rule(), Source::Upstream.rule()), (None, None));
        assert_eq!(ApiError::upstream_unreachable("http://u", "refused").status, 502);
        assert_eq!(
            ApiError::upstream_incomplete("cut").code.as_deref(),
            Some("fakellm_upstream_incomplete")
        );
    }

    #[test]
    fn ir_round_trips_through_json() {
        let req = Request::new("gpt-4o", vec![Message::text(Role::User, "hi")]);
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
        let out = Outcome::rejected(ApiError::invalid_api_key());
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["outcome"], "error");
        assert_eq!(json["source"]["kind"], "rejected");
    }
}
