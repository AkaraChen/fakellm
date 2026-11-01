# M4 implementation spec (internal)

Audience: the agents implementing M4 in parallel. Read this, `docs/design.md` (cassette and Hugging Face sections),
`docs/m3-spec.md` (the trajectory format is the base of the cassette format; its ownership rules still apply),
`docs/agent.md`, `docs/consumers.md` and the stub files you own. Everything marked "decided" is settled; do not
reopen it. Where this spec is silent, follow the nearest existing behavior and `docs/design.md`.

M4 is: cassettes (record and semantic replay), upstream proxy mode, Hugging Face import, structured output in the
fallback, control-plane snapshots, hot reload of the startup files and publishing dry runs. The tree at the start of
the Implement phase already contains the IR and Config extensions, the engine and pipeline glue, all routes and
flags, and stubs with `todo!()` bodies. `cargo build --all-targets`, `cargo test`,
`cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `(cd sdk/js && npm test)`,
`(cd bindings/node && npm run build:debug && npm test)` and `(cd go && go test ./...)` pass. Keep them passing.

## 1. Ownership

| Id | Owns (may edit) | Does not touch |
|---|---|---|
| (a) cassettes | `src/cassette/{mod,format,normalize,replay}.rs` (including `CassetteState`, `CassetteEntryState` and `Cassettes::export_state/import_state`, which (d)'s `snapshot.rs` only calls), `src/cassette_cli.rs`; the cassette routes in `src/control.rs` only if a bug needs it | everything below |
| (b) upstream | `src/upstream/{mod,client,record,reassemble}.rs`; the `proxy` function and `Trace.incoming/deferred` in `src/openai/mod.rs` only if a bug needs it | |
| (c) import | `src/import/{mod,hf,sharegpt,wildchat,hermes,swe_agent,openhands}.rs`, `src/import_cli.rs`, `tests/fixtures/hf/` | |
| (d) schema, snapshots, watch, publish | `src/ir/schema.rs`; the structured-output branch of `src/fallback.rs`; `src/snapshot.rs`; the stub bodies `Policies::export_state/import_state` (`src/policy/mod.rs`); `src/watch.rs`; `Cargo.toml` metadata, `.gitignore`/`exclude` for the package | |
| (e) tests | `tests/common/mod.rs` (additions only: helpers for a fake HF server, a raw upstream that records the bytes it receives, a temp watch file, free ports; no existing helper changes, and no other agent edits this file), `tests/cassette.rs`, `tests/upstream.rs`, `tests/import.rs`, `tests/schema.rs`, `tests/snapshots.rs`, `tests/watch.rs`, `tests/fixtures/` (except `hf/`), additions to `tests/bin.rs` and `tests/control.rs` | `src/` |

Rules for everybody (as in M3, adjusted):

- Frozen for everyone except where the table says otherwise: `src/ir.rs`, `src/lib.rs`, `src/control.rs`,
  `src/main.rs`, `src/ctl.rs`, `Cargo.toml` (except (d)'s publish metadata), the routing and pipeline in
  `src/openai/mod.rs`, the engine glue in `src/script.rs`. If you need a change there, stop and report it to the coordinator.
- Rust dependencies: only `Cargo.toml` as it is (`hyper-rustls` was added for TLS, section 4.6). No new ones.
- Unit tests in the module's `#[cfg(test)] mod tests`; HTTP tests in `tests/*.rs` with `tests/common/mod.rs`. Use the virtual
  clock instead of sleeping, except where this spec says "host clock" (hot reload polling, upstream timing).
- Delete the `#![allow(dead_code, unused_variables)]` of a stub file when no `todo!()` is left in it.
- No README/TODO/design edits and no user docs until the docs pass; put the facts that pass needs in your report.
- Style as fakes3: `//!` doc explaining the why, comments only for non-obvious decisions, `cargo fmt` (max_width 140).
  Code and comments English, user docs Chinese. No benchmarks, no commits.
- Network: only the Hugging Face datasets-server API in the one gated test (`FAKELLM_HF_TEST=1`, five rows of
  `allenai/WildChat-1M`) and an OpenAI-compatible upstream when `FAKELLM_UPSTREAM_TEST` is set. Everything else talks to a second
  fakellm instance or a local fake HTTP server.

## 2. What the contract already contains

- `ir.rs`: `Source::Cassette{name, index}` and `Source::Upstream` (the request log's `source=` filter knows `cassette` and
  `upstream`; `Source::rule()` is `None` for both); `Protocol: Default` (Chat) and serde defaults on `Request.protocol` and
  `Request.stream`, so hand-written cassette exchanges can omit them; `ApiError::upstream_unreachable` (502
  `fakellm_upstream_unreachable`) and `ApiError::upstream_incomplete` (502 `fakellm_upstream_incomplete`, log only); the empty
  module `ir::schema` with `pub fn instance(schema: &Value, seed: u64) -> Value` (stub).
- `cassette/`: the document types (`Cassette`, `Exchange`, `ExchangeMeta`, `CassetteOutcome`, `Normalizer`, `CassetteFormat`,
  `ToTrajectory`), `Cassette::new/to_json/to_jsonl`, `reply_spec`, `default_normalizers`, `DEFAULT_IGNORE`, the registry
  `Cassettes` (`list`, `get`, `remove`, `clear`, `clear_all`, `len`, `record`, `start_recording`, `recording` work; `replace`, `upsert`, `lookup`, the state import/export and everything in
  `normalize.rs` / `replay.rs` are stubs) and `tests/contract_m4.rs` (routes with empty registries, status fields, start-up validation).
- `script.rs`: `Engine.cassettes`; `Engine::unscripted` asks `Cassettes::lookup` first (it returns at once when nothing is
  registered), a hit becomes `Outcome` with `Source::Cassette` and the engine's default delivery, recorded replies with zero
  usage get tokenizer counts. `ScriptState`, `Scripts::export_state/import_state` (done).
- `agent/trajectory.rs`: `TrajectoryState`, `Trajectories::export_state/import_state` (done). `clock.rs`: `Clock::export/import`
  (done). `openai/responses_store.rs`: `ResponseStore::export/import` (done). `policy/mod.rs`: `PolicyState` and the stubs.
- `lib.rs`: `Config.{cassettes, upstream, upstream_api_key, record, watch}` (`upstream` is an `http::Uri`: there is no `url`
  crate), `Server::cassettes()`, re-exports (`Cassette`, `Exchange`, `CassetteFormat`, `HttpClient`, `WatchConfig`, ...), `pub mod import`;
  `Server::start` spawns `watch::spawn` when `Config::watch` is non-empty and aborts it on shutdown.
- `control.rs`: `Control.{snapshots, upstream}`; `ResetOptions.snapshots` (default false); `ControlError::Conflict` (409); routes for
  cassettes and snapshots (section 3.6, 8), status JSON gains `cassettes`, `snapshots` and `upstream` (`{url, record, recorded}`);
  `reset` with `scripts=true` also clears all cassettes and the recording. `Control::new` registers `Config::cassettes`, builds
  the `Upstream` (`record` without `upstream` is an error).
- `openai/mod.rs`: `Trace.incoming/deferred`, `proxy()` (section 4.5), the unknown-`previous_response_id` fallback
  (`parse_without_history`), the embeddings hook. `handle` skips the log record for relayed requests (`finish` writes it).
- `main.rs`: `--cassette FILE` (repeatable, format by extension), `--upstream URL`, `--upstream-api-key`, `--record FILE`,
  `--no-watch` (env `FAKELLM_CASSETTES`, `_UPSTREAM`, `_UPSTREAM_API_KEY`, `_RECORD`, `_NO_WATCH`), subcommands `import` and
  `cassette`; default log filter `warn,fakellm::watch=info,fakellm::upstream=info`.
- `ctl.rs`: `ctl cassettes {list, put, add, get NAME [--jsonl], delete NAME, clear}`, `ctl snapshot {create [NAME], list, restore NAME,
  delete NAME}`, `ctl reset --drop-snapshots`, status lines (done, with unit tests).
- `upstream/mod.rs`: `Upstream::new`, `target()` (URL join, tested), `upstream/client.rs`: `HttpClient::{new, send, get}` (real).
- Beyond the first contract: `ApiError::upstream_loop` (508 `fakellm_upstream_loop`); `Upstream::{next_hop, request_headers, with_local_key, id}` with the loop guard and the credential table of 4.2 (real, unit
  tested) and `HOP_HEADER`; `Server::start` refuses an `upstream` that is the bound address (4.1); `Control::exclusive()` (the admin lock, 9) taken by every non-GET control route except the snapshot ones;
  `Policies::prepare/commit` (the two halves of `replace`, 9).
