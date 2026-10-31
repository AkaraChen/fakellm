//! The provider-neutral representation of one model round trip.
//!
//! Every protocol frontend (OpenAI Chat Completions today; Responses and Anthropic Messages later)
//! parses its request into [`Request`] and renders an [`Outcome`] back into its own wire format,
//! so scripts, the request log, cassettes and the fake agent all work on one shape. Nothing in this
//! module knows about HTTP or JSON layouts of any particular API.
//!
//! Tool call arguments stay a raw string end to end: clients are tested on how they reassemble
//! streamed fragments, including fragments that never add up to valid JSON.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Which API the request came in on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// `POST /v1/chat/completions`.
    #[serde(rename = "openai.chat")]
    OpenaiChat,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai.chat",
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
    /// Reasoning text replayed by the client (`reasoning_content` and friends).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
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

/// A parsed request. Fields the frontend understands are typed; everything else lands in
/// `extra` so the log can still show it and scripts can still match on it later.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub protocol: Protocol,
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
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

    /// Text of the most recent user message.
    pub fn last_user_text(&self) -> Option<String> {
        self.messages.iter().rev().find(|m| m.role == Role::User).map(Message::text_content)
    }

    /// Text of the most recent user message, only if nothing followed it (the usual shape of a
    /// fresh turn). `None` when the last message is a tool result or an assistant message.
    pub fn last_message_user_text(&self) -> Option<String> {
        self.last_message().filter(|m| m.role == Role::User).map(Message::text_content)
    }

    pub fn tool_names(&self) -> impl Iterator<Item = &str> {
        self.tools.iter().map(|t| t.name.as_str())
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tool_names().any(|n| n == name)
    }

    /// The tool results at the tail of the conversation (the trailing run of tool messages),
    /// each with the name of the function it answers, resolved through the assistant message that
    /// issued the call. Empty when the last message is not a tool result.
    pub fn trailing_tool_results(&self) -> Vec<ToolResult<'_>> {
        let mut out = Vec::new();
        for m in self.messages.iter().rev() {
            if m.role != Role::Tool {
                break;
            }
            let name = m
                .tool_call_id
                .as_deref()
                .and_then(|id| self.tool_call_by_id(id))
                .map(|c| c.name.as_str())
                .or(m.name.as_deref());
            out.push(ToolResult {
                tool_call_id: m.tool_call_id.as_deref(),
                name,
                content: m.text_content(),
            });
        }
        out.reverse();
        out
    }

    /// Whether the conversation ends with one or more tool results.
    pub fn ends_with_tool_result(&self) -> bool {
        self.last_message().is_some_and(|m| m.role == Role::Tool)
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
    pub content: String,
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
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            ttft: Duration::ZERO,
            token_interval: Duration::ZERO,
            chunk_tokens: 1,
            args_chunk_bytes: 16,
            cut_after_chunks: None,
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

    /// Replaces the status, keeping everything else.
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
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
}

impl Outcome {
    pub fn source(&self) -> &Source {
        match self {
            Self::Reply { source, .. } | Self::Error { source, .. } => source,
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
