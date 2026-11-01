# M1 implementation spec (internal)

> Status: delivered. This is the implementation spec the milestone was built from, kept as history; the user docs (`docs/*.md`, Chinese) and the code are authoritative where they differ.

Audience: the agents implementing M1 in parallel. Read this, `docs/research.md` (binding wire
constraints, especially the codex section) and the stub file you own. Everything marked
"decided" is settled; do not reopen it. Where this spec is silent, follow the nearest existing
behavior of the Chat Completions frontend and `docs/design.md`.

The tree at the start of the Implement phase already contains: the extended IR, all routing, the
generic request pipeline, the Responses store, the Anthropic error body, `render_with` for error
headers, id helpers in `fallback.rs`, the extra script fields (parsed, not yet acted on), a
generalized test helper, and stubs with `todo!()` bodies. `cargo build --all-targets`,
`cargo test`, `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` pass. Keep them
passing.

## 1. Ownership

| Id | Owns (may edit) | Does not touch |
|---|---|---|
| (a) anthropic | `src/anthropic/{mod,messages,stream}.rs` (`error.rs` is done, extend only if needed), `tests/anthropic.rs`, `sdk/node/anthropic.test.mjs`, `sdk/node/package.json` + lockfile (may `npm install -D @anthropic-ai/sdk @ai-sdk/anthropic`) | everything else |
| (b) responses | `src/openai/responses.rs`, `responses_stream.rs`, `responses_store.rs` (already implemented, extend if needed), `tests/responses.rs`, `sdk/node/responses.test.mjs` | |
| (c) sse+script | `src/sse.rs`, `src/openai/stream.rs` (delete after migrating), `src/openai/chat.rs` (`chunks` -> `Vec<Frame>`, `error_frames`), `src/script.rs`, the Chat branch of `complete()` in `src/openai/mod.rs`, hang logging guard in `handle()` (see 4.3), `tests/faults.rs`, `docs/scripts.md` (Chinese) | |
| (d) wire+chores | `src/wire.rs`, `src/main.rs`, `src/ctl.rs`, `src/clock.rs` (`parse_duration`), `src/control.rs`, `src/reqlog.rs`, `src/lib.rs` (`Config` fields, `DEFAULT_MODELS`), `tests/wire.rs`, additions to `tests/control.rs` and `tests/bin.rs`, `docs/control-plane.md` (Chinese) | `src/openai/mod.rs` routing |
| (e) tests | `tests/scripts_http.rs` (new), `tests/chat.rs` additions, `sdk/node/openai.test.mjs`, `sdk/node/ai-sdk.test.mjs`, `sdk/node/helpers.mjs` | `src/` |

Rules for everybody:

- `src/openai/mod.rs` routing, `src/ir.rs` and `Cargo.toml` are frozen. If you need an IR change,
  stop and report it to the coordinator instead of editing. (c) and (d) may edit the regions of
  `openai/mod.rs` named above; nobody else touches it.
- Each stub file starts with `#![allow(dead_code, unused_variables)]`. Delete it when your file
  has no `todo!()` left. `cargo clippy -D warnings` must pass without it.
- Do not edit README.md, TODO.md or `docs/design.md`. A docs pass after the Implement phase folds
  everything in; put the facts that pass needs in your final report.
- Unit tests go in the module's `#[cfg(test)] mod tests`. HTTP tests go in your `tests/*.rs`
  using `tests/common/mod.rs`. Use the virtual clock instead of sleeping.
- Do not add dependencies. Do not commit.
- Style as fakes3: `//!` doc explaining the why, comments only for non-obvious decisions,
  `cargo fmt` (max_width 140). Code and comments in English; user-facing docs in Chinese.

## 2. IR changes (done, `src/ir.rs`)

- `Protocol::{OpenaiResponses = "openai.responses", AnthropicMessages = "anthropic.messages"}`.
  `Protocol::OpenaiChat` keeps `"openai.chat"`.
- `Part::Thinking {text, signature: Option<String>}`, `Part::RedactedThinking {data}`.
- `Message`: `reasoning_opaque: Option<String>`, `is_error: bool` (tool messages; Anthropic
  `is_error`). Tool results with several parts: a tool message simply has several `content` parts.
- `Request`: `previous_response_id: Option<String>`, `thinking: Option<ThinkingConfig {kind,
  budget_tokens}>`. `instructions` is a leading `System` message. `max_tokens` doubles as
  `max_output_tokens`.
- `Usage::cache_creation_tokens` (counted inside `prompt_tokens`, like `cached_tokens`).
- `Reply::reasoning_opaque: Option<String>`.
- `ApiError::{headers: Vec<(String,String)>, in_stream: bool}` with builders `header()`,
  `in_stream()`, `with_message()`; `ApiError::for_status_in(protocol, status)` gives the
  protocol's default `type`/`message` for a bare status (Anthropic table below).
- `Outcome::Hang {until: Option<Duration>, then: Box<Outcome>}`; `Outcome::inner()` looks through
  it, `Outcome::source()` already does.
- Behavior change (decided): `Request::trailing_tool_results()` / `ends_with_tool_result()` now
  mean "tool messages after the last assistant message", not "the very last messages". Claude
  Code sends `tool_result` blocks and trailing text blocks in one user message; the Anthropic
  frontend splits that into `Tool` messages followed by a `User` message, and scripts keyed on
  `has_tool_result` / `last_tool_result` must still match. `ToolResult` gained `parts` and
  `is_error`.
- Serde compatibility: protocol strings, `outcome` tags and existing field names are unchanged;
  new fields are `default` + skipped when empty/false/None.

Fields the response echoes (Responses): see 7.1 "extra mirrors". Fields in `Request::extra` are
"everything the frontend did not capture in typed form, verbatim"; the Responses frontend
additionally mirrors a few typed ones there because the response object echoes them.

## 3. Pipeline (done, `src/openai/mod.rs`)

`handle()` -> `dispatch()` -> `complete()` for the three generation endpoints. Routes (all also
without the `/v1` prefix):