- `bindings/node/src/lib.rs`: `..d` in the `ResetOptions` literal (SDK helpers for cassettes and snapshots are not part of M4).

## 3. Cassettes (a)

### 3.1 Document format (`kind: "cassette"`)

```json
{
  "kind": "cassette",
  "name": "weather",
  "recorded_at": "2026-11-01T10:00:00Z",
  "upstream": "https://api.openai.com",
  "ignore": ["!max_tokens", "system"],
  "normalize": [{"pattern": "Today is \\w+", "replace": "Today is <day>"}],
  "normalize_defaults": true,
  "match_tool_schemas": false,
  "model_glob": "*",
  "fuzzy": false,
  "once": false,
  "meta": {"note": "recorded against gpt-4o"},
  "exchanges": [
    {"request": { ...IR Request... },
     "outcome": {"outcome": "reply", "reply": { ...IR Reply... }},
     "protocol": "openai.chat",
     "meta": {"model": "gpt-4o", "duration_ms": 812, "upstream_status": 200},
     "ignore": []}
  ]
}
```

- Types are in `src/cassette/format.rs` (`deny_unknown_fields`; `kind` defaults to `cassette` and is written first; defaults are
  omitted on output). `Cassette::to_json` / `parse_many` round-trip: `parse_many(to_json(c)) == [c]`, JSONL included.
- `request` is the IR `Request` exactly as `GET /_fakellm/requests?full=true` shows it (`protocol` and `stream` may be left out).
  `outcome` has the tag and fields of `Outcome::Reply` / `Outcome::Error` minus `delivery` and `source` (`CassetteOutcome`); a full
  `Outcome` JSON is accepted on read, the extra keys ignored. `Hang` is never recorded. `protocol` is the protocol the exchange was
  recorded under (default: `request.protocol`); it is informational, replay is cross-protocol. `meta.model` defaults to
  `request.model`. `meta.duration_ms` and `meta.upstream_status` are set by the recorder; other `meta` keys round-trip (importers put the
  row id in `meta.row`).
- Document-level fields besides `exchanges`: `ignore`, `normalize`, `normalize_defaults`, `match_tool_schemas`, `model_glob`, `fuzzy`, `once`
  are the matching options (3.2-3.4); `recorded_at` (RFC 3339) and `upstream` are provenance; `meta` is free-form. `exchanges` may be empty.
- Per-exchange `ignore` adds to the cassette's list for that exchange only.
- **Forms** (as trajectories, `docs/m3-spec.md` 3.2): JSON / YAML document, array, `{"cassettes": [...]}`; JSONL = a header line (the document
  without `exchanges`, `kind: "cassette"` and `name` required) followed by one exchange per line; a further header starts the next cassette; a
  line with `exchanges` is a whole document; blank lines ignored. `CassetteFormat::Auto` uses the same detection as `TrajectoryFormat::Auto`
  (several non-empty lines that each parse as a JSON object: JSONL; else `[`/`{`: JSON; else YAML). A trajectory document (`kind: "trajectory"`,
  or a `steps` key) is a clear error ("this is a trajectory; use --trajectory"), as is a cassette given to `--trajectory`
  (`Trajectory::parse_many` already rejects the unknown `exchanges` key; leave it).
- `validate()`: `name` non-empty, no `/`, not `recording`; every `ignore` entry known (3.2); every `normalize` pattern compiles; `model_glob` non-empty;
  every exchange has at least one message. Errors read `cassette "x": exchange 3: ...`. `Cassettes::replace/upsert` compile the matchers and one key
  per exchange (3.3) and are all-or-nothing; a same-named upsert replaces in place and restarts its counters; duplicate names in one call are an error.

### 3.2 The ignore list

Entries are IR field names; they are dropped from the comparable form of both the recorded and the live request.

- Default (`DEFAULT_IGNORE`): `temperature`, `top_p`, `seed`, `user`, `metadata`, `stream`, `include_usage`, `store`, `max_tokens`,
  `reasoning_effort`, `tool_choice=auto`, `previous_response_id`, `extra.*`. (`previous_response_id` is beyond the brief: the id is random per run and the
  history behind it is already in `messages`.)
- A cassette's `ignore` is added to the defaults; `!field` removes a default (`"!max_tokens"` makes `max_tokens` significant); the exchange's `ignore` is added
  on top for that exchange. `field=value` ignores the field only when its value is `value` (`tool_choice=auto`: ignored when the choice is `auto` or absent; a forced tool still counts).
- Accepted names: `temperature`, `top_p`, `seed`, `user`, `metadata`, `stream`, `include_usage`, `store`, `max_tokens`, `n`, `stop`, `parallel_tool_calls`,
  `reasoning_effort`, `thinking`, `response_format`, `previous_response_id`, `tool_choice`, `extra.<key>`, `extra.*`, plus the pseudo-fields `system` (system and
  developer messages are left out) and `tools` (the tool list is left out). `model` is not ignorable (use `model_glob`); `messages` is not ignorable. Anything else:
  `validate()` error naming the entry.

### 3.3 Normalization and the comparable form

`Matcher::key(req, exchange_ignore)` builds one canonical JSON text (`normalize::canonical`: object keys sorted, compact) from:

1. `messages`, each as `{role, content, tool_calls?, is_error?}`: `developer` becomes `system`; `content` is the message text with every normalizer applied (text
   parts joined with `\n`; images contribute their URL, audio their format and length, other parts their canonical JSON); an assistant's `tool_calls` are
   `{name, arguments}` with `arguments` parsed as JSON when valid (so key order and whitespace do not matter, then each string value normalized), else the
   normalized string; **tool call ids are not compared** (`tool_call_id` is dropped; ids differ per run and per protocol), tool results are matched by position and
   text; `reasoning`, `reasoning_opaque` and `Thinking`/`RedactedThinking` parts are dropped; `Message.name` is dropped. With `system` ignored, system and developer
   messages are removed.
2. `tools` (unless ignored): the sorted list of names; with `match_tool_schemas`, `{name, parameters}` sorted by name, `parameters` canonical.
3. The scalar fields not ignored, under their IR names: `tool_choice`, `parallel_tool_calls`, `n`, `stop`, `thinking`, `response_format`, `temperature`, ...,
   `extra.<k>` (values canonical). `model` is not part of the key.

The normalizers are `default_normalizers()` (unless `normalize_defaults: false`) then the cassette's `normalize`, applied in order to every text above (also to
string values inside parsed tool arguments). Regexes are Rust `regex`; a bad pattern is a registration error. Example of a user normalizer:
`{"pattern": "(?s)<system-reminder>.*?</system-reminder>", "replace": ""}`.

Normalizers apply to **every message text, tool results (`role: tool` contents) included**, and to string values in tool call arguments. The defaults are deliberately narrow (timestamps, UUIDs, `req_` ids):
a bare date such as `2025-03-14` is **not** a default, because two prompts that differ only in a date usually should not share a reply. Things that vary per run and are not covered
(bare dates, epoch seconds, temp directories, git shas) are the cassette's own `normalize` job; documented recipes for the docs pass:
`{"pattern": "\\b\\d{4}-\\d{2}-\\d{2}\\b", "replace": "<date>"}`, `{"pattern": "\\b1[5-9]\\d{8}\\b", "replace": "<epoch>"}`, `{"pattern": "/tmp/[A-Za-z0-9._-]+", "replace": "/tmp/<tmp>"}`,
`{"pattern": "\\b[0-9a-f]{40}\\b", "replace": "<sha>"}`.

`Matcher::accepts_model(model)`: `glob_match(model_glob, model)` against the **request's** model; a cassette recorded with `gpt-4o` and left at `*` replays for any
model, a pinned `model_glob: "gpt-4o*"` only for those.

### 3.4 Lookup (`Cassettes::lookup`)

Called by the engine for requests no script rule answered. Embeddings requests (`Protocol::OpenaiEmbeddings`) never match. Under one lock:

1. For each registered cassette in order, if `accepts_model`: compute the request's key with the cassette's matcher and find the first exchange whose
   precomputed key is equal and, for `once` cassettes, not yet consumed. The first cassette with such an exchange wins.
2. Else, for each cassette in order with `fuzzy: true` and an accepted model: over its unconsumed exchanges, `Matcher::similarity` (same number of messages and same normalized last
   user text, else `None`; the score is the number of messages equal from the end after normalization; highest score wins, earliest exchange on a tie). The best across cassettes
   (earlier cassette on a tie) wins.
