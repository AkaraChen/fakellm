package fakellm

import (
	"encoding/json"
	"time"
)

// Ptr returns a pointer to v, for the optional fields of Rule, Match and ReplySpec.
func Ptr[T any](v T) *T { return &v }

// Rule is one script rule: which requests it matches and how it answers (docs/scripts.md).
type Rule struct {
	Name  string    `json:"name,omitempty"`
	Match Match     `json:"match"`
	Reply ReplySpec `json:"reply"`
	Times *uint32   `json:"times,omitempty"`
}

// Match holds the conditions of a rule; all that are set must hold. Pointer fields distinguish
// "unset" from false and 0.
type Match struct {
	Model             string   `json:"model,omitempty"`
	LastUser          string   `json:"last_user,omitempty"`
	System            string   `json:"system,omitempty"`
	AnyMessage        string   `json:"any_message,omitempty"`
	HasTool           string   `json:"has_tool,omitempty"`
	NoTools           *bool    `json:"no_tools,omitempty"`
	HasToolResult     *bool    `json:"has_tool_result,omitempty"`
	LastToolResult    string   `json:"last_tool_result,omitempty"`
	ToolResultContain string   `json:"tool_result_contains,omitempty"`
	LastToolCallID    string   `json:"last_tool_call_id,omitempty"`
	Stream            *bool    `json:"stream,omitempty"`
	Turn              *int     `json:"turn,omitempty"`
	Any               *bool    `json:"any,omitempty"`
	Protocol          string   `json:"protocol,omitempty"`
	Probability       *float64 `json:"probability,omitempty"`
	EveryNth          *uint32  `json:"every_nth,omitempty"`
}

// ReplySpec is what a rule answers: text and/or tool calls, or an error, plus pacing and faults.
// Text, Reasoning and ReasoningOpaque are pointers (use Ptr) so that an empty string can be sent.
// The string fields of Match are matched as patterns and omitted when empty; to match the empty
// string, pass the rule as a JSON or YAML document instead of a Rule.
type ReplySpec struct {
	Text            *string        `json:"text,omitempty"`
	ToolCalls       []ToolCallSpec `json:"tool_calls,omitempty"`
	Reasoning       *string        `json:"reasoning,omitempty"`
	Error           *ErrorSpec     `json:"error,omitempty"`
	FinishReason    string         `json:"finish_reason,omitempty"`
	Stream          *StreamSpec    `json:"stream,omitempty"`
	DelayMs         *uint64        `json:"delay_ms,omitempty"`
	Hang            *bool          `json:"hang,omitempty"`
	HangMs          *uint64        `json:"hang_ms,omitempty"`
	ReasoningOpaque *string        `json:"reasoning_opaque,omitempty"`
	Usage           *UsageSpec     `json:"usage,omitempty"`
	Reset           *bool          `json:"reset,omitempty"`
	// Trajectory and Policy delegate the reply to a registered trajectory or policy by name.
	Trajectory string `json:"trajectory,omitempty"`
	Policy     string `json:"policy,omitempty"`
}

// ToolCallSpec is a tool call to emit. Arguments is any JSON value; a string is sent verbatim.
type ToolCallSpec struct {
	Name      string `json:"name"`
	Arguments any    `json:"arguments,omitempty"`
	ID        string `json:"id,omitempty"`
}

// ErrorSpec makes the rule answer with an API error.
type ErrorSpec struct {
	Status     int               `json:"status"`
	Type       string            `json:"type,omitempty"`
	Code       string            `json:"code,omitempty"`
	Message    string            `json:"message,omitempty"`
	Param      string            `json:"param,omitempty"`
	RetryAfter *float64          `json:"retry_after,omitempty"`
	InStream   *bool             `json:"in_stream,omitempty"`
	Headers    map[string]string `json:"headers,omitempty"`
}

// StreamSpec shapes a streamed reply. Durations are server-clock milliseconds.
type StreamSpec struct {
	TtftMs           *uint64  `json:"ttft_ms,omitempty"`
	TokensPerSec     *float64 `json:"tokens_per_sec,omitempty"`
	ChunkTokens      *int     `json:"chunk_tokens,omitempty"`
	CutAfterChunks   *int     `json:"cut_after_chunks,omitempty"`
	ResetAfterChunks *int     `json:"reset_after_chunks,omitempty"`
}