| Route | Style (error body) |
|---|---|
| `POST /v1/chat/completions` | OpenAI |
| `POST /v1/messages`, `POST /v1/messages/count_tokens` | Anthropic |
| `POST /v1/responses`, `GET /v1/responses/{id}`, `DELETE /v1/responses/{id}` | OpenAI |
| `GET /v1/models`, `GET /v1/models/{id}` | Anthropic iff the request has `anthropic-version`, else OpenAI |
| anything else | 404 `Invalid URL (METHOD path)`, Anthropic style iff `anthropic-version` |

Auth: unchanged (`Authorization: Bearer k` or `x-api-key: k`, either accepted on every route).
Failure: OpenAI style `invalid_api_key` 401, Anthropic style 401 `authentication_error`
"invalid x-api-key" (`{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}`).
Every response gets `x-request-id` (`req_<16 hex>` from the request seed mixed with a per-server
counter, so retries differ) and, for Anthropic-style routes, an identical `request-id`.

`complete(api)` in order: read JSON (non-JSON is a 400; Chat keeps the OpenAI hint text,
Messages says "The request body is not valid JSON."); `parse`; `Engine::resolve`; `Hang`; then
error or reply delivery. Details:

- **Hang**: `Outcome::Hang{until, then}` waits for `arrival + until` on the server clock (forever
  for `None`, i.e. until the client disconnects and hyper drops the future), then continues with
  `then`. `trace.outcome` holds the Hang (the log shows what was decided).
- **Errors**: after `delivery.ttft`. If `error.in_stream && req.stream`, the response is a 200
  event stream built from the protocol's `error_frames`, written by `sse::respond`. Otherwise the
  protocol's HTTP body (`openai::error::render` or `anthropic::error::render`), both through
  `openai::error::render_with`, which also emits `retry-after` (whole seconds, rounded up) and
  every `err.headers` entry (invalid names/values skipped).
- **Replies**: streams via `sse::respond(clock, delivery, frames, [x-request-id], sentinel)` with
  sentinel `Some("[DONE]")` for Chat and `None` otherwise. Sync bodies are sent after
  `ttft + token_interval * (completion_tokens - 1)`.
- **Responses store**: before delivery, unless `req.store == Some(false)`,
  `control.responses.insert(responses::to_stored(..))`. A stream that is cut is still stored.
- Ids: `unique = request_seed ^ hash(per-server counter)`; Chat id `chatcmpl-..`, Messages id
  `fallback::message_id(unique)`, Responses id `fallback::responses_id("resp", unique, 0)` and
  `ResponseMeta.seed = unique`.
- `count_tokens`: parse with `require_max_tokens = false`, answer `{"input_tokens": n}`; the IR
  goes in the log, no outcome, no rule consulted or consumed.

### 3.1 Request log

Unchanged shape. `protocol` is now one of the three strings. A request that was parsed has the
IR in `request`; `outcome` holds the decision (a `Hang` is serialized as `{"outcome":"hang",
"until":..,"then":{..}}`; `control::record_json` looks through it with `Outcome::inner`, (d)
adds a `"hang": {"until_ms": n|null}` field to the `reply` object when the outcome is a Hang).
`anthropic-beta` and every other header are visible with `full=true` (`headers`, lower-cased).

## 4. Shared SSE writer (c): `src/sse.rs`

`Frame {kind: FrameKind, event: Option<String>, data: String}` and
`respond(clock, delivery, frames, headers, done_sentinel) -> Response`; stub has the exact
signatures and the timing rules in its docs. Rules (decided):

- Wire: `event: <name>\ndata: <data>\n\n` or, without an event name, `data: <data>\n\n`. The
  sentinel is `data: <sentinel>\n\n`. Compact JSON in `data`.
- `Head`: sent at `start + ttft`, all leading ones at once. `Content`: first one goes out with
  the head, each later one `token_interval * chunk_tokens` after the previous content frame
  (absolute deadlines on the server clock fixed at call time). `Trailer`: immediately after the
  previous frame.
- `cut_after_chunks = Some(n)`: once `n` content frames were written, stop at the next non-`Head`
  frame: nothing more, no sentinel, clean end of the chunked body. If there are fewer than `n`
  content frames, the cut never fires (with exactly `n`, the trailing frames are dropped). Same observable behavior as today's `stream::respond`;
  the existing `openai/stream.rs` unit tests move to `sse.rs` (converted to `Frame`) and must
  pass unchanged in meaning.
- Headers: `content-type: text/event-stream; charset=utf-8`, `cache-control: no-cache`, then the
  given headers; no `content-length`.
- Writing stops when the client disconnects, even while parked on a frozen clock (keep the
  `tx.closed()` select and the Arc-count test).

Migration of Chat (c): `chat::chunks` returns `Vec<Frame>` (`Role` -> `Head`, `Content` ->
`Content`, `Trailer` -> `Trailer`, `event: None`), `complete()`'s Chat stream branch calls
`sse::respond(.., Some("[DONE]"))`, delete `openai/stream.rs` and `ChunkKind`. Chat output bytes
must be identical to before; all `tests/chat.rs` and `sdk/node` tests stay green.

Frame kind assignment for all protocols: every frame before the first token-carrying delta is
`Head`, the deltas are `Content`, everything after the first delta that is not a delta is
`Trailer`. If there is no delta at all everything is `Head`.

### 4.1 Chat in-stream errors (c)

`chat::error_frames(&ApiError) -> Vec<Frame>`: exactly one `Head` frame `data:
{"error":{"message","type","param","code"}}` (the object `openai::error::body` returns; all four
keys present), then the pipeline's `[DONE]` sentinel, after `ttft`. HTTP status 200.

### 4.2 Hang (c)

The pipeline already waits. (c) makes `Engine::resolve` produce `Outcome::Hang` (section 5).

### 4.3 Logging a hung request (c)