3. Else `None`: the request falls through to the upstream (if configured), then strict mode or the default reply. A miss is not an error.

A hit increments the cassette's `hits`; in a `once` cassette it marks the exchange consumed (the same recorded request twice with different replies replays them in
recording order; when all are spent the request falls through). Without `once` an exchange answers any number of times, and among equal exchanges the first always wins.
The recording cassette is not consulted. The outcome is the exchange's `CassetteOutcome` (usage as recorded, tool call ids as recorded, errors as recorded including
`status`, `kind`, `code`, `retry_after`, headers and `in_stream`); the replay is rendered in the **request's** protocol and streaming mode. `max_tokens` is not applied to replays.

### 3.5 Cassette to trajectory

`Cassette::to_trajectory(&ToTrajectory)` and `fakellm cassette to-trajectory FILE -o OUT [--name] [--with-expect] [--keep-errors] [--model GLOB] [--json]`:

- Walk the exchanges in order; skip those whose model does not match `--model`; skip error outcomes unless `keep_errors` (then the step is `{reply: {error: {...}}}`).
- Keep an exchange when `request.assistant_turns() == steps_so_far`; the kept exchange's reply is the next step (`reply_spec`: text, reasoning, tool calls with ids,
  `finish_reason`, output usage). Side requests (titles, other conversations) and retries have a different count and drop out; the result replays with `cursor: by_turn`.
- `--with-expect`: for a step that follows tool calls, `expect.tool_results` has `{name, id}` for each tool result the kept request carried after its last assistant message.
- Header: `name` (default: the cassette's), `meta: {source: "cassette", cassette: <name>, upstream: ...}`, defaults for the rest. No exchange kept: error
  (`no exchange of cassette "x" starts a conversation`).
- Output: the trajectory as JSONL (`Trajectory::to_jsonl`), or `--json`. The CLI prints `N of M exchanges kept` to stderr.

### 3.6 Control plane (done, `src/control.rs`)

| Route | Behavior |
|---|---|
| `GET /_fakellm/cassettes` | `{"cassettes": [CassetteInfo]}`: `name, exchanges, once, fuzzy, hits, consumed, recording`; registered ones in order, then the recording if it has been created |
| `PUT /_fakellm/cassettes` | replace all registered; body: cassette, array, `{"cassettes": [...]}`, JSONL (`application/x-ndjson`), YAML; `{"added": n, "count": total}`; a 400 changes nothing |
| `POST /_fakellm/cassettes` | add; same-named replaced in place (counters restart) |
| `DELETE /_fakellm/cassettes` | `{"cleared": true}`; the recording stays |
| `GET /_fakellm/cassettes/{name}[?format=jsonl]` | the document (404 when absent); `recording` is the in-memory recording; JSONL has `Content-Type: application/x-ndjson` |
| `DELETE /_fakellm/cassettes/{name}` | `{"deleted": true}` or 404; `recording` empties the recording |

`POST /_fakellm/reset` (`scripts=true`) clears everything including the recording. The name `recording` is reserved: registering a cassette with it is a 400.

### 3.7 Request log

`source` of a replayed request is `{"kind":"cassette","name":"<cassette>","index":<exchange>}`; `?source=cassette` filters; `rule` is null.

## 4. Upstream proxy (b)

### 4.1 Configuration

`Config.upstream: Option<http::Uri>` (`--upstream URL`, `http://` or `https://`, optional base path), `Config.upstream_api_key` (`--upstream-api-key`, requires
`--upstream`), `Config.record` (`--record FILE`, requires `--upstream`). The control plane (`/_fakellm/*`) is never forwarded. `GET /v1/models`, `GET/DELETE
/v1/responses/{id}` and `POST /v1/messages/count_tokens` are answered locally even in upstream mode (decided: they are not model calls). Forwarded: `POST
/v1/chat/completions`, `/v1/responses`, `/v1/messages`, `/v1/embeddings` (and their un-prefixed and gateway-prefixed spellings, routed by `api_path`).

Order of sources: script rule, cassette (`Engine`), upstream (`openai::proxy`), default reply / strict error. So `--strict` plus `--upstream` means "scripts and
cassettes, then the real server"; the strict error is not produced. A cassette hit is never recorded again. If `--api-key` (fakellm's own) is set and no
`--upstream-api-key`, fakellm's own key is **never forwarded** (see the table in 4.2): the relayed request carries no credentials, which will usually be a 401 from the upstream; `Control::new` logs a `tracing::warn!` (target `fakellm::upstream`).

**Loops.** An upstream that leads back to the instance (or around a cycle of instances) would relay a request to itself without end. Two guards: `Server::start` fails with `InvalidInput` when the upstream URL names the bound
address (same port and the bound IP, or `localhost` / a loopback IP when bound to loopback or an unspecified address; no DNS lookup), and every relayed request carries `x-fakellm-hop: <id>[,<id>...]`
(each `Upstream` has an id unique per process); `forward` refuses a request whose chain already contains its own id, or is 8 ids long, with `ApiError::upstream_loop` (508 `fakellm_upstream_loop`, rendered in the protocol's error
format and logged with `Source::Upstream`, nothing sent upstream). The ids make a legitimate chain `client -> fakellm A -> fakellm B -> real server` work.

### 4.2 Forwarding (`Upstream::forward`)

Contract in the doc comment of `forward`. Details:

- Request body: the client's bytes, untouched (not re-rendered from the IR). Headers: end-to-end headers copied; hop-by-hop ones (`connection`, `keep-alive`,
  `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`, `transfer-encoding`, `upgrade`) and `host`, `content-length` dropped (hyper sets them);
  `accept-encoding` forced to `identity`. Implemented by `Upstream::request_headers` (done, unit tested), which also adds `x-fakellm-hop` and enforces the loop guard of 4.1. Credentials (decided):

  | `--upstream-api-key` | Client sent | Upstream receives |
  |---|---|---|
  | set | `authorization`, `x-api-key`, both or neither | both of the client's dropped; `authorization: Bearer <key>`, plus `x-api-key: <key>` when the request is `anthropic.messages` |
  | unset | `authorization` and/or `x-api-key` | each passed through unchanged (both when both are present: fakellm does not choose), **except** a value equal to fakellm's own `--api-key` (`Bearer <own>` / `<own>`), which is dropped |
  | unset | neither | neither |

  `anthropic-version` is passed through; an Anthropic request without it gets `2023-06-01`. `openai-organization`, `openai-project`, `anthropic-beta` and every other end-to-end header pass through. A key that cannot be a header value is a start-up error.
  Test: the upstream instance's request log never shows fakellm's own key, and shows the replacement key when one is set.
- URL: `Upstream::target` (tested). The query string is kept.
- Response: status and end-to-end headers unchanged (a `content-length` is kept; the body is not altered), body forwarded frame by frame. Implement as a `http_body::Body`
  wrapper around `Incoming` (`hyper::body::Body`/`Frame` are available through hyper) that feeds each data frame to a `Reassembler` and, when the inner body ends or errors, or the wrapper is
  dropped, calls `on_complete` exactly once with a `Completed` (synchronously, before yielding the final `None`). Upstream HTTP errors (4xx/5xx) are relayed too and reassembled into an
  error outcome. A body error mid-stream is forwarded as a body error (the client sees a broken stream), `Completed.outcome = None`, `problem` set.
- Connection refused, DNS failure, TLS failure or no response head within `client::HEADER_TIMEOUT` (120 s): `Err(ApiError::upstream_unreachable(base, detail))`; `proxy()` renders it in the
  protocol's error format and logs it with `Source::Upstream`.
- Timing is real time; the virtual clock does not apply to relayed responses. `Completed.duration` is host time from sending to the end of the body.

### 4.3 Reassembly (`src/upstream/reassemble.rs`)

`Reassembler::finish(status, headers)`: `status >= 400` is an error body; `content-type` starting `text/event-stream` is SSE; else a sync JSON body. SSE framing: events separated by a blank line (`\n\n` or
`\r\n\r\n`), `data:` lines joined with `\n`, `event:` names used by Anthropic and Responses; comments (`:`) and unknown events ignored; invalid JSON in a `data:` line is skipped (and counts as a
problem only if no final event arrives). `[DONE]` ends a Chat stream.