// UsageSpec overrides the token counts the tokenizer would report, for the fields set.
type UsageSpec struct {
	PromptTokens        *uint32 `json:"prompt_tokens,omitempty"`
	CompletionTokens    *uint32 `json:"completion_tokens,omitempty"`
	CachedTokens        *uint32 `json:"cached_tokens,omitempty"`
	ReasoningTokens     *uint32 `json:"reasoning_tokens,omitempty"`
	CacheCreationTokens *uint32 `json:"cache_creation_tokens,omitempty"`
}

// RuleInfo is a rule as listed by the server, with its counters.
type RuleInfo struct {
	Index     int       `json:"index"`
	Name      string    `json:"name,omitempty"`
	Match     Match     `json:"match"`
	Reply     ReplySpec `json:"reply"`
	Times     *uint32   `json:"times,omitempty"`
	Hits      uint64    `json:"hits"`
	Reached   uint64    `json:"reached"`
	Exhausted bool      `json:"exhausted"`
}

// ClockState is the server clock.
type ClockState struct {
	Now     time.Time `json:"now"`
	Frozen  bool      `json:"frozen"`
	Shifted bool      `json:"shifted"`
}

// Status is GET /_fakellm.
type Status struct {
	Version   string     `json:"version"`
	Clock     ClockState `json:"clock"`
	Scripts   Count      `json:"scripts"`
	Responses Count      `json:"responses"`
	Strict    bool       `json:"strict"`
	Log       LogConfig  `json:"request_log"`
}

// Count is a {"count": n} object.
type Count struct {
	Count int `json:"count"`
}

// LogConfig is the request log setting and its occupancy.
type LogConfig struct {
	Enabled  bool   `json:"enabled"`
	Capacity int    `json:"capacity"`
	Stored   int    `json:"stored"`
	Dropped  uint64 `json:"dropped"`
}

// StoredResponse is a saved Responses API object (id and creation time only).
type StoredResponse struct {
	ID        string    `json:"id"`
	CreatedAt time.Time `json:"created_at"`
}

// RequestRecord is one logged API request. Request and Raw are left as JSON.
type RequestRecord struct {
	Seq       uint64          `json:"seq"`
	Time      time.Time       `json:"time"`
	Method    string          `json:"method"`
	Path      string          `json:"path"`
	Status    int             `json:"status"`
	Hung      bool            `json:"hung"`
	LatencyUs int64           `json:"latency_us"`
	Protocol  string          `json:"protocol"`
	Model     string          `json:"model"`
	Stream    bool            `json:"stream"`
	Source    Source          `json:"source"`
	Rule      string          `json:"rule"`
	Request   json.RawMessage `json:"request"`
	Reply     *LoggedReply    `json:"reply"`
	Headers   [][2]string     `json:"headers,omitempty"`
	Raw       json.RawMessage `json:"raw,omitempty"`
	RawText   string          `json:"raw_text,omitempty"`
}

// Source says who answered a request: Kind is script, fallback, strict or rejected.
type Source struct {
	Kind  string `json:"kind"`
	Index int    `json:"index"`
	Name  string `json:"name"`
}

// LoggedReply is the decided reply of a logged request, either content or an error.
type LoggedReply struct {
	Finish     string          `json:"finish"`
	Text       string          `json:"text"`
	Reasoning  string          `json:"reasoning"`
	ToolCalls  []LoggedCall    `json:"tool_calls"`
	Usage      *LoggedUsage    `json:"usage"`
	Status     int             `json:"status"`
	Error      json.RawMessage `json:"error"`
	RetryAfter *float64        `json:"retry_after"`
	InStream   bool            `json:"in_stream"`
	Headers    json.RawMessage `json:"headers"`
	Hang       json.RawMessage `json:"hang"`
}

// LoggedCall is a tool call in a logged reply; Arguments is the JSON text.
type LoggedCall struct {
	ID        string `json:"id"`
	Name      string `json:"name"`
	Arguments string `json:"arguments"`
}

// LoggedUsage is the token usage of a logged reply.
type LoggedUsage struct {
	PromptTokens     int `json:"prompt_tokens"`
	CompletionTokens int `json:"completion_tokens"`
	TotalTokens      int `json:"total_tokens"`
	ReasoningTokens  int `json:"reasoning_tokens"`
	CachedTokens     int `json:"cached_tokens"`
}

// RequestList is the answer of GET /_fakellm/requests.
type RequestList struct {
	Requests []RequestRecord `json:"requests"`
	// Total is the number of matches before Limit was applied.
	Total   int    `json:"total"`
	Dropped uint64 `json:"dropped"`
}