Today a request is logged after its response head exists. A request hung forever never produces
one and, when the client disconnects, hyper drops the `handle()` future, so nothing is logged.
Fix in `handle()`: hold a guard whose `Drop` records the request with `status: 0` (meaning "no
response was sent") if the future is dropped before the normal log call; defuse it on the normal
path. Record only when `Control::log.is_enabled()`. Tests: a rule with `hang: true`, client
timeout, then `GET /_fakellm/requests` shows `status: 0` with the IR and the Hang outcome.

## 5. Scripts (c): `src/script.rs`

The new fields already exist and parse (`deny_unknown_fields` stays). (c) implements behavior,
validation and unit tests.

`Match`:

- `protocol`: `req.protocol == p`.
- `probability` (0..=1; validation error "match.probability must be between 0 and 1" otherwise,
  NaN included): evaluated last, after every other condition. Draw =
  `splitmix64(request_seed ^ reached.wrapping_mul(0x9e37_79b9_7f4a_7c15)) as f64 / 2^64`, matches
  if `draw < p`. Deviation from the brief, decided: the counter is the number of earlier
  requests that *reached* this rule (all other conditions held, rule not exhausted), not the hit
  count, because a failed draw does not change the hit count and an identical retry would draw
  the same number forever. Add `reached: u64` to `Compiled` and (serde default) to `RuleInfo`.
- `every_nth` (`>= 1`, validation error otherwise): the request is counted as reached
  (`reached += 1`), matches iff `reached % n == 0`. With `probability` too, both must hold;
  `reached` increments once per request either way. `times` counts hits only.
- `reached` increments exactly when a non-exhausted rule's other conditions held, whether or not
  `probability`/`every_nth` then rejects. Rules without those fields still maintain it.

`ReplySpec`/`ErrorSpec`:

- `error.in_stream`, `error.headers` map onto `ApiError.in_stream` / `ApiError.headers`
  (header names lower-cased, order = `BTreeMap` order; `retry_after` still produces
  `retry-after` only; scripts add `retry-after-ms` through `headers`). `error.headers` is a
  `BTreeMap<String,String>` (deviation from "Map<String,String>"; `serde_json::Map` is not
  generic).
- `ApiError` defaults when `kind` is `None` come from `ApiError::for_status_in(req.protocol,
  status)` (already wired). A `code`/`message`/`param` override applies on top.
- `hang: true` without `hang_ms`: `Outcome::Hang {until: None, then}`. `hang_ms: Some(ms)` (with
  or without `hang`): `until: Some(ms)`. `then` is what the rest of the spec yields (reply or
  error). A spec with neither text, tool calls nor error but a hang is valid (`then` = a reply
  with `text: Some("")`). `validate` must accept hang-only rules.
- `reasoning_opaque` goes to `Reply.reasoning_opaque` (already wired).
- Delivery of the inner outcome is unchanged (`delay_ms`/`stream.*`); it applies after release.
- `Scripts::replace_keep_hits` exists (hits carried over by index); extend it to carry
  `reached` too. (d) calls it for `PUT ?keep_hits=true`.
- Tool call ids: `fallback::tool_call_id(req.protocol, ..)` (`toolu_...` for Anthropic) is wired.
  The forced-tool-call fallback uses it too.
- No `thinking`/`reasoning` is invented by the fallback: scripts supply `reasoning`; a request
  with `thinking.enabled` or `reasoning.effort` set still gets the plain lorem reply.
- Docs: (c) updates `docs/scripts.md` (Chinese) with the new matchers and fields and a fault
  injection section.

## 6. Anthropic Messages (a)

### 6.1 Request parsing, `messages::parse(body, require_max_tokens)`

Errors are `invalid_request_error` 400 with `param: None`; messages follow Anthropic's
`field: problem` wording.

| Input | Result |
|---|---|
| body not an object | "Input should be a valid dictionary or object" |
| `model` missing / not string | "model: Field required" / "model: Input should be a valid string" |
| `messages` missing / empty / not array | "messages: Field required" / "messages: at least one message is required" / "messages: Input should be a valid list" |
| `messages[i].role` not `user`/`assistant`/`system` (`system` is accepted leniently since Claude Code sends it; the message is treated as system) | "messages.{i}.role: Input should be 'user' or 'assistant'" |
| `max_tokens` missing (`require_max_tokens`) | "max_tokens: Field required" |
| `max_tokens` not a positive integer | "max_tokens: Input should be a valid integer" / "max_tokens: Input should be greater than or equal to 1" |
| `thinking.type` not `enabled`/`adaptive`/`disabled` | "thinking.type: Input should be 'enabled', 'adaptive' or 'disabled'" |
| `thinking.type == enabled` and `budget_tokens < 1024` | "thinking.enabled.budget_tokens: Input should be greater than or equal to 1024" |
| `thinking.type == enabled` and `budget_tokens >= max_tokens` (only if `max_tokens` present) | "thinking.enabled.budget_tokens: Input should be less than max_tokens" |
| tool without `name` | "tools.{i}.name: Field required" |
| block without a string `type` | "messages.{i}.content.{j}.type: Field required" |

Mapping to the IR (`protocol = AnthropicMessages`):

- `system`: string or array of `{type:"text", text, cache_control}` blocks -> one leading
  `System` message whose content is one `Text` part per block. `cache_control` is dropped (the raw
  body is in the log).
- `messages[i].content`: string = one `Text` part. Blocks:
  - `text` -> `Text`. `image` (`source` `base64` {media_type, data} or `url`) -> `Image {url:
    "data:<media_type>;base64,<data>" | url, detail: None}`. `document` -> `File {file_id:
    source.file_id, filename: title, file_data: data URL for base64, the URL for `url`, the text for
    `text` sources}`. Everything else (`search_result`, `server_tool_use`, `web_search_tool_result`,
    `mcp_*`, unknown) -> `Other {value}`.
  - `thinking` -> `Part::Thinking {text: thinking, signature}`; `redacted_thinking` ->
    `Part::RedactedThinking {data}`. Both stay in `content`, in order. Additionally the message's
    `reasoning` is the thinking texts joined with `\n` and `reasoning_opaque` the last signature
    (convenience for scripts and the log). `text_content()` ignores them.
  - `tool_use {id, name, input}` (assistant) -> `ToolCall {id, name, arguments: compact JSON of
    input}`; an absent `input` gives `{}`.
  - `tool_result {tool_use_id, content, is_error}` (user) -> a `Tool` message: `tool_call_id`,
    `content` parts from a string (one `Text`) or a block array (same block rules, so text and
    images), `is_error`. Splitting rule (decided): a user message's blocks are cut into maximal
    runs; each run of `tool_result`s becomes `Tool` messages (one per block), each run of other
    blocks becomes one `User` message, order preserved. A typical `[tool_result, text]` message
    therefore becomes `Tool`, `User`.
  - Assistant message: one `Assistant` message with `Text`/thinking parts, `tool_calls`.
- `tools[]`: every entry with a `name` -> `ToolDef {name, description, parameters: input_schema,
  strict: None}`. Server tools (`type: web_search_...`, no `input_schema`) become a `ToolDef`
  with `parameters: None`.
- `tool_choice`: `auto` -> `Auto`, `any` -> `Required`, `tool` -> `Tool{name}`, `none` -> `None`;
  `disable_parallel_tool_use: true` -> `parallel_tool_calls = Some(false)`.
- `max_tokens` -> `Request.max_tokens`; `stop_sequences` -> `stop`; `temperature`, `top_p`;
  `top_k` -> `extra`; `metadata` -> `Request.metadata` and `metadata.user_id` -> `Request.user`;
  `stream`; `thinking` -> `ThinkingConfig {kind, budget_tokens}`.
- Everything else (`service_tier`, `container`, `mcp_servers`, `context_management`,
  `output_config`, ...) -> `extra`. `null` values are dropped like Chat.
- `anthropic-version` is not enforced. `anthropic-beta` is only logged (headers).

### 6.2 Sync body, `messages::render_sync`

```
{"id":"msg_..","type":"message","role":"assistant","model":<request model>,
 "content":[<blocks>],"stop_reason":..,"stop_sequence":null,
 "usage":{"input_tokens":..,"cache_creation_input_tokens":..,"cache_read_input_tokens":..,"output_tokens":..}}
```

- Block order: thinking (when `reply.reasoning` is non-empty: `{"type":"thinking","thinking":text,
  "signature": reply.reasoning_opaque or fallback::reasoning_opaque(seed)}`), text
  (`{"type":"text","text":..}` when `text` is `Some` and either non-empty or there is no other
  block; a `refusal` is appended to / replaces it as text), then one `tool_use` per call
  `{"type":"tool_use","id","name","input": <arguments parsed as JSON; `{}` when they do not parse>}`。流式不做这个回退，原样发 `partial_json`，所以截断的参数在 `stream:false` 得到 `{}`、在 `stream:true` 得到不完整的 JSON；这是有意的，用来测客户端对截断参数的处理。
  `seed` is `fallback::request_seed(req)`.
- `stop_reason`: `Stop` end_turn, `Length` max_tokens, `ToolCalls` tool_use, `ContentFilter`
  refusal. `stop_sequence` is always `null`.
- Usage: `cache_read_input_tokens = cached_tokens`, `cache_creation_input_tokens =
  cache_creation_tokens`, `input_tokens = prompt_tokens - cached - cache_creation` (saturating),
  `output_tokens = completion_tokens`.
- Default errors per status (`ApiError::for_status_in`): 400 invalid_request_error, 401
  authentication_error, 403 permission_error, 404 not_found_error, 413 request_too_large, 429
  rate_limit_error, 504 timeout_error, 529 overloaded_error, other 5xx api_error.

### 6.3 SSE, `stream::frames`

Order, each frame `event: <type>` + `data: {"type":"<type>",..}`:

1. `message_start`: `{"type":"message_start","message":{"id","type":"message","role":"assistant",
   "model","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens": N,
   "cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}`.
   `input_tokens` (non-cached, as in sync) is REQUIRED. Cache fields carry the reply's values.
2. `ping`: `{"type":"ping"}`.
3. Per block, index `i`: `content_block_start` with `content_block` `{"type":"thinking",
   "thinking":"","signature":""}` / `{"type":"text","text":""}` / `{"type":"tool_use","id","name",
   "input":{}}`; deltas; `content_block_stop {"index":i}`.
   - thinking: `thinking_delta {"thinking": piece}` per `chunk_tokens` tokens, then one
     `signature_delta {"signature": opaque}`.
   - text: `text_delta {"text": piece}` per `chunk_tokens` tokens
     (`tokenizer::tokenize(..).chunks(chunk_tokens)` like Chat).
   - tool_use: `input_json_delta {"partial_json": fragment}` per `args_chunk_bytes` bytes
     (char-boundary-safe split like `chat::split_bytes`, which is private and in (c)'s file:
     write your own copy). Empty arguments still send one
     `input_json_delta` with `""`. Fragments may be invalid JSON; pass them through.
4. `message_delta`: `{"type":"message_delta","delta":{"stop_reason":..,"stop_sequence":null},
   "usage":{"output_tokens": n}}` REQUIRED; also include `input_tokens` and the cache fields
   (newer API versions do).
5. `message_stop`: `{"type":"message_stop"}`.

Frame kinds as in section 4: `message_start`, `ping`, first `content_block_start` are `Head`;
deltas (including `signature_delta`) `Content`; later starts, stops, `message_delta`,
`message_stop` `Trailer`.

In-stream error (`error_frames`): `message_start` (input tokens as above), `ping`, then
`event: error` / `data: {"type":"error","error":{"type":<err.kind>,"message":<err.message>}}`,
all `Head`. No `message_stop`.

### 6.4 Other endpoints

- `count_tokens`: `{"input_tokens": tokenizer::count_request(req)}`.
- `GET /v1/models` (with `anthropic-version`): `{"data":[{"type":"model","id","display_name"
  (= id),"created_at":"<RFC 3339 UTC, derived from the id like openai::models::created>"}],
  "has_more":false,"first_id":<first|null>,"last_id":<last|null>}`. `GET /v1/models/{id}`: one
  such object, or 404 `not_found_error` "model: {id}" (already wired in the pipeline).
- Tests (a): `tests/anthropic.rs` covering every wire rule above (sync, stream order, tools,
  thinking round trip with signature echo, max_tokens required, 401 body, error mapping, in-stream
  error, count_tokens, models shape, trailing text after tool_result still matches
  `has_tool_result`), plus `sdk/node/anthropic.test.mjs` with `@anthropic-ai/sdk` (`messages.create`,
  `messages.stream`, tool use, `maxRetries: 0` on a 529) and `@ai-sdk/anthropic` (`generateText`,
  `streamText`; its default base URL ends in `/v1`, so pass `${url}/v1`; the Anthropic SDK takes
  `baseURL: url` without `/v1`).

## 7. OpenAI Responses (b)

### 7.1 Request parsing, `responses::parse(body, store)`

Errors are `invalid_request_error` with `param` and `code` as in Chat: missing `model` ->
`ApiError::missing_parameter("model")`; non-string -> "Invalid type for 'model': expected a
string." code `invalid_type` (reuse the Chat wording); `input` missing/null without
`previous_response_id` -> `missing_parameter("input")`; `input` neither string nor array ->
invalid_type; `max_output_tokens < 16` -> 400 "Invalid 'max_output_tokens': integer below minimum
value. Expected a value >= 16, but got N instead." code `integer_below_min_value`, param
`max_output_tokens`; `previous_response_id` unknown -> `previous_not_found(id)`.

IR mapping (`protocol = OpenaiResponses`):

1. **Message order**: `[System(instructions)]` (if `instructions` is a non-empty string), then the
   stored conversation of `previous_response_id` (cloned), then the new `input`. `input` as a
   string is one `User` message.
2. **Item grouping (decided)**: items are walked in order and assistant-side items are merged
   into a single `Assistant` message so that `Request::assistant_turns()` counts model turns:
   a run of `reasoning`, `message` with `role: assistant`, `function_call`, `custom_tool_call`
   and unknown `*_call` items becomes one `Assistant` message (`content` = text parts of the
   assistant messages in order, `tool_calls` = the calls, `reasoning` = summary texts joined by
   `\n`, `reasoning_opaque` = last `encrypted_content`). Any other item ends the run.
3. `message` items (`type` may be absent when `role` is set): roles `user`, `system`,
   `developer`, `assistant` -> the same IR roles. Content string or parts: `input_text` and
   `output_text` -> `Text`; `input_image {image_url, file_id, detail}` -> `Image {url: image_url or
   "file_id:<id>"}`; `input_file {file_id, filename, file_data, file_url}` -> `File` (`file_url`
   goes into `file_data`); `refusal` -> `Refusal`; anything else -> `Other`.
4. `function_call {call_id, name, arguments}` -> `ToolCall {id: call_id, ..}`;
   `custom_tool_call {call_id, name, input}` -> `ToolCall` with `arguments = input`.
5. `function_call_output {call_id, output}` and `custom_tool_call_output` -> `Tool` message,
   `tool_call_id = call_id`, content from a string output (one `Text`) or an array of
   `input_text`/`input_image` parts.
6. `reasoning {summary:[{type:summary_text,text}], encrypted_content}` -> group member as above.
7. `item_reference {id}` -> its id appended to `extra["item_references"]` (an array); nothing is
   resolved.
8. Unknown item types: `Part::Other {value: item}` inside an `Assistant` group member if the type
   ends with `_call` or is `compaction`/`reasoning`-like (`web_search_call`, `local_shell_call`,
   `tool_search_call`, `compaction`); inside a `Tool` message (tool_call_id = `call_id` if
   present) if it ends with `_output`; otherwise a `User` message. They never fail the request.
9. `tools`: `function` -> `ToolDef {name, description, parameters, strict}`; `custom` (freeform,
   e.g. codex `apply_patch`) -> `ToolDef {name, description, parameters: None}`; `namespace`,
   `tool_search`, `web_search`, `file_search`, `mcp`, ... -> not a `ToolDef`. The raw `tools`
   array is mirrored into `extra["tools"]`. 输出方向：reply 里调用了请求声明为 `custom` 的工具时，输出 `custom_tool_call {id: ctc_..., call_id, name, input}`（流式用 `response.custom_tool_call_input.delta/done`），`input` 是参数字符串，或参数为带 `input` 字符串字段的对象时取该字段；其余为 `function_call`。
10. `tool_choice`: `"auto"|"none"|"required"` and `{type:"function", name}` (also the Chat form
    `{function:{name}}`) -> `ToolChoice`; `allowed_tools` and other objects -> `extra["tool_choice"]`.
11. `max_output_tokens` -> `max_tokens`; `parallel_tool_calls`; `stream`; `temperature`; `top_p`;
    `store` (`Some(b)` only when sent; absent means true for the pipeline and the echo);
    `previous_response_id`; `metadata` (object) -> `Request.metadata`; `user`.
12. `text {format, verbosity}` -> `response_format` from `format` (`text`, `json_object`,
    `json_schema {name, schema, strict}` flattened, as Responses puts the fields next to `type`);
    `reasoning {effort, summary}` -> `reasoning_effort = effort`.
13. **Extra mirrors** (decided): besides unknown fields, these stay in `extra` verbatim even
    though captured above, because the response object echoes them: `instructions`, `tools`,
    `tool_choice` (only when it did not map), `text`, `reasoning`, `include`, `service_tier`,
    `truncation`, `prompt_cache_key`, `safety_identifier`, `client_metadata`. `null` values are
    dropped.
14. `include` containing `reasoning.encrypted_content` is the switch for `encrypted_content` on
    reasoning items (7.2). Read it from `req.extra["include"]`.

### 7.2 Response object and output items

Object (`response_object`), keys in this order: `id`, `object: "response"`, `created_at`,
`status` (`completed`, `incomplete`, `failed`, `in_progress`), `background: false`, `error`
(null, or `{code, message}` when failed), `incomplete_details` (null, or `{reason}`),
`instructions` (extra.instructions or null), `max_output_tokens`, `model`, `output`, `parallel_tool_calls`
(request value, default true), `previous_response_id`, `reasoning` (extra.reasoning or `{"effort":
null,"summary":null}`), `safety_identifier` (extra or null), `service_tier` (extra or "default"),
`store` (request value, default true), `temperature` (request or 1.0), `text` (extra.text or
`{"format":{"type":"text"}}`), `tool_choice` (IR mapped back: "auto" default, "none", "required",
`{"type":"function","name"}`, or extra), `tools` (extra.tools or `[]`), `top_p` (request or 1.0),
`truncation` (extra or "disabled"), `usage` (null while in progress), `user`, `metadata` (`{}`
default), `prompt_cache_key` (extra or null).

Usage: `{"input_tokens", "input_tokens_details": {"cached_tokens"}, "output_tokens",
"output_tokens_details": {"reasoning_tokens"}, "total_tokens"}`. Reasoning tokens count the
reasoning text (the Engine already does).

Status mapping: `Stop`/`ToolCalls` -> completed; `Length` -> incomplete with
`incomplete_details {"reason":"max_output_tokens"}`; `ContentFilter` -> incomplete with reason
`content_filter` (codex retries it). Failed only for in-stream errors.

Output items, in this order, ids from `fallback::responses_id(prefix, meta.seed, output_index)`:

- `reasoning` (when `reply.reasoning` non-empty): `{"id":"rs_..","type":"reasoning",
  "summary":[{"type":"summary_text","text":<reasoning>}]}` plus `"encrypted_content":
  reply.reasoning_opaque or fallback::reasoning_opaque(meta.seed)` when `include` asks for it.
- `message` (when `text` is `Some` and non-empty, or when there is no reasoning and no tool call;
  a `refusal` becomes a `{"type":"refusal","refusal":..}` content part): `{"id":"msg_..",
  "type":"message","status":"completed","role":"assistant","content":[{"type":"output_text",
  "text":..,"annotations":[],"logprobs":[]}]}`. Status is `incomplete` when the response is.
- one `function_call` per tool call: `{"id":"fc_..","type":"function_call","status":"completed",
  "call_id": call.id,"name","arguments"}`.

codex constraint (research.md): `function_call` must carry `call_id`, `name`, `arguments`;
`reasoning` must carry `summary`; `message` must carry `role` and `content`; otherwise codex
silently drops the item.

### 7.3 SSE, `responses_stream::frames`

Every event: `event: <type>` and `data: {"type":"<type>","sequence_number":n,..}`, `n` from 0,
strictly increasing over the whole stream. No `[DONE]`. Sequence:

1. `response.created` and `response.in_progress`: `{"response": <response_object, status
   "in_progress", output [], usage null>}`.
2. Per output item at `output_index`:
   - all kinds open with `response.output_item.added {"output_index","item"}` where `item` is the
     final item with empty payload (`message`: `status: "in_progress"`, `content: []`;
     `function_call`: `status: "in_progress"`, `arguments: ""`; `reasoning`: `summary: []`) and
     closes with `response.output_item.done {"output_index","item": <final item>}`.
   - reasoning: `response.reasoning_summary_part.added {"item_id","output_index",
     "summary_index":0,"part":{"type":"summary_text","text":""}}`,
     `response.reasoning_summary_text.delta {"item_id","output_index","summary_index":0,"delta"}`
     per `chunk_tokens` tokens, `response.reasoning_summary_text.done {..,"text"}`,
     `response.reasoning_summary_part.done {..,"part":{"type":"summary_text","text"}}`.
   - message: `response.content_part.added {"item_id","output_index","content_index":0,"part":
     {"type":"output_text","text":"","annotations":[],"logprobs":[]}}`, `response.output_text.delta
     {"item_id","output_index","content_index":0,"delta","logprobs":[]}` per `chunk_tokens`
     tokens, `response.output_text.done {..,"text","logprobs":[]}`, `response.content_part.done
     {..,"part"}`. For a refusal: `response.refusal.delta` / `response.refusal.done {"refusal"}`
     in place of the text events.
   - function_call: `response.function_call_arguments.delta {"item_id","output_index","delta"}`
     per `args_chunk_bytes` bytes, then `response.function_call_arguments.done {"item_id",
     "output_index","name","arguments"}`. Empty arguments still send one delta with `""`.
3. `response.completed {"response": <final object>}`; for `Length`/`ContentFilter` the event is
   `response.incomplete` with the object's `incomplete_details`.

Frame kinds per section 4 (`response.created`, `response.in_progress`, first `output_item.added`
and any part-added before the first delta are `Head`).

In-stream error (`error_frames`): `response.created`, then `response.failed {"response":
<object, status "failed", output [], error {"code","message"}>}`, all `Head`. `error.code` is
`error_code(err)`: `err.code` when set; else by kind/status: `insufficient_quota` kind ->
`insufficient_quota`; 429 -> `rate_limit_exceeded`; 503 or 529 -> `server_is_overloaded`; other
>= 500 -> `server_error`; 400 `context_length_exceeded` stays as `err.code`; else `err.kind`.
When `err.retry_after` or `err.headers` is set, `response.error` also gets `"headers"`: an object
of the lower-cased header names to string values (`retry-after` in whole seconds first); this is
the best-effort reading of codex's `error.headers`. Codex behavior to keep in mind
(research.md): `context_length_exceeded` and `insufficient_quota` do not retry, unknown codes
retry, `server_is_overloaded` retries only with a retry-after.

### 7.4 Store and endpoints

`ResponseStore` (done): `insert`, `get`, `remove`, `list` (`(id, created_at)` oldest first),
`clear`, `len`. `to_stored` builds `StoredResponse {id, created_at, response: render_sync(..),
conversation}` where `conversation` is `req.messages` without the leading instructions
`System` message (detect it by `extra["instructions"]` being present and the first message being
`System`) followed by the reply as IR messages in the grouping of 7.1 (one `Assistant` message:
text, tool calls, reasoning, `reasoning_opaque`).

- `GET /v1/responses/{id}`: the stored `response` JSON, 200; unknown -> `responses::not_found`
  (404 `invalid_request_error`, "Response with id '<id>' not found.", param null, code null).
  Already wired.
- `DELETE /v1/responses/{id}`: `{"id","object":"response","deleted":true}`; unknown -> 404 as
  above. Already wired.
- `store: false`: not stored; `previous_response_id` naming it -> 404 `invalid_request_error`
  "Previous response with id 'x' not found.", param `previous_response_id`, code
  `previous_response_not_found`.
- A stored conversation is a snapshot; deleting the response does not affect responses chained
  from it.
- Not implemented (404 Invalid URL): `/input_items`, `/cancel`, `background`.
- Tests (b): `tests/responses.rs` covering every rule above (sync, stream event order and
  sequence numbers, reasoning with `include`, tool calls, chaining across 3 turns, store false,
  GET/DELETE, incomplete, in-stream failed, the codex minimal-request shape with a `developer`
  message and `include`/`store:false`/`tool_choice:"auto"`, unknown item types accepted, request
  log shows the resolved messages), plus `sdk/node/responses.test.mjs` with the `openai` SDK
  (`client.responses.create`, `.stream`, `previous_response_id`, `retrieve`, `delete`) and
  `@ai-sdk/openai` `openai('gpt-4o')` (default Responses provider) `generateText`/`streamText`
  including a tool loop.

## 8. Control plane additions (d): `src/control.rs`, `src/ctl.rs`

`Control::responses` and `ResetOptions::responses` (default true, `?responses=false` on
`POST /_fakellm/reset`) exist; (d) wires the rest:

- `GET /_fakellm/responses` -> `{"responses":[{"id","created_at"}],"count":n}`, `created_at` as an
  RFC 3339 string via `fmt_time` (the store holds unix seconds). `DELETE /_fakellm/responses` ->
  `{"deleted": n}` and clears the store. `405` for other methods.
- `GET /_fakellm` status gains `"responses": {"count": n}`.
- `fakellm ctl responses {list, clear}`, `ctl reset --keep-responses`; `ctl status` prints the
  count. `--json` as everywhere.
- `GET /_fakellm/requests` gains `protocol=<openai.chat|openai.responses|anthropic.messages>`
  (`RequestFilter::protocol: Option<Protocol>`); `ctl requests list --protocol`.
- `record_json`: for Anthropic requests the `reply.error` object uses `anthropic::error::body`
  (the wire shape), for others `openai::error::body`; error replies also expose `in_stream` and
  `headers`; Hang adds `hang` (3.1).
- Rust: `Control::response_ids()` -> `Vec<(String, i64)>` and `Control::clear_responses()`;
  `Server` mirrors them.
- Update `docs/control-plane.md` (Chinese).

## 9. `fakellm wire` (d): `src/wire.rs`, `src/main.rs`

`fakellm wire <target> [--endpoint URL] [--format env|json|toml|shell]`; `--endpoint` defaults to
`http://127.0.0.1:4010` and reads `FAKELLM_ENDPOINT`. Default format (decided): `shell` when
`env` is non-empty, else `toml` when a `.toml` file exists, else `json`. With `shell` the
`snippet` (and the contents of any files, each under a `# <file name>` comment line) is printed
after the exports; `env` prints only `KEY=value` lines; `json` prints the whole `WireOutput`.
Unknown target or bad endpoint: message on stderr, exit code 2. `wire` is a
pure function (`wire::wire`), the CLI only formats. Endpoint normalization: trim, strip trailing
`/`, strip a trailing `/v1`, require `http://` or `https://`.

Outputs (`E` = normalized endpoint, no `/v1`):

| target | env | files | snippet |
|---|---|---|---|
| `claude` | `ANTHROPIC_BASE_URL=E`, `ANTHROPIC_API_KEY=fakellm` | none | note: Claude Code prefers `ANTHROPIC_AUTH_TOKEN` (sent as `Bearer`) when set, fakellm accepts both; unset it or set it to `fakellm`; model names like `sonnet` or `glm-5.2[1m]` are accepted |
| `codex` | `FAKELLM_API_KEY=fakellm` | `config.toml` (below) | the `-c` one-liner (below) |
| `opencode` | `OPENCODE_CONFIG_CONTENT=<compact opencode.json>` | `opencode.json` | the `opencode.json` pretty-printed plus a note on the env form |
| `pi` | none | `models.json` | where it goes (`~/.pi/agent/models.json`) |
| `openai-sdk` | none | none | JS and Python snippets |
| `ai-sdk` | none | none | JS snippet (`createOpenAI`, `createAnthropic`) |
| `anthropic-sdk` | none | none | JS and Python snippets |

codex `config.toml` (from research.md):

```toml
model = "gpt-5.5"
model_provider = "fakellm"

[model_providers.fakellm]
name = "fakellm"
base_url = "E/v1"
# env_key = "FAKELLM_API_KEY"   # optional; if set, the env var must be non-empty
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 5000
```

codex one-liner: `codex -c model=gpt-5.5 -c model_provider=fakellm -c 'model_providers.fakellm={ name = "fakellm", base_url = "E/v1", env_key = "FAKELLM_API_KEY", wire_api = "responses", request_max_retries = 0, stream_max_retries = 0, stream_idle_timeout_ms = 5000 }'`
(prefixed by `FAKELLM_API_KEY=fakellm`).

opencode `opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "model": "fakellm/fake-model",
  "provider": {
    "fakellm": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "fakellm",
      "options": { "baseURL": "E/v1", "apiKey": "fakellm" },
      "models": { "fake-model": { "name": "fake-model", "tool_call": true } }
    }
  }
}
```

pi `models.json`: `{"providers":{"fakellm":{"baseUrl":"E/v1","api":"openai-completions",
"apiKey":"fakellm","models":[{"id":"fake-model"}]}}}` (pretty-printed). Snippets: OpenAI SDK
`baseURL: "E/v1"`, Anthropic SDK `baseURL: "E"` (no `/v1`), ai-sdk `createOpenAI({baseURL:
"E/v1"})` and `createAnthropic({baseURL: "E/v1"})`; all with `apiKey: "fakellm"`.

`Format` parsing, `--format toml` for a target without a toml file, and `shell` quoting are in
the stub docs. Tests (d): `tests/wire.rs` (every target, endpoint normalization, errors, TOML
parses back with `serde_norway`-free checks via string asserts since no TOML crate is allowed;
JSON outputs are parsed with serde_json) and `tests/bin.rs` (subprocess `fakellm wire codex`).

## 10. Chores (d), `TODO.md`

All in `main.rs` unless noted. Already done in the contract commit, do not redo: the per-server counter that makes
`x-request-id` and response ids differ between retries (`Control::next_salt`, reset by
`reset` together with the request log).

1. `FAKELLM_STRICT` accepts `1/0/true/false/yes/no/on/off`: clap `BoolishValueParser`
   (`#[arg(long, env, value_parser = BoolishValueParser::new(), num_args = 0..=1,
   default_missing_value = "true", default_value_t = false)]`; `--strict` alone still works).
2. Env vars: `FAKELLM_TTFT`, `FAKELLM_TOKENS_PER_SEC`, `FAKELLM_FALLBACK_TOKENS`,
   `FAKELLM_NO_REQUEST_LOG`, `FAKELLM_REQUEST_LOG_CAPACITY`, and new flags `--chunk-tokens`
   (`FAKELLM_CHUNK_TOKENS`, default 1, must be >= 1) and `--args-chunk-bytes`
   (`FAKELLM_ARGS_CHUNK_BYTES`, default 16, >= 1) flowing into `Config.delivery`
   (`Delivery.chunk_tokens`, `args_chunk_bytes`). Booleans use the boolish parser.
3. `--scripts` read errors name the file: `<path>: <io error>` (`invalid()`-style, not
   `Os { code: 2 .. }`).
4. `clock::parse_duration` accepts a decimal number before a unit (`1.5s`, `0.25m`, `.5h`,
   `1.5ms` = 1_500_000 ns). Integer-only behavior and all existing errors unchanged; truncate
   sub-nanosecond remainders; compute with integer math on the fractional digits (no float). The
   error text lists a decimal example. `--ttft` uses it (it already does).
5. `ctl scripts list` text output: one line per rule: `#<index> <name or -> hits/<times or inf>
   [exhausted] <matcher summary>`; matcher summary lists the set fields compactly (`model=gpt-*
   last_user=/x/ stream=true ...`, new fields included) and `-> text|tool_calls|error 429|hang`.
   `--json` unchanged. Unit-test the formatter.
6. `PUT /_fakellm/scripts?keep_hits=true` -> `Scripts::replace_keep_hits`; `ctl scripts put FILE
   --keep-hits`. `POST` unchanged.
7. Request log redaction: `Config::redact_auth: bool` (default false), `--redact-auth`
   (`FAKELLM_REDACT_AUTH`), applied inside `RequestLog::record`: values of `authorization`,
   `proxy-authorization` and `x-api-key` become `"<redacted>"` (header names kept). The
   `RequestLog::new` signature stays; add `RequestLog::set_redact_auth`.
8. `DEFAULT_MODELS`: drop `text-embedding-3-*`; add `gpt-5.5`, `claude-sonnet-4-5`,
   `claude-opus-4-1`, `claude-haiku-4-5` (decided beyond the brief: clients that list models
   should find what they will ask for). Fix any test that asserts the old list.
9. `lib.rs`: `Config` new fields (`redact_auth`); keep `Default` the same otherwise. Do not touch
   `pub use` of existing names; you may add `pub use` for new public types.

### Test gaps (e)

`tests/scripts_http.rs` (HTTP level, Chat endpoint, `common` helpers):

- The three examples of `docs/scripts.md` as integration tests: `tool_result_contains`
  (+ `last_tool_result`), `any_message`, `turn`, and the title side rule (`system` regex +
  `no_tools: true`, ordered before the main rule, with a request that has tools and a request that
  has none).
- Request log: capacity shrink via `PUT /_fakellm/requests/config` drops the oldest and reports
  `dropped`; `enabled:false` then `true` (nothing recorded while off, `seq` keeps counting);
  `since` + `after` + `limit` + `model` combinations.
- `maxRetries: 0` with a virtual-clock `retry-after` (already in TODO) if cheap in the SDK suite.

`sdk/node` (edit `openai.test.mjs`, `ai-sdk.test.mjs`, `helpers.mjs`): observe what the official
`openai` SDK and ai-sdk actually do on `cut_after_chunks` (check, then assert exactly that:
expect an error or an `interrupted`/early end, not a clean finish), and `n > 1` streaming via the
openai SDK (`choices` indexes, one finish per choice, content separated). Run
`npm test` in `sdk/node`; `cargo test` runs it through `tests/node_sdk.rs`.

## 11. Test helper (done): `tests/common/mod.rs`

`open_sse(server, path, headers, body) -> Sse`, `Sse::next_event()` / `events()` returning
`SseEvent {event: Option<String>, data}` (+ `.json()`), `post_json_with(server, path, headers,
body)`. The Chat-only `open_stream`, `Sse::next()`, `all()` stay. Add helpers to your own test
file rather than to `common/mod.rs` (it is shared and compiled into every test binary).

## 12. Decisions beyond the brief

- Script `probability` and `every_nth` count requests that *reached* the rule, not hits.
- `error.headers` is a `BTreeMap`.
- Anthropic bodies carry no top-level `request_id`; the `request-id` header is the same value as
  `x-request-id`.
- `trailing_tool_results` semantics (section 2).
- Default model list gains Claude names and `gpt-5.5`.
- `ApiError` grew; `#![allow(clippy::result_large_err)]` is set crate-wide in `lib.rs`.
- Anthropic `thinking` budget validation and Responses `max_output_tokens >= 16` follow the real
  API's rejections so harnesses see the same 400s.
- A hung request is logged with `status: 0` when the client gives up (4.3).
- `Message::is_error` and `Usage::cache_creation_tokens` were added to carry Anthropic
  `is_error` / `cache_creation_input_tokens`.