**Finish reasons**: Chat `stop`, `length`, `tool_calls` / `function_call` (ToolCalls), `content_filter`; Anthropic `end_turn` / `stop_sequence` / `pause_turn` (Stop), `max_tokens` (Length), `tool_use` (ToolCalls), `refusal` (ContentFilter);
Responses: `status: completed` is Stop, ToolCalls when an output item is a function/custom call, `incomplete` with `incomplete_details.reason` `max_output_tokens` is Length and `content_filter` ContentFilter. Unknown values: Stop.

**Usage** (`Reply.usage`): Chat `prompt_tokens`, `completion_tokens`, `prompt_tokens_details.cached_tokens` (cached), `completion_tokens_details.reasoning_tokens`. Anthropic `prompt_tokens = input_tokens + cache_read_input_tokens +
cache_creation_input_tokens` (the IR keeps the total, see docs/m3-spec.md D5), `cached_tokens = cache_read_input_tokens`, `cache_creation_tokens`, `completion_tokens = output_tokens` (the last `message_delta` value wins over `message_start`'s).
Responses `input_tokens`, `output_tokens`, `input_tokens_details.cached_tokens`, `output_tokens_details.reasoning_tokens`. Missing usage: zeros (the replay then counts with the tokenizer).

| Protocol | Sync body | SSE |
|---|---|---|
| Chat | `choices[0].message`: `content` (text), `refusal`, `reasoning_content` or `reasoning` (reasoning), `tool_calls[].{id, function.{name, arguments}}` | `choices[0].delta` pieces concatenated: `content`, `refusal`, `reasoning_content`/`reasoning`, `tool_calls[]` merged by `index` (`id` and `function.name` from the first fragment that has them, `function.arguments` concatenated in order); `finish_reason` from the chunk that has it; `usage` from the chunk that has it (the extra final chunk with empty `choices`); a frame `{"error": {...}}` is an in-stream error. Complete when a `finish_reason` was seen |
| Anthropic | `content[]`: `text` blocks joined (no separator), `tool_use` -> tool call (`id`, `name`, `arguments` = compact JSON of `input`), `thinking` -> `reasoning` (+ `signature` -> `reasoning_opaque`), `redacted_thinking` dropped; `stop_reason`, `usage` | `message_start` (usage), `content_block_start` (block kind, tool `id`/`name`), `content_block_delta` (`text_delta`, `input_json_delta` -> arguments, `thinking_delta`, `signature_delta`), `content_block_stop`, `message_delta` (`stop_reason`, usage), `message_stop`; `ping` ignored; an `error` event is an in-stream error. Complete at `message_stop` |
| Responses | the response object: `output[]` items in order: `message` (`content[]` `output_text` joined -> text, `refusal` -> refusal), `function_call` (`call_id` as id, `name`, `arguments`), `custom_tool_call` (`call_id`, `name`, `input` as the arguments string), `reasoning` (`summary[].text` joined with a blank line -> reasoning, `encrypted_content` -> reasoning_opaque); `status`, `incomplete_details`, `usage`, `error` | `response.completed` / `response.incomplete` carry the whole `response` (take it as in the sync case; it is also `Reassembled::response`); `response.failed` -> error from `response.error` `{code, message}`; an `error` event -> in-stream error. If neither final event arrived but `response.output_item.done` events did, the stream is incomplete (an error `finish`) |
| Embeddings | `usage.prompt_tokens` only (`Reply` with no text; `data[].embedding` is dropped) | n/a |

**Errors**: OpenAI `{"error": {message, type, code, param}}` -> `ApiError{status, kind: type, code, message, param}`; Anthropic `{"type": "error", "error": {type, message}}` -> `kind: error.type`, no code; `retry-after` header (seconds)
-> `retry_after`; `retry-after-ms`, `x-should-retry` kept in `headers`. A non-JSON body: `kind` from `ApiError::for_status_in(protocol, status)`, `message` the body text (first 500 characters). In-stream errors: `in_stream: true`,
`status` from the error type (`rate_limit_error`/`rate_limit_exceeded`: 429, `overloaded_error`: 529, `invalid_request_error`: 400, `authentication_error`: 401, `permission_error`: 403, `not_found_error`: 404, else 500).
A body that is cut short (SSE without its final event) or not JSON where JSON is expected: `finish` returns `Err(reason)`; nothing is recorded.

### 4.4 Recording

`upstream::record::finish` (called from the body wrapper, see 4.2), in order:

1. If `completed.outcome` is `Some`: build `Exchange{request: (*f.request).clone(), outcome, protocol: request.protocol, meta: {model: request.model, duration_ms, upstream_status: Some(status)}, ignore: []}`; `Cassettes::record(exchange.clone())`
   (creates the `recording` cassette on first use with `recorded_at` = now (server clock) and `upstream` = the configured URL, set by `start_recording` at `Control::new`) and `Recorder::append` (a write error logs a `warn!` and does not fail the request).
   Embeddings exchanges are recorded too (reply with usage only); they are never replayed (3.4).
2. Responses: when `completed.response` is `Some` and `f.store_response`, store a `StoredResponse` under the **upstream's** id (`response.id`) built with `responses::to_stored(req, reply, meta)` where `meta.id` is the upstream id and `created` its `created_at`,
   so the client's next request with `previous_response_id` finds its history locally, in the IR and in the recording. (If the response object is not stored, a later unknown `previous_response_id` is forwarded with partial IR, `parse_without_history`.)
3. If `f.head` is `Some` and `control.generation() == f.generation`: `log.record(RequestRecord{status: completed.status, latency: f.started.elapsed(), outcome: Some(Arc::new(Outcome::Reply|Error{.., source: Source::Upstream})), ..head})`. When `completed.outcome` is
   `None` the outcome is `Outcome::Error{error: ApiError::upstream_incomplete(problem), source: Source::Upstream}` and nothing was recorded in step 1.

The record file (`Recorder`): JSONL cassette. A new or empty file starts with the header `{"kind":"cassette","name":"<file stem>","recorded_at":"...","upstream":"..."}`; an existing non-empty file is only appended to, so recordings continue
across runs and the header (and any matching options a person added to it) stays. Each exchange is one line, written with a single `write_all` and flushed. The in-memory `recording` and the file hold the same exchanges while the process lives.

### 4.5 Pipeline integration (done)

`openai::complete` after `Engine::resolve_async`: `if control.upstream.is_some() && source is Fallback|Strict` -> `proxy()`. `embeddings::respond` calls `proxy()` after its error-rule check. `proxy` builds the `Finish`, calls `Upstream::forward` with `finish` as the
callback, sets `trace.deferred` so `handle` does not log, and renders `Err` locally. A Responses request whose `previous_response_id` the store does not know is parsed without it (history missing in the IR) and forwarded with the original bytes. IR parse errors
(`400`s from our own parsers) are returned locally and not forwarded. API-key checking (`--api-key`) happens before, as always. Requests that scripts or cassettes answered never reach the upstream.

### 4.6 TLS dependency (decided)

`hyper-rustls 0.27` with `default-features = false, features = ["http1", "ring", "tls12", "webpki-roots"]`, plus `hyper-util`'s `client-legacy` and `http1` features. Chosen over `reqwest` (rustls + stream features), which brings tower, an h2 stack,
and redirect/cookie layers on top of the same hyper-rustls for nothing this crate uses; both need a C compiler for `ring` (present in `rust:alpine`; `musl-dev make perl` in the Dockerfile are enough, verify at the first docker build). `ring` over `aws-lc-rs` (which
needs cmake and a heavier C build); bundled `webpki-roots` over the OS store so the alpine image needs no `ca-certificates` and behavior does not depend on the machine. The client is http/1.1 only, no redirect following. The Docker image gains nothing at runtime.

## 5. Request log and filters

`source=` accepts `script` (rules, trajectories, policies), `cassette`, `upstream`, `fallback`, `strict`, `rejected`. `ctl requests list --source cassette` works through the existing flag. `GET /_fakellm` status: `cassettes.count`, `snapshots.count`,
`upstream` (`null` or `{url, record, recorded}`).

## 6. Hugging Face import (c)

### 6.1 Fetching (`import/hf.rs`)

`fetch_rows(client, endpoint, source, token)`: `GET {endpoint}/rows?dataset=<d>&config=<c>&split=<s>&offset=<o>&length=<n>` with `n = min(100, remaining)`, repeated from `offset` until `limit` rows or an empty page; response `{"rows": [{"row_idx", "row", "truncated_cells"}], "num_rows_total": N}`.
Rows with a non-empty `truncated_cells` are left out and counted in `Rows::truncated` (the server cut a cell, the row is incomplete). Without `--config`, `GET {endpoint}/splits?dataset=<d>` and the first `splits[]` entry whose `split` matches (its `config`). Headers: `authorization: Bearer $HF_TOKEN` when the variable is set and
non-empty. Errors (`ImportError`): 401/403 -> "dataset <d> is gated or private; set HF_TOKEN to a token that can read it"; 404 -> "dataset, config or split not found" (with the configs and splits `/splits` lists, when it answers); 429 -> one retry after `retry-after` (at most 5 s) then an error; other
non-200 -> the server's `error` field with the status. The response size limit per page is 32 MiB. `endpoint()` reads `FAKELLM_HF_ENDPOINT` so tests can point at a local server; the default is `https://datasets-server.huggingface.co`.

### 6.2 Converters (shapes written from knowledge of the datasets; fixtures are marked synthetic)

All converters produce `Conversation` (IR messages). Common rules: roles map to `Role`; empty-text messages with no tool calls are dropped; tool calls get ids `call_<n>` (1-based per conversation) unless the row has ids, and the tool message that answers carries the same `tool_call_id`;
a conversation without an assistant message is skipped (`"no assistant turn"`); a row without the expected field is skipped (`"no <field> field"`); malformed single turns (not an object, no text) are skipped silently inside an otherwise good row. `Conversation.id` is the row's id field or `row-<index>`.

| Format | Row shape | Mapping |
|---|---|---|
| `sharegpt` | `conversations: [{from, value}]`, `id` | `from`: `system` -> system, `human`/`user` -> user, `gpt`/`assistant`/`chatgpt`/`bing`/`bard` -> assistant, `tool`/`observation`/`function`... see below. A `system` turn only at the start stays system; unknown `from` skips the turn. `tool` / `observation` -> a tool message answering the previous assistant tool call when there is one, else dropped |
| `wildchat` | `conversation: [{role, content, ...}]`, `conversation_hash`, `model`, `language`, `toxic`, `redacted`, `timestamp`, `country`, `state` | `role` as IR; `content` string (parts arrays: text parts joined). `model` -> `Conversation.model`; `conversation_hash` the id; `meta`: `language`, `toxic`, `redacted`, `timestamp` |
| `hermes-fc` | `conversations: [{from, value}]` with `from` `system` / `human` / `gpt` / `tool`; `tools` (JSON string or array), `id`, `category`, `subcategory`, `task` | in `gpt` turns every `<tool_call>...</tool_call>` block is parsed as JSON `{"name", "arguments"}` (Python-dict style with single quotes accepted via a lenient fallback; `arguments` may be an object or a JSON string) into a tool call, the text outside the blocks (and `<think>...</think>` stripped into `reasoning`) is the assistant text; `tool` turns hold one or more `<tool_response>...</tool_response>` blocks, each a tool message answering the pending call in order. `tools` -> `Conversation.tools` (OpenAI function-tool objects or bare `{name, description, parameters}`). The system turn is kept as system text with the `<tools>...</tools>` block removed. `meta`: `category`, `subcategory`, `task` |
| `swe-agent` | (nebius) `trajectory: [{role: system\|user\|ai, text, system_prompt?}]`, `instance_id`, `model_name`, `target`, `exit_status`, `generated_patch`; (SWE-smith) `messages: [{role, content, tool_calls?, tool_call_id?}]`, `instance_id`, `traj_id`, `model`, `resolved` | nebius: `ai` -> assistant; the assistant text is split into the discussion (text outside fenced blocks) and the first fenced block, which becomes a call: `bash` with `{command: <block>}` (the language tag is ignored), or `str_replace_editor` when the block's first word is `str_replace_editor` (its `--flag value` arguments are not parsed: the whole line goes in `command`); the next `user` turn is the observation: a tool message answering that call (a `user` turn after a turn without a block stays a user message). SWE-smith: `assistant.tool_calls[].function.{name, arguments}` (arguments JSON string) as structured calls, `tool` messages as results (by `tool_call_id`), content may be a string or a list of `{type: text, text}`. `Conversation.tool_map` = `swe_agent::default_tool_map()`. `meta`: `instance_id`, `model_name`/`model`, `exit_status`, `target`/`resolved`, `generated_patch` length |
| `openhands` | `events` (or `history`, `trajectory`): `[{id, source: agent\|user\|environment, action?, args?, observation?, content?, message?, cause?, extras?}]` | `action: message` + `source: user` -> user, `source: agent` -> assistant text; `action: run` -> `execute_bash {command}`; `edit` -> `str_replace_editor` (`args`: `command`, `path`, `old_str`, `new_str`, `file_text`, `insert_line`); `read` -> `str_replace_editor {command: "view", path}`; `finish` -> assistant text from `args.final_thought` / `args.message`; `think` -> becomes `reasoning` of the next assistant message; other actions: a tool call with the action name and `args`. Each observation event (`cause` = id of the action, else the preceding action) is a tool message answering that call, text = `content`. `system` events are the system message. `meta`: `instance_id` |

`detect(row)`: `events` -> OpenHands; `trajectory` list whose items have `role: ai` or `text`, or `messages` whose items have `tool_calls` -> SweAgent; `conversations` with `from` -> Hermes when any `gpt` value contains `<tool_call>` or the row has `tools`, else ShareGPT; `conversation` with `role` -> WildChat; else `None`.
`convert_rows(.., Auto)` detects per row. Unknown rows: skipped with the reason `"unrecognized row shape"`.

### 6.3 Outputs

`to_cassette(conversations, opts)`: one `Cassette{name: opts.name, meta: {source, rows}}` with an `Exchange` for every assistant message: `request` = IR `Request::new(model, messages before it)` with `tools` from the conversation (or one name-only `ToolDef` per tool name used anywhere in it), protocol Chat; `outcome`
= `Reply{text, tool_calls, reasoning, finish: ToolCalls|Stop}` with zero usage; `meta: {model, row: <conversation id>}`. `model` = the conversation's, else `opts.model`. `to_trajectories`: per conversation with an assistant message, `Trajectory{name: <prefix>-<sanitized id>, steps}`, one step per
assistant message (`reply: {text?, tool_calls?, reasoning?}`, ids kept, no `expect`), `tool_map` from the conversation, `meta: {source, id, ...conversation meta}`. Names are made unique (`-2`, `-3` suffixes) and free of `/`. Output validates with `Cassette::validate` / `Trajectory::validate`.

### 6.4 CLI

`fakellm import hf <dataset> [--config C] [--split train] [--limit 100] [--offset 0] [--format auto|sharegpt|wildchat|hermes-fc|swe-agent|openhands] [--as cassette|trajectory] [--name N] [--model M] -o FILE` and `fakellm import file <PATH|-> ...` (same output
flags; reads a JSON array, `{"rows": [...]}` or JSONL, so a dataset dump or a hand-made fixture converts offline - beyond the brief). `-o -` writes to stdout. Cassettes are written with `Cassette::to_jsonl`, trajectories as concatenated `Trajectory::to_jsonl` (a multi-trajectory JSONL). `--as` defaults to `Format::default_output`;
`--name` defaults to the dataset name with `/` replaced by `-`, or the file stem. A summary goes to stderr: `fetched N rows (T truncated), converted C conversations -> E exchanges|S steps, skipped K (reason: n, ...)`. Exit 0 on success (skipped rows are not failures), 1 on errors (network, bad
arguments, nothing converted), 2 for usage errors (clap).

### 6.5 Canonical fixtures (the shared contract between (c) and (e))

(c) writes, and (e) asserts against, `tests/fixtures/hf/{sharegpt,wildchat,hermes,swe_agent,openhands}.jsonl`, each exactly three JSON rows, each row carrying `"_comment": "synthetic"`. Row 1 of each file is given below verbatim; rows 2 and 3 are fixed by the table.
Expected values are for `convert_rows(.., <that format>)` and, unless said, for `--format auto` too (the `_comment` field does not take part in `detect`). Rows 3 are skipped with `no assistant turn`.

| File | Row 1 (verbatim) | Row 1 converts to | Row 2 | Totals |
|---|---|---|---|---|
| `sharegpt` | `{"_comment":"synthetic","id":"sg-1","conversations":[{"from":"system","value":"Be brief."},{"from":"human","value":"Hi"},{"from":"gpt","value":"Hello!"}]}` | id `sg-1`; system "Be brief.", user "Hi", assistant "Hello!" | `sg-2`: human, gpt, human, gpt (2 assistant turns) | 2 converted, 1 skipped; 3 exchanges, 3 steps |
| `wildchat` | `{"_comment":"synthetic","conversation_hash":"abc123","model":"gpt-4","language":"English","toxic":false,"redacted":false,"timestamp":"2023-05-01T10:00:00+00:00","country":"US","state":"Ohio","conversation":[{"role":"user","content":"Hi"},{"role":"assistant","content":"Hello!"}]}` | id `abc123`, model `gpt-4`; user, assistant; meta `language` "English", `toxic` false, `redacted` false, `timestamp` | `abc124`: user, assistant, user, assistant | 2 converted, 1 skipped; 3 exchanges |
| `hermes` | `{"_comment":"synthetic","id":"h1","category":"Weather","subcategory":"Forecast","task":"Get weather","tools":"[{\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"description\":\"Weather\",\"parameters\":{\"type\":\"object\",\"properties\":{\"city\":{\"type\":\"string\"}},\"required\":[\"city\"]}}}]","conversations":[{"from":"system","value":"You are a function calling AI.\n<tools>[]</tools>"},{"from":"human","value":"Weather in Paris?"},{"from":"gpt","value":"<tool_call>\n{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Paris\"}}\n</tool_call>"},{"from":"tool","value":"<tool_response>\n{\"temp\": 18}\n</tool_response>"},{"from":"gpt","value":"It is 18 C in Paris."}]}` | system "You are a function calling AI." (tools block removed); user; assistant with one call `get_weather {"city":"Paris"}` id `call_1`, no text; tool `call_1` `{"temp": 18}`; assistant "It is 18 C in Paris."; 1 tool in the conversation; meta `category` "Weather" | `h2`: the call in Python-dict quoting `{'name': 'get_weather', 'arguments': {'city': 'Rome'}}` after a `<think>plan</think>` (reasoning "plan"), then tool, then answer | 2 converted, 1 skipped; 4 exchanges; auto detects Hermes (has `tools`) |
| `swe_agent` | `{"_comment":"synthetic","instance_id":"o__r-1","model_name":"swe-agent-llama-70b","target":true,"exit_status":"submitted","generated_patch":"diff --git a/a.py b/a.py","trajectory":[{"role":"system","text":"You are a helpful assistant."},{"role":"user","text":"Fix the bug in a.py"},{"role":"ai","text":"Let me look.\n```\nls\n```"},{"role":"user","text":"a.py b.py"},{"role":"ai","text":"Done.\n```\nsubmit\n```"}]}` | system; user; assistant text "Let me look." + call `bash {"command":"ls"}` id `call_1`; tool `call_1` "a.py b.py"; assistant "Done." + call `bash {"command":"submit"}` id `call_2`; meta `instance_id`, `model_name`, `exit_status` | SWE-smith shape: `{"instance_id":"s-1","traj_id":"t1","model":"gpt-4o","resolved":true,"messages":[system "sys"; user with content `[{"type":"text","text":"Fix it"}]`; assistant with tool call `c1` `bash {"command":"ls"}`; tool `c1` "a.py"; assistant "Done"]}` (ids kept: `c1`) | 2 converted, 1 skipped (a nebius row with only system and user turns); 3 exchanges; `tool_map` is `default_tool_map()` |
| `openhands` | `{"_comment":"synthetic","instance_id":"oh-1","events":[{"id":0,"source":"user","action":"message","args":{"content":"List files"}},{"id":1,"source":"agent","action":"run","args":{"command":"ls"}},{"id":2,"source":"environment","observation":"run","cause":1,"content":"a.py"},{"id":3,"source":"agent","action":"finish","args":{"final_thought":"Done"}}]}` | user "List files"; assistant with call `execute_bash {"command":"ls"}` id `call_1`; tool `call_1` "a.py"; assistant "Done" | `oh-2`: a `think` action then a `run` (reasoning on the next assistant message), one observation, `finish` | 2 converted, 1 skipped (only a user message); 3 exchanges |

An `action: message` takes its text from `args.content`, then `args.message`, then the event's `message`, then `content`. Skip counts are per reason: `convert_rows` returns `skipped: [(reason, n)]`. With `--format auto` rows 3 of `sharegpt`, `wildchat`, `hermes` and `openhands` are skipped as
`no assistant turn`; a row that matches no shape is `unrecognized row shape`.

## 7. Structured output in the fallback (d)

`ir::schema::instance(schema, seed)`. Deterministic: same schema and seed, same value; no clock, no global RNG. Child seeds are derived from the parent seed and the property name or item index (splitmix64 over FNV, `fallback::hash64`) so adding a property does not change its siblings. Supported keywords and the
value rules:

- `const`; `enum` (element chosen by seed); `examples` (element chosen by seed) and `default`, in that order, when the schema declares them (a declared value is valid by definition, and what a person would expect to see); `$ref` to `#/definitions/<n>`, `#/$defs/<n>` or any JSON pointer into the root schema (other refs: treated as `{}`); `oneOf` / `anyOf`: the first branch; `allOf`: branches merged (`properties` union, `required` union, other keywords from the first that sets them).
- `type` string or list (first non-`null`); no `type` but `properties` -> object, `items` -> array, `enum`/`const` handled first, otherwise a short lorem word. `true` / `{}` -> a lorem word.
- `boolean`: from the seed; `null`: `null`.
- `integer` / `number`: `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum` (numeric or draft-4 boolean), `multipleOf`; default range `0..=100` (if only one bound is given, the range extends 100 from it); numbers have at most 2 decimals; the value is always inside the bounds (an empty range returns the lower bound).
- `string`: `minLength`, `maxLength` (default length 4..=24, lorem words joined with spaces, cut or extended with letters to fit); `format`: `date-time` (`2025-03-14T09:26:53Z`-style, plausible dates 2020-2029), `date`, `time`, `email` (`<word><n>@example.com`), `uri`/`url` (`https://example.com/<word>`), `uuid`, `hostname`, `ipv4`;
  `pattern`: supported for a small regex subset, generated by a hand-written parser (no new dependency): optional `^`/`$` anchors, literals and escapes, `.`, classes (`[a-z0-9_]`, negation, `\d \w \s`), groups with alternation `(a|b)`, quantifiers `? * + {n} {n,m}`
  (`*` and `+` generate 1 to 3 repetitions). The pattern wins over `minLength`/`maxLength` when both cannot hold. A pattern outside the subset (lookaround, backreferences, lazy quantifiers, Unicode classes) is ignored.
- `array`: `items` (object, or `prefixItems` / tuple list), count `minItems ..= max(minItems, 2)`, capped by `maxItems`; `uniqueItems` makes items distinct (retry with other child seeds, give up after 8 tries).
- `object`: all `required` properties; each optional property in `properties` included when `hash(seed, name)` is even (50%); `additionalProperties: false` is respected trivially (only declared properties are generated); extra properties `extra_1`.. are added only to satisfy `minProperties` when `additionalProperties` is not `false`.
- Recursion deeper than `MAX_DEPTH` (cycles through `$ref` included) generates the smallest value: required properties only, `minItems` items.
- `contains` (arrays): the first `max(minContains, 1)` items are generated from the `contains` schema, the rest from `items`; `dependentRequired`: when a property is present, the properties it requires are added too (generated from `properties`, or a lorem word).
- `not`: when its body is `const`, `enum` or `{type}`, the generated value is checked against it and retried with other child seeds (up to 32 tries); a more complex `not` is ignored.
- Limits (documented in the user docs): `if/then/else`, `patternProperties`, `propertyNames`, `unevaluated*`, `dependentSchemas`, `contentEncoding` and regexes outside the subset are ignored, so a strict validator can reject a value
  that uses them. The tests state the supported keyword set and check validity only for it.

Fallback integration (`src/fallback.rs`, (d)):

- `response_format: json_schema` (Chat) and `text.format: json_schema` (Responses) with a schema: the text is `serde_json::to_string(&instance(schema, seed))` with `seed = request_seed(req)`. `max_tokens` truncation behaves as before (a too-small cap yields `finish: length` and truncated, invalid JSON, like a real length stop). `json_schema` without a schema, and `json_object`, keep
  `{"message": <lorem>}`.
- A tool the client forces (`tool_choice` `Tool{name}` or `Required`; for Anthropic `{"type":"tool"}` / `{"type":"any"}`, which the frontend maps the same way) gets `arguments = to_string(instance(tool.parameters, seed ^ hash(name)))` when the tool has `parameters`; no `parameters`: `{}` as today. All three protocols.
- Token usage is computed by the engine from the text and arguments as before.
- Unit tests in `fallback.rs` (existing ones stay valid) and `ir/schema.rs`; `tests/schema.rs` drives all three protocols over HTTP and checks the generated value against the schema with a small validator written in the test (types, required, enum/const, bounds, lengths).

## 8. Snapshots (d)

`Control::snapshot(name)` / `restore(name)` (`src/snapshot.rs`). Captured: scripts with `hits`/`reached` (`Scripts::export_state`), trajectories with `hits`/`last_step`, policies (spec, `state`, `hits`, `restarts`; `Policies::export_state`), cassettes with consumed flags, hits and the recording (`Cassettes::export_state`), the stored
Responses objects, and the clock word (`Clock::export`). Not captured: the request log, other snapshots, the startup configuration, the id counter.

- `restore`: policies first (`Policies::import_state` = `prepare` with fresh dry-run subprocesses, `commit`, then the captured state/hits/restarts set; on failure return the error and change nothing), then scripts, trajectories, cassettes (`import_state` recompiles matchers), responses (`ResponseStore::import`), clock (`Clock::import`: frozen time is the same instant again, a running clock the same offset from the host clock).
  The snapshot is not consumed. `snapshot()` and `restore()` hold `Control::exclusive()` for their whole duration (they are the only control entry points that take it themselves, so library callers are covered); a restore therefore never interleaves with a PUT, a hot
  reload or another restore. The only slow, fallible step is `Policies::import_state`, which runs first (nothing has changed if it fails); after it come the synchronous steps `Scripts::import_state`, `Cassettes::import_state` (each validates and compiles before mutating its registry and cannot fail for
  state exported from a live registry), trajectories, responses, clock. Requests in flight are **not** held back: each registry swap is atomic on its own (one lock), but a request that straddles a restore may see old scripts and new cassettes for the instant between two swaps. Document: use between tests.
- Routes (done): `POST /_fakellm/snapshots` body `{"name"?: "..."}` (or `?name=`) -> 201 `SnapshotInfo{name, created, scripts, trajectories, policies, cassettes, responses}`; invalid name -> 400 (1-64 characters of `A-Za-z0-9._-`, `snapshot::valid_name`); taken name -> 409; none given -> `snapshot-<n>`. `GET /snapshots` -> `{"snapshots": [...]}` sorted by name; `GET /snapshots/{name}`;
  `POST /snapshots/{name}/restore` -> the info (404 when absent); `DELETE /snapshots/{name}`; `DELETE /snapshots`. `POST /reset?snapshots=true` deletes them (default keeps).
- `ResetOptions.snapshots` (default `false`). `created` is the server clock's time.
- Tests: take a baseline with rules, a trajectory mid-way, a once-cassette with one consumed exchange, a policy with state (Node only, skip otherwise) and a frozen clock; mutate everything; restore; assert counters, cursors, consumed flags, policy state, clock and stored responses are back; `reset` keeps the snapshot and `reset?snapshots=true` drops it; duplicate and invalid names; two restores of the same snapshot.

## 9. Hot reload (d)

`watch::spawn(control, WatchConfig)` (`src/watch.rs`); `Config.watch` (library default `None`), filled by `main.rs` from `--scripts`, `--trajectory`, `--policy`, `--cassette` unless `--no-watch`.

- Atomicity (decided). A reload holds `Control::exclusive()` end to end, so it never interleaves with a control-plane PUT/POST/DELETE or a restore. Inside it: (1) read and parse the files of every changed kind, compile rules and cassettes, and start the policies with `Policies::prepare` - slow and fallible, no
  registry touched; a kind that fails here is skipped with a warning and the others go on; (2) commit the prepared kinds back to back in one synchronous section with no `.await` (`Scripts::replace`, `Trajectories::replace`, `Cassettes::replace`, `Policies::commit`); each of those swaps its registry under a single lock (parse and validation happen before the lock, as `Scripts::replace`
  and `Policies::replace` already do), so no request ever sees half of a registry; across kinds the window between two swaps is microseconds and a request that straddles it may see new scripts with old cassettes (accepted, documented; it needs a request in flight during the reload).
  In-flight policy decisions: a policy that is replaced is closed, and a decision awaiting it fails as `fakellm_policy_crashed` (500, `Policies::decide`'s existing closed-channel path); it is not retried against the new subprocess. Requests that resolved before the swap finish with what they resolved.
- Poll every `POLL_INTERVAL` (500 ms, `tokio::time::sleep`, host clock) with `std::fs::metadata` (mtime and length) per file; no new dependency. The baseline is taken when the task starts (so edits between the binary's startup read and the baseline are missed: accepted).
- On a change of any file of a kind, read all files of that kind again and replace the whole registry: scripts `Scripts::replace` (`keep_hits` false), trajectories `Trajectories::replace`, policies `Policies::replace(...).await` (fresh subprocesses, state reset), cassettes `Cassettes::replace`. Rules or documents registered through the control plane since startup are replaced too (documented behavior). Formats by
  extension exactly as `main.rs` does. A kind with no files is never touched.
- Failure (file missing, unreadable, invalid): `tracing::warn!(target: "fakellm::watch")` with the file and error, registry unchanged; the stamp is recorded so the same content is not retried every tick; an I/O error (file briefly missing during an atomic save) does not record the stamp, so the next tick retries.
- Success: `tracing::info!(target: "fakellm::watch", "reloaded {kind} from {file}: {n} {unit}")`. The binary's default log filter shows it.
- Tests: write a temp file, start with `Config.watch`, rewrite it with different content, poll `GET /_fakellm/scripts` (real time, up to 5 s) until the new rule shows; a broken rewrite leaves the old rule; `Config.watch = None` never reloads; `--no-watch` through `tests/bin.rs`.

## 10. Publishing dry runs (d)

Nothing is published. `Cargo.toml`: add `description` (exists), `license`, `repository` (exist), `homepage`, `documentation` (docs.rs link), `readme = "README.md"`, `keywords` (at most 5: `llm`, `openai`, `testing`, `mock`, `anthropic`), `categories` (`development-tools::testing`, `web-programming::http-server`), `rust-version` if it builds on it, and `exclude`
(`/sdk`, `/bindings`, `/go`, `/e2e`, `/examples`, `/target`, `/docs/m*-spec.md`, `/.github`, `/Dockerfile`) so the package holds the crate, its tests and fixtures only (`tests/` needs `tests/fixtures`; keep it, but not `tests/node_sdk.rs` if it needs `sdk/node` - make it skip as it does today). Verify with `cargo publish --dry-run --allow-dirty` (offline index
may be needed: `--offline` where cargo supports it; if the registry is unreachable the dry run cannot complete: report that instead of faking it). Also `(cd sdk/js && npm pack --dry-run)` and `(cd bindings/node && npm pack --dry-run)` (fix `files`/`.npmignore` so no `node_modules`, `target`, `.node` debug builds of other platforms or tests ship unintentionally), and `(cd go && go vet ./...)`. Fix what they report; do not publish and do not tag.

## 11. Tests (e)

Written against this spec; tests whose subject is still a stub carry `#[ignore = "stub: <what>"]` and the coordinator removes the attribute at integration. Tests that already pass are not ignored. Keep every existing test file green.

- `tests/cassette.rs`: formats (JSON object, array, `{"cassettes"}`, YAML, JSONL header+exchanges, multi-cassette JSONL) and export round trip; CRUD routes, 400s (bad regex, unknown ignore entry, reserved name, trajectory document), 404s; `Config::cassettes`; `reset` clears. Replay: an exchange recorded as Chat answers Chat, Responses and Anthropic requests (all three renderings, sync and stream); ignore defaults (changed `temperature`, `max_tokens`, `stream`, `extra` still match;
  a changed message does not); `!max_tokens` makes it significant; the default normalizers (timestamp, UUID, `req_` id in a system or user text); a custom `normalize`; tool names only vs `match_tool_schemas`; tool call ids not compared; `model_glob` pinning; `fuzzy` (same count and last user, one earlier message differs) vs no fuzzy (falls to the default reply); `once` (two recorded
  exchanges for one request replay in order, third falls through); precedence (a script rule beats the cassette; strict mode errors only after the cassette missed); recorded errors replay with status, headers and `in_stream`; `?source=cassette`, `source.name/index` in the log; hits/consumed in `GET /cassettes`; cassette to trajectory (`to_trajectory` unit tests: side requests dropped, `with_expect`, errors) and `fakellm cassette to-trajectory` in `tests/bin.rs`.
- `tests/upstream.rs`: a **second fakellm instance as the upstream** (scripts on it give known replies, errors and streams): through Chat, Responses, Anthropic and embeddings, sync and streaming; the client sees the upstream's bytes unchanged (compare bodies/frames except `x-request-id`-like headers); the request log of the proxy shows `source: upstream` and the request log of the upstream shows exactly the client's request (byte-forward: same raw body, `authorization` passed through; with `upstream_api_key` it is replaced; Anthropic `x-api-key`);
  the recording (`GET /_fakellm/cassettes/recording`, and `--record FILE` JSONL parsed back) holds one exchange per call with the right reply (text, tool calls with fragmented arguments, reasoning, usage incl. cache tokens, finish reason), `meta.upstream_status` and `duration_ms`; replaying the recording on a third server (or the same one with the upstream stopped) answers equivalent replies; scripts on the proxy beat the upstream (fault injection: a script 429 with `times: 1`
  then the real answer); cassette beats upstream (hits do not reach it); upstream errors (429 with `retry-after`, 500) relayed and recorded as error exchanges; in-stream error; unreachable upstream -> 502 `fakellm_upstream_unreachable` in each protocol's format, logged, not recorded; Responses `previous_response_id` chain through the proxy (the second request's recorded IR has the history; a proxy restarted in between forwards with partial IR);
  control plane not forwarded; **loops**: two instances pointing at each other end in a 508 `fakellm_upstream_loop` within seconds (`tests/contract_m4.rs` holds the ignored case `a_cycle_of_instances_ends_in_a_quick_508`: move it here and remove the attribute at integration), a chain client -> A -> B -> C works, an upstream equal to the bound address is refused at start (done, in `contract_m4.rs`), a request that already carries `x-fakellm-hop` with the instance's id is refused;
  **credentials**: every row of the table in 4.2 (fakellm's own `--api-key` never reaches the upstream instance's log; `--upstream-api-key` replaces both client headers; both client headers pass through when no key is set; `anthropic-version` added when absent); client disconnect mid-stream logs `fakellm_upstream_incomplete` and records nothing; reassembly unit tests with hand-written chunk/event fixtures split at arbitrary byte boundaries. `FAKELLM_UPSTREAM_TEST` (an OpenAI-compatible URL, key in `FAKELLM_UPSTREAM_TEST_KEY`) runs one real round trip; skipped with a notice when unset.
- `tests/import.rs`: every converter on the fixtures in `tests/fixtures/hf/` (2-3 hand-written rows per format, marked synthetic in a `_comment`/file header: ShareGPT, WildChat, Hermes with `tools` and `<tool_call>` XML (both JSON and dict quoting), SWE-agent nebius and SWE-smith, OpenHands), asserting messages, tool calls/ids pairing, `meta`, skip counts and reasons; `detect`; outputs validate; a converted trajectory drives a request chain through the server; a converted cassette replays; `fetch_rows`
  against a small local HTTP server that mimics `/rows` and `/splits` (paging 100, truncated cells, 401, 404, `HF_TOKEN` header, `FAKELLM_HF_ENDPOINT`); `fakellm import file` end to end in `tests/bin.rs`; `FAKELLM_HF_TEST=1` fetches 5 rows of `allenai/WildChat-1M` for real and converts them (skipped with a notice otherwise).
- `tests/schema.rs`: `instance` unit-level properties over a corpus of schemas (determinism, seed sensitivity, validity by the test's validator for each keyword of section 7, `$ref` cycles, depth cap), and HTTP: Chat `json_schema`, Responses `text.format`, a forced tool in all three protocols, `json_object` unchanged, `max_tokens` truncation.
- `tests/snapshots.rs` (section 8), `tests/watch.rs` (section 9, with `tests/bin.rs` for the flags). `tests/watch.rs` also has **requests during a reload**: a background task sends requests continuously while the file is rewritten (scripts and a cassette file, a few times); every response is either the old or the new rule's answer (never a 5xx, never a mix inside one response), and the last ones are the new; a policy reload during in-flight decisions yields `fakellm_policy_crashed` for the in-flight ones only; a control-plane `PUT` racing a reload ends in one of the two states, never a merge. `tests/snapshots.rs` also restores while a `PUT /scripts` runs in parallel (serialized by the admin lock) and checks that a failed restore (policy runtime removed) changes nothing.
- Do not edit another suite's file except the listed additions.

## 12. Decisions beyond the brief

- Source order is scripts, cassettes, upstream, default/strict; a cassette hit is never recorded; relayed requests are logged when their body ends (once, with the reassembled outcome), not at the response head.
- `Config.upstream` is an `http::Uri` (no `url` crate). Not forwarded in upstream mode: models, `GET/DELETE /v1/responses/{id}`, `count_tokens`. IR parse errors are local 400s.
- Cassette `outcome` is the data part of `Outcome` (no `delivery`, no `source`), a full `Outcome` is read leniently. Per-exchange `ignore` is the brief's "request's ignore list". `!field` removes a default ignore; `field=value` conditional ignores; pseudo-fields `system` and `tools`; `previous_response_id` ignored by default; tool call ids and reasoning never compared; `developer` = `system`.
- The `recording` cassette is registered nowhere (never used for replay), reachable under that name through the routes, cleared by `reset`, snapshotted; `DELETE /cassettes` leaves it.
- Embeddings are forwarded and recorded (usage only) but never replayed from a cassette; their vectors are not stored.
- Responses objects forwarded from the upstream are stored locally under the upstream's id so `previous_response_id` chains work through the proxy; unknown ids are forwarded with a partial IR.
- Streams that end incomplete are relayed but not recorded; they are logged as `fakellm_upstream_incomplete`.
- `reply_spec` (cassette to trajectory) leaves prompt-side usage out: it is request-dependent and protocol-dependent (Anthropic's literal `input_tokens`).
- Replay applies no `max_tokens` truncation and no recorded timing (`duration_ms` is informational).
- `fakellm import file` and `fakellm cassette to-trajectory` are additions; SWE-agent conversations carry an explicit identity `tool_map` to edit.
- Hot reload replaces the whole registry of a kind (control-plane additions included) and restarts policies; the baseline race at startup is accepted.
- Snapshot restore also restores the clock; policies are restored first so a failed restore changes nothing. `ControlError::Conflict` (409) is new.
- `HttpClient` is public (`fakellm::HttpClient`) because the importer's `fetch_rows` takes it. `import` is a public module for the CLI and tests.
- Loop guard: `x-fakellm-hop` carries the chain of instance ids; an id seen again (or a chain of 8) is a 508 `fakellm_upstream_loop`; an upstream equal to the bound address is a start-up error. Credentials follow the table in 4.2; fakellm's own key never leaves the process.
- Registry replacement is one lock per registry after validation outside it; `Control::exclusive()` serializes control-plane writes, hot reload and snapshot restore; a policy replaced under an in-flight decision fails that decision as `fakellm_policy_crashed`. Cross-kind atomicity for in-flight requests is not promised (9).
- Default normalizers stay narrow (no bare dates); normalizers apply to tool results; recipes for the rest are documentation (3.3). The schema generator gained a regex subset for `pattern`, `contains`, `dependentRequired`, simple `not`, and uses `examples` and `default` (7).
- Not in M4: SDK helpers (`sdk/js`, `bindings/node`, Go) for cassettes, snapshots or the upstream; `ctl trajectories|policies`; corpus-sampling replay of imported conversations (design.md's "pick a reply from the dataset by seed"): imported cassettes replay by semantic match and `fuzzy`, not by sampling.

## 13. Notes for the docs pass

`docs/cassettes.md` (Chinese): recording with `--upstream`/`--record`, replay and the matching rules (3.2-3.4 as a user guide with the ignore table), the JSONL form, converting to a trajectory, Responses `previous_response_id` through the proxy, auth pass-through. `docs/import.md`: the formats and datasets table with licenses (design.md's), CLI, gated datasets and `HF_TOKEN`. `docs/scripts.md`: structured output in the default reply. `docs/control-plane.md`: cassette and
snapshot routes, `reset?snapshots=`, status fields, the 409. `README.md`: flags (`--cassette`, `--upstream`, `--upstream-api-key`, `--record`, `--no-watch`), hot reload, subcommands. `docs/cassettes.md` also documents the loop guard and the credential table (4.2). `TODO.md`: close the M4 roadmap items and `--scripts` hot reload; note the unverified Docker build with `ring`.
