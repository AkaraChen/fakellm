# M3 implementation spec (internal)

> Status: delivered. This is the implementation spec the milestone was built from, kept as history; the user docs (`docs/*.md`, Chinese) and the code are authoritative where they differ.

Audience: the agents implementing M3 in parallel. Read this, `docs/design.md` (fake agent section), `docs/consumers.md`,
`docs/m1-spec.md` (pipeline, ownership rules; they still apply) and the stub files you own. Everything marked
"decided" is settled; do not reopen it. Where this spec is silent, follow the nearest existing behavior and `docs/design.md`.

M3 is: the fake agent (trajectory replay with tool mapping, policy scripts), embeddings, and the chores from
`docs/consumers.md` / `TODO.md`. The tree at the start of the Implement phase already contains the IR and script
extensions, the engine glue, all routing and control-plane routes, `Config` fields and CLI flags, three embedded
policy shims (Node and Bun tested here, txiki.js not), and stubs with `todo!()` bodies. `cargo build --all-targets`,
`cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `(cd sdk/js && npm test)`,
`(cd bindings/node && npm run build:debug && npm test)` and `(cd go && go test ./...)` pass. Keep them passing.

## 1. Ownership

| Id | Owns (may edit) | Does not touch |
|---|---|---|
| (a) trajectories | `src/agent/trajectory.rs`, `src/agent/tool_map.rs` (stubs, delete `#![allow(dead_code, unused_variables)]` when no `todo!()` is left) | everything below |
| (b) policies | `src/policy/mod.rs`, `protocol.rs`, `runtime.rs`, `shim_node.js`, `shim_bun.js`, `shim_tjs.js` (tested for Node and Bun; keep the three in step) | |
| (c) chores | D1, D2, D5, D7 (`src/ir.rs` `last_user_text` only, `src/script.rs` matcher bits, `api_path` in `src/openai/mod.rs`, `src/anthropic/*`, `src/ctl.rs`); D3 is done (section 6) | |
| (d) embeddings + SO_LINGER | `src/openai/embeddings.rs`; D6 (`src/lib.rs` accept loop, `src/sse.rs`, `src/openai/mod.rs` only where D6 needs to pass the connection handle) | |
| (e) tests | `tests/agent.rs`, `tests/policy.rs`, `tests/embeddings.rs`, `tests/chores.rs` (new), additions to `tests/control.rs`, `tests/bin.rs`, `sdk/node/*.mjs` (openai embeddings), `sdk/js` types for the new rule fields | `src/` |

Rules for everybody (as in M1, adjusted):

- Frozen for everyone except where the table says otherwise: `src/ir.rs`, `src/lib.rs`, `src/control.rs`,
  `src/main.rs`, `Cargo.toml`, the routing in `src/openai/mod.rs`, the engine glue in `src/script.rs`. If you need a change
  there, stop and report it to the coordinator.
- Rust dependencies: only `Cargo.toml` as it is. No base64 crate, so embeddings writes its own encoder.
- Unit tests in the module's `#[cfg(test)] mod tests`; HTTP tests in `tests/*.rs` with `tests/common/mod.rs`. Use the
  virtual clock instead of sleeping, except where this spec says "host clock" (policy timeouts).
- No README/TODO/design edits and no `docs/agent.md` until the docs pass; put the facts that pass needs in your report.
  `docs/agent.md` (Chinese) documents section 4 and 5; it is written after Implement.
- Style as fakes3: `//!` doc explaining the why, comments only for non-obvious decisions, `cargo fmt` (max_width 140).
  Code and comments English, user docs Chinese. No benchmarks, no commits.

## 2. What the contract already contains

- `ir.rs`: `Protocol::OpenaiEmbeddings` (`"openai.embeddings"`); `Source::{Trajectory{index,name,trajectory,step},
  Policy{index,name,policy}}` and `Source::rule()` (index and rule name of any of the three script-derived sources; the request log's
  `source=script` filter and `rule=` filter see all three); `Request::last_user_text_raw()` and the (still stub) stripping
  `last_user_text()`; `ApiError` constructors `trajectory_diverged`, `trajectory_exhausted`, `unknown_tool`,
  `trajectory_not_found`, `policy_not_found`, `policy_error`, `policy_timeout`, `policy_crashed`.
- `script.rs`: `Match.last_user_raw` (wired), `StreamSpec.args_chunk_bytes` (wired, validated >= 1), `ReplySpec.{trajectory,policy}`
  with validation (section 3.1), `ReplySpec::with_defaults_from`, `validate_reply` / `validate_leaf_reply`, `Engine.{trajectories,policies}`,
  `Engine::resolve` (sync) / `Engine::resolve_async` (the server's entry) / `Engine::resolve_error` (embeddings), `Pattern::compile` is `pub(crate)`.
- `lib.rs`: `Config.{path_prefix, trajectories, policies, policy_runtime, policy_timeout_ms}`, `Server::{trajectories(), policies()}`,
  re-exports (`Trajectory`, `PolicySpec`, `Runtime`, ...), embedding models back in `DEFAULT_MODELS`.
  `Server::start` dry-runs `Config::policies` (only when non-empty).
- `control.rs`: all routes of sections 3.7 and 4.6, `route` is `async`, status JSON gains `trajectories` and `policies` counts,
  `reset` (with `scripts=true`) also clears trajectories and policies.
- `main.rs`: `--trajectory FILE` (repeatable; format by extension `.json`, `.jsonl`/`.ndjson`, `.yaml`/`.yml`, else auto),
  `--policy FILE` (repeatable, name = file stem), `--policy-runtime`, `--policy-timeout-ms`, `--path-prefix` (env `FAKELLM_*`).
- `openai/mod.rs`: `Route::Embeddings` (POST `/v1/embeddings`, `/embeddings`), body read and API-key check shared, `embeddings::respond` is
  called after them; `complete()` uses `resolve_async`; `Shared.path_prefix` and the `api_path()` hook (pass-through stub, D2).
- `agent/`, `policy/`, `openai/embeddings.rs`: the stubs. Types, serde shapes and constructors are final; bodies marked `todo!()` are yours.
  Things that must work without any stub already do: `Default` registries, `len/is_empty/clear`, `PolicySpec::parse_many/from_path`,
  `Runtime` parsing/shim choice, `ToolMap::client_name`.

## 3. Fake agent: trajectories (a)

A trajectory is one agent run as an ordered list of model turns, independent of provider. fakellm plays the model; the
client runs the tools and replays the conversation.

### 3.1 Document format (`kind: "trajectory"`)

```json
{
  "kind": "trajectory",
  "name": "fix-bug",
  "on_divergence": "strict",
  "stop_text": "Trajectory diverged; stopping.",
  "cursor": "by_turn",
  "unknown_tool": "error",
  "check_pairing": false,
  "tool_map": {"bash": {"name": "run_shell", "args": {"cmd": "$command"}, "keep_rest": false}},
  "meta": {"source": "swe-agent", "instance": "astropy-1234"},
  "steps": [
    {"reply": {"tool_calls": [{"name": "bash", "arguments": {"command": "ls"}, "id": "call_1"}]}},
    {"reply": {"text": "Done."},
     "expect": {"tool_results": [{"name": "bash", "id": "call_1", "contains": "Cargo", "is_error": false}], "any_user": "fix"}}
  ]
}
```

Types are in `src/agent/trajectory.rs` (`Trajectory`, `Step`, `Expect`, `ToolResultExpect`) and serialize exactly like this
(`deny_unknown_fields`; `kind` defaults to `trajectory`, always serialized first; defaults are omitted on output except
`on_divergence`, `cursor` and `unknown_tool`, which are always written so exports are explicit). `Trajectory::to_json` and
`parse_many` must round-trip: `parse_many(to_json(t)) == t`. This is also the cassette format of M4: do not rename fields.

- `step.reply` is a `ReplySpec`: text, tool_calls, reasoning, reasoning_opaque, usage, finish_reason, stream, delay_ms, `error`, `hang*`, `reset`.
  Validated with `script::validate_leaf_reply` at registration (no `trajectory`/`policy`). An `error` step is how a trajectory injects a fault at step N.
- `Expect` regexes are `Pattern`s (`"re"`, `"/re/"`, `"/re/i"`), compiled at registration; a bad regex is a registration error naming the step.
- `name`: non-empty, no `/`, unique. `steps`: at least one. Errors: `TrajectoryError("trajectory \"x\": step 2: ...")`, which the control plane
  turns into a 400.
- A rule attaches a trajectory with `reply: {trajectory: "name"}`; the name need not exist at rule registration (looked up per request,
  `fakellm_trajectory_not_found` otherwise). `ReplySpec` validation (done): `trajectory` excludes `text`, `tool_calls`, `reasoning`,
  `finish_reason`, `error` and `policy`; `stream`, `delay_ms`, `hang*`, `reset`, `usage` on the rule are defaults for steps that do not set their own
  (`ReplySpec::with_defaults_from`). Rules keep their own `match` and `times`; many rules may share a trajectory.

### 3.2 Import and export formats

- **JSON**: one document (above). `parse_many` also accepts an array of documents and `{"trajectories": [...]}`.
- **YAML**: the same structure.
- **JSONL**: one trajectory per file or stream. Header line: the document without `steps` (it must have `kind: "trajectory"` and `name`); then one line per
  step (`{"reply": {...}, "expect": {...}}`). Blank lines are ignored. A further header line starts the next trajectory, so a JSONL file may hold several.
  A line with `steps` is a JSON document, not a header, and is accepted too (JSONL of whole trajectories). `to_jsonl` writes header then steps, ending in `\n`.
- `TrajectoryFormat::Auto`: if the text has more than one non-empty line and every non-empty line parses as a JSON object, and the first has no `steps`, it
  is JSONL; else JSON if the first non-space character is `[` or `{`; else YAML.
- Control plane: `Content-Type: application/x-ndjson` (or `application/jsonl`, `application/json-lines`, `text/jsonl`) selects JSONL, YAML types select YAML, else Auto.
  Export with `GET /_fakellm/trajectories/{name}` (JSON) or `?format=jsonl` (`Content-Type: application/x-ndjson`).

### 3.3 Step selection (stateless)

`Trajectories::step(name, req)`:

1. Look up `name`; `ApiError::trajectory_not_found` when absent. Count a hit (`TrajectoryInfo.hits`) for every call, errors included.
2. `index = req.assistant_turns()` for `cursor: by_turn`. Conversations are independent, retried requests carry the same history and get the same
   step, parallel conversations do not interfere. Responses requests with `previous_response_id` work because the store prepends history.
   `cursor: by_hits`: `index` is the number of earlier `step` calls on this trajectory (counter per trajectory, restarts on re-registration and `reset`), for
   clients that send no history. Retries advance it; document that.
3. `check_pairing` (when true): every assistant tool call in the history must have a tool result with the same id somewhere after it; a missing one is a
   divergence ("tool call call_x has no result"). This is codex's `validate_request_body_invariants`.
4. If `index >= steps.len()`: `strict` -> `ApiError::trajectory_exhausted`; `lenient`/`stop` -> a reply with `text: stop_text`, finish `stop`, step index = `steps.len()`.
5. `steps[index].expect` is checked (section 3.4); a failure is handled by `on_divergence`:
   - `strict` -> `ApiError::trajectory_diverged(name, index, detail)` (400 `fakellm_error`, code `fakellm_trajectory_diverged`); `detail` lists each failed
     expectation as `expected <what>, got <what>` (include the got tool results' names, ids and the first 80 chars of their text).
   - `lenient` -> ignore the failure and answer the step.
   - `stop` -> a reply with `text: stop_text` (default `"Trajectory diverged; stopping."`), finish `stop`; the step index reported is `index`.
6. The step's reply gets the tool map applied (section 3.5) and is returned as `Stepped {step, reply}`. The engine fills the rest (ids, usage, delivery) and logs
   `Source::Trajectory`.

`strict` errors are 400 so SDKs do not retry them. They count in `hits` and `last_step`.

### 3.4 `expect`

Evaluated on the request that asks for step `index`, i.e. on the results of step `index - 1`'s calls.

- `tool_results`: each entry needs at least one trailing tool result (`Request::trailing_tool_results()`) on which every set field holds; results no entry
  mentions are ignored; each entry is matched to a result of its own (augmenting-path matching), so two entries need two results. Fields: `name` (the tool the result answers, written in trajectory terms: it matches when
  the result's name equals `tool_map.client_name(name)` or equals `name`), `id` (exact `tool_call_id`), `contains` (regex over the result text), `equals`
  (exact text), `is_error`.
- `any_user`: regex over the text of any user message with `<system-reminder>` blocks stripped.
- Step 0 normally has no `expect`; if `tool_results` is set there it can only fail.

### 3.5 Tool mapping (`ToolMap`)

`{"<trajectory tool>": {name, args?, keep_rest?}}` in `Trajectory.tool_map`. Applied to each of the step's `tool_calls` when it is rendered:

- Rename: the call's `name` becomes `mapping.name`. Tools without a mapping keep their name and arguments. The `id` is untouched.
- Arguments (`args` non-empty): the call's `arguments` (a JSON object; a string is parsed as JSON first) is rebuilt as an object with one entry per
  `args` key, in order. Template values: a string `"$field"` copies the trajectory argument `field` with its JSON type (`"$a.b"` descends into objects; missing source
  omits the argument); `"${field}"` inside a longer string interpolates the argument as text (strings unquoted, other values compact JSON, missing -> empty);
  `"$$"` is a literal `$`; every other value (non-strings, strings without `$`) is a literal. `keep_rest: true` appends the trajectory arguments no template
  copied, under their own names (after the mapped ones, mapped keys win). Without `args`, arguments pass through unchanged.
- Arguments that are a string that is not a JSON object cannot be remapped: renamed, passed verbatim.
- `unknown_tool` (per trajectory, default `error`) decides what happens when a step's call, **after mapping**, names a tool that is not in `req.tools` (an empty
  `tools` counts as nothing offered): `error` -> `ApiError::unknown_tool(name, step, tool, offered)` (400 `fakellm_unknown_tool`, message lists the client's tools);
  `stop` -> answer `stop_text` with finish `stop` instead of the step; `pass` -> send the call anyway. The check applies to the step being answered only.
  If the step has an `error` reply there are no calls to check.
- `ToolMap::validate`: every `name` non-empty. `expand_template` is exposed for unit tests.

### 3.6 Engine integration (done)

`Engine::resolve_matched` calls `Trajectories::step` for a rule with `trajectory`, merges `step.reply.with_defaults_from(rule.reply)`, then renders as any reply.
Errors become `Outcome::Error` with `Source::Trajectory{step: <the index that was tried>}` (the hit counter for `by_hits`). Do not touch it.

### 3.7 Control plane (done, `src/control.rs`)

| Route | Behavior |
|---|---|
| `GET /_fakellm/trajectories` | `{"trajectories": [TrajectoryInfo]}`: `name, steps, on_divergence, cursor, unknown_tool, hits, last_step` |
| `PUT /_fakellm/trajectories` | replace all; body: object, array, `{"trajectories": [...]}`, JSONL; response `{"added": n, "count": total}`; a 400 changes nothing |
| `POST /_fakellm/trajectories` | add; a same-named trajectory is replaced in place (its counters restart) |
| `DELETE /_fakellm/trajectories` | `{"cleared": true}` |
| `GET /_fakellm/trajectories/{name}[?format=jsonl]` | the trajectory document (404 `NotFound` when absent) |
| `DELETE /_fakellm/trajectories/{name}` | `{"deleted": true}` or 404 |

`POST /_fakellm/reset` (with `scripts=true`, the default) clears all trajectories. Names are percent-decoded.

### 3.8 Request log

`record_json.source` for a trajectory answer is `{"kind":"trajectory","index":i,"name":rule_name|null,"trajectory":"fix-bug","step":2}`; `rule` is the rule name; `?source=script` matches it.

## 4. Fake agent: policy scripts (b)

A JS module decides the reply from the request, for behavior that does not fit a fixed list ("read_file first, then edit_file; on a tool error retry once, then give up").

### 4.1 The module

```js
export default async function decide(request, context) {
  // request: the IR Request JSON as in GET /_fakellm/requests (messages, tools, model, ...), raw text, nothing stripped
  // context: { state, hits, log(...args) }
  return { tool_calls: [{ name: "read_file", arguments: { path: "src/a.rs" } }], state: { stage: "read" } };
}
```

- Default export is a function, sync or async, returning an object that is a `ReplySpec` (all `ReplySpec` fields) plus an optional `state`. A reply with a `state` field
  replaces the policy's state (no merging); without it the state is unchanged. `state` is JSON, initially `{}`.
- `context.hits` is the number of decisions this policy made before this one (0 on the first). `context.log(...)` writes one line to stderr.
- The reply may not contain `policy`; it may contain `trajectory` (the policy picks a trajectory per request). It is validated with `script::validate_reply`
  (a reply with neither content, error, hang nor trajectory is a `fakellm_policy_error`). The rule's delivery fields (`stream`, `delay_ms`, `hang*`, `reset`, `usage`)
  are defaults for the policy's reply.
- ES module syntax only. The source is written next to the shim in a temp dir as `policy.mjs`; relative imports of other files are not supported, `node:` / `bun:` builtins are.
- `console.log/info/debug` are redirected to stderr by the shims; stdout carries the protocol.

### 4.2 Registration

**Security:** registering a policy runs code, and `runtime` may be any executable path, on the unauthenticated control plane (only the `Origin` check applies). Documented in
`docs/control-plane.md` ("安全"); another reason not to bind `0.0.0.0`.

`PUT|POST /_fakellm/policies` (JSON or YAML; one `{name, source, runtime?}` object, an array, or `{"policies": [...]}`), `Config::policies`, `--policy FILE` (name = file stem).

- `name`: letters, digits, `.`, `_`, `-`; unique. `runtime`: `"tjs" | "node" | "bun" | "<path>"` (a path's shim follows its file stem: `tjs`, `bun`, else Node's).
- Default runtime: `Config::policy_runtime` (`--policy-runtime`), else the first of `tjs`, `node`, `bun` found on `PATH` (`runtime::detect_runtime()`), looked up at
  registration. No runtime available -> `PolicyError("no JavaScript runtime found: install node, bun or txiki.js (tjs), or pass runtime")`.
- Dry run at registration: write the temp dir, start the subprocess, wait for `{"ready":true}` (at most the policy timeout, host clock). `{"load_error": msg}` or an exit
  before ready -> `PolicyError("policy \"p\": <msg>")` -> 400. `replace` is all-or-nothing. The dry-run process becomes the live one.
- `Server::start` returns `InvalidInput` when a `Config::policies` module does not load.
- `PUT` replaces everything (states and subprocesses of removed policies are dropped); `POST` upserts by name (the replaced policy's state restarts).

### 4.3 Lifecycle and protocol

One subprocess per policy, started at registration (the dry run) and restarted lazily on the next decision after it died (counted in `restarts`). The temp dir
`fakellm-policy-<pid>-<n>` under `std::env::temp_dir()` holds the shim (`shim.mjs`, or `shim.js` for tjs) and `policy.mjs`; it is deleted when the policy is
removed, replaced, the registry cleared, or the server dropped. The child is spawned with `kill_on_drop`, `FAKELLM_POLICY_MODULE=<abs path of policy.mjs>`, stdin and stdout piped, stderr piped.

The wire protocol is in `src/policy/protocol.rs` (and exercised by the shims): newline-delimited JSON on stdout/stdin.

```
shim -> fakellm   {"ready":true} | {"load_error":"msg"}          once at start
fakellm -> shim   {"id":7,"request":<Request JSON>,"state":<JSON>,"hits":3}
shim -> fakellm   {"id":7,"reply":<ReplySpec JSON>,"state":<JSON>}   state only when the module returned one
                  {"id":7,"error":"msg"}
```

stderr lines are the policy's log: forwarded with `tracing::info!(target: "fakellm::policy", policy = name, ...)` and kept in a ring of the last 200
(`PolicyInfo.log`, `GET /_fakellm/policies/{name}`). Non-JSON lines on stdout are ignored with a `tracing::warn!`.

`Policies::decide(name, req)`: per-policy `tokio::sync::Mutex` (decisions of one policy are serialized, so `state` is consistent; different policies run in parallel);
assign an increasing `id`; write the line; read until the line with that `id` (stale ids from before a timeout are dropped).

| Situation | Result |
|---|---|
| unknown policy name | `policy_not_found` (400) |
| no answer within `policy_timeout_ms` (default 5000, **host clock**, not the virtual clock) | kill the process, `policy_timeout` (500 `fakellm_policy_timeout`); state and hits unchanged; next request restarts |
| process exits / broken pipe while deciding | `policy_crashed` (500); next request restarts |
| `{"id","error"}`, unparsable reply, reply not a valid `ReplySpec`, reply with `policy`, reply failing validation | `policy_error` (400 `fakellm_policy_error`, message includes the policy's message) |
| success | state replaced if present, `hits += 1` |

Failures do not count as hits. 500s are retried by SDKs and so call the policy again; that is why only the timeout and crash are 500.

### 4.4 Runtimes and shims

`src/policy/shim_node.js` (= `shim_bun.js`, byte-identical, keep them so) and `shim_tjs.js` implement the protocol. Node and Bun were run against `pol.mjs`-style modules
by hand (ready line, state, errors, stderr log, load errors). `shim_tjs.js` is **untested, no tjs binary here**; written from the txiki.js API
(`tjs.stdin.read`, `tjs.stdout.write`, `tjs.env`, `tjs.exit`, dynamic `import()`). If a tjs binary appears, run it first. `Runtime::args()`: `run` for tjs and bun, none for Node.
Tests may only assume Node (`which node`); skip with a printed notice when absent, like `tests/node_sdk.rs`.

### 4.5 Engine integration (done)

`Engine::resolve_async`: rule with `policy` -> `Policies::decide` -> `decided.with_defaults_from(rule.reply)` -> trajectory delegation if the policy named one ->
render. Errors become `Outcome::Error` with `Source::Policy`. Sync `Engine::resolve` answers a policy rule with a `fakellm_policy_error` ("needs resolve_async").

### 4.6 Control plane (done)

| Route | Behavior |
|---|---|
| `GET /_fakellm/policies` | `{"policies": [PolicyInfo]}`: `name, runtime, running, hits, restarts, state, log` |
| `PUT /_fakellm/policies` | replace all (dry run); `{"added": n, "count": total}`; 400 with the module's error, nothing changes |
| `POST /_fakellm/policies` | upsert |
| `DELETE /_fakellm/policies` | `{"cleared": true}` |
| `GET /_fakellm/policies/{name}`, `DELETE /_fakellm/policies/{name}` | `PolicyInfo` / `{"deleted": true}`; 404 when absent |

`reset` (scripts=true) clears all policies and kills their processes. `GET /_fakellm` status shows `policies.count`, `trajectories.count`.

## 5. Embeddings (d): `src/openai/embeddings.rs`

`POST /v1/embeddings` (also `/embeddings`, any prefix, section 6 D2). Body `{model, input, dimensions?, encoding_format?, user?}`.

- `input`: a string, an array of strings, an array of numbers (one pre-tokenized input), or an array of arrays of numbers. Validation (all 400 `invalid_request_error`, `param` set): missing `model`
  / `input` -> `ApiError::missing_parameter`; empty string, empty array, an array of more than `MAX_INPUTS` (2048) inputs, wrong element types -> `'$.input' is invalid. Please check the API reference: https://platform.openai.com/docs/api-reference.`
  (`param: "input"`); `dimensions` not an integer in `1..=MAX_DIMENSIONS` (8192) -> param `dimensions`; `encoding_format` not `float`/`base64` -> param `encoding_format`.
- Vector: `seed = FNV-1a64(model bytes, 0xff, input bytes)` (text: `T` + UTF-8; tokens: `K` + comma-joined decimal numbers), splitmix64 stream, first `dimensions` components as `f32` in `[-1, 1)`,
  L2-normalized (norm in `f64`). `dimensions` defaults to `DEFAULT_DIMENSIONS` (1536). Deterministic across runs and platforms; a shorter `dimensions` is the renormalized prefix; distinct inputs differ, equal inputs
  (same model) are identical, different models differ. Use `fallback::hash64`-style FNV constants (reuse `fallback::hash64` for the seed over the concatenated bytes if it is FNV-1a 64; check).
- Response: `{"object":"list","data":[{"object":"embedding","index":i,"embedding":[f32...]}],"model":<as sent>,"usage":{"prompt_tokens":n,"total_tokens":n}}`.
  `base64`: `embedding` is the standard padded base64 of the little-endian `f32` bytes. `usage`: sum of `input_tokens` (tokenizer count, min 1; token arrays: their length); `total_tokens == prompt_tokens`.
  All vectors in a batch have the same length. Headers as the other routes (`x-request-id`, `openai-processing-ms`, handled by `handle`).
- Request log: `Request{protocol: OpenaiEmbeddings, model, messages: [one User message per input element; token arrays as Part::Other{value}], extra: {dimensions?, encoding_format?}}`,
  the outcome is a `Reply` with `text: None`, `usage.prompt_tokens = n`, `Source::Fallback`; the vectors are not logged. `?protocol=openai.embeddings` filters it.
- Scripts: only rules whose reply has `error` apply (`Engine::resolve_error`), after the API-key check and parsing; the error is delayed by its `delay_ms` on the server clock and rendered
  as an OpenAI error (`error::render`); `times` and `every_nth` work as usual. Rules without `error` are skipped and keep their budget. Hang is not supported on this route. Strict mode does not apply.
- Models: `text-embedding-3-small` and `text-embedding-3-large` are in `DEFAULT_MODELS` again. Any model name is accepted, as for chat.
- `respond(shared, bytes, trace)` is the entry from `dispatch`; follow `count_tokens` in `openai/mod.rs` for the trace handling (`note_body`, `trace.seed`, `trace.request`, `trace.outcome`, `reject`).

## 6. Chores

### D1 (c): `last_user` ignores `<system-reminder>`

- `Request::last_user_text()` (the view `Match.last_user` uses): from the end, the first user message whose text, after removing every `<system-reminder>...</system-reminder>`
  block (non-greedy, across lines; an unclosed opening tag is left alone) and trimming, is non-empty; returns that stripped, trimmed text. `None` when there is none.
  `last_message_user_text()` strips the same way (and is `None` when the stripped text is empty).
- The IR keeps raw text. `Match.last_user_raw` (done) matches `Request::last_user_text_raw()`: the most recent user message, unstripped. `any_message` and `system` stay raw.
- Update the doc comment on `last_user_text`, `docs/scripts.md` is the docs pass.

### D2 (c): path tolerance

- `Config.path_prefix` / `--path-prefix /anthropic` (done) and the automatic rule are implemented in `openai/mod.rs::api_path(path, prefix) -> &str` (its doc comment is the contract, the stub is a
  pass-through). It returns a suffix of `path`. Order: (1) a configured prefix removes itself on a segment boundary (`/anthropic` and `/anthropic/`; the prefix is normalized to start with `/` and have no trailing `/`);
  (2) if the result is not already a known path (`/v1/...`, `/chat/completions`, ...), the shortest suffix, after one or more leading segments, that is exactly one of `/v1/messages`, `/v1/messages/count_tokens`, `/v1/chat/completions`,
  `/v1/responses`, `/v1/responses/<id>` (single segment), `/v1/models`, `/v1/models/<id...>`, `/v1/embeddings`, and also the same without `/v1`, is used. Paths under `/_fakellm` never reach it (handled before).
- The request log keeps the full path (`RequestRecord.path` is already built from the URI). Unknown paths still 404 `Invalid URL (METHOD /path)`; use the routed path in that message.
- Test: `POST /anthropic/v1/messages`, `POST /gateway/v2/proxy/v1/chat/completions`, `GET /x/v1/models`, `POST /foo/embeddings`; `POST /foo/bar` is a 404.

### D3: `StreamSpec.args_chunk_bytes` (done in the contract)

Field, validation (>= 1) and engine wiring (`delivery.args_chunk_bytes`) exist; the three frontends already take `delivery.args_chunk_bytes`. (e) adds the HTTP tests: with `args_chunk_bytes: 5` a 20-byte argument string
arrives as 4 fragments in all three protocols; it overrides `--args-chunk-bytes`; `0` is a registration error. Combined with `tokens_per_sec` it gives oar's slow tool arguments.

### D5 (c): Anthropic usage (done, kept as history)

Decision: Anthropic's real `usage.input_tokens` excludes cache reads and cache writes (`total = input_tokens + cache_read_input_tokens + cache_creation_input_tokens`), and
`count_tokens` counts the whole input. So fakellm keeps `Usage.prompt_tokens` as the total (cache included) and Anthropic renders `input_tokens = prompt - cached - cache_creation` when the number is derived (unchanged).
When a rule **sets** `usage.prompt_tokens`/`input_tokens`, the value is applied literally: `Engine::answer` must make a rule that says `input_tokens: 20236` produce `input_tokens: 20236` on the wire even with
`cached_tokens: 5`, by storing `prompt_tokens = input_tokens + cached + cache_creation` in the IR for Anthropic requests (so `usage_json`'s subtraction yields the literal). For Chat and Responses the
override stays the total as before. `count_tokens`: `Engine::count_input_tokens` returns the rule's value literally (20236), unchanged; without a rule it returns the tokenizer total. Update the comment on
`anthropic::count_tokens` accordingly. Docs pass: replace the paragraph in `docs/scripts.md` that describes the subtraction and close the TODO item.
Tests: rule `{input_tokens: 20236, cached_tokens: 5}` -> `input_tokens: 20236`, `cache_read_input_tokens: 5` (sync and stream); the request log shows `prompt_tokens: 20241`; derived case unchanged.

### D6 (d): connection reset with RST (done, kept as history)

Investigation result to confirm and then implement if it holds: tokio's `TcpStream::set_linger` (check it exists and is not removed in tokio 1.54; otherwise `TcpSocket`) sets `SO_LINGER{on, 0}`,
which makes `close` send RST. The socket is owned by hyper through `TokioIo`, so the stream's body task cannot reach it, but a duplicate can: in the accept loop do `let std = socket.into_std()?; let handle = std.try_clone()?;
let socket = TcpStream::from_std(std)?` and keep `handle` (wrapped in `tokio::net::TcpStream::from_std` or used via the std type's raw socket option route allowed by the code base: no new deps, no `unsafe`) in an `Arc` put into
the request extensions (`req.extensions_mut().insert(...)` in the `service_fn`). `Shared::handle` passes it to `Delivery`-driven code; when a reset fires (`reset_after_chunks`, `reset: true`, the sync half-body reset), set linger 0
on the handle just before the body yields its error, so hyper's close sends RST. Setting it on every socket at accept time is wrong (it would RST normal closes and lose data). If the clone/extension route is not workable
without a fork or `unsafe`, document why in a paragraph of `docs/m3-spec.md` section 9 and skip; do not leave half of it. Verify with a test that reads the client side error kind (`ConnectionReset`) on Linux; gate the assertion
on `cfg(target_os = "linux")`.

### D7 (c): `ctl requests list` marks hung requests (done, kept as history)

In `src/ctl.rs` the text table shows `hung` in the status column (instead of `0`) for records with `hung: true`; `--json` already has the field. Add a unit test in `ctl.rs` and an assertion in `tests/bin.rs` if a hang is cheap to arrange.

## 7. Tests (e)

Written against this spec; tests whose subject is still a stub carry `#[ignore = "stub: <what>"]` and the coordinator removes the attribute at integration. Tests that already pass (D1 aside) are not ignored.

- `tests/agent.rs`: registration formats (JSON object, array, `{"trajectories"}`, YAML, JSONL header+steps, multi-trajectory JSONL) and export round-trip; CRUD routes and 404/400s; `Config::trajectories`; `reset` clears.
  Selection: three-step tool/tool/text trajectory driven through Chat, Responses and Anthropic requests built by hand (assistant messages replayed); two parallel conversations interleaved; a retried identical request gets the same step;
  `by_hits`. `expect`: tool_results by name/id/contains/equals/is_error, `any_user`, strict 400 body (code `fakellm_trajectory_diverged`, message lists expected vs got), lenient, stop. Exhausted: strict 400, lenient/stop stop_text. `check_pairing`.
  Tool map: rename, `$field`, `${field}`, `$$`, literal, `keep_rest`, string arguments, tool without mapping; `unknown_tool` error (message lists the client's tools) / stop / pass. Error and hang steps; step `usage` and `stream` overrides; rule-level `stream` default. `?source=script` and
  `source.step` in the request log. `ToolMap` and `expand_template` unit tests live in `tool_map.rs`.
- `tests/policy.rs` (needs Node; skip with a notice otherwise): PUT a policy (JSON and YAML) and drive it through a rule; read_file -> edit_file -> retry-once-on-error -> give up as in the docs example; `state` persists across requests and across a killed process
  (`DELETE`-less crash: the module calls `process.exit(1)` on a trigger input; next request is fine, `restarts == 1`); `context.hits`; `log` appears in `GET /policies/{name}`; throws -> 400 `fakellm_policy_error` with the message;
  timeout (`policy_timeout_ms: 300`, module awaits a long promise) -> 500 `fakellm_policy_timeout` and the next request works; module syntax error and missing default export -> 400 at registration, nothing changed; unknown `runtime` path -> 400; reply with `policy` -> `fakellm_policy_error`;
  policy returning `trajectory`; rule delivery defaults; parallel requests serialized; `--policy FILE` and `--policy-runtime` through `tests/bin.rs`; `Config::policies`; reset kills the process (the temp dir is gone).
- `tests/embeddings.rs`: shape, determinism, distinctness, `dimensions` prefix property, base64 decode equals the float vector (decode in the test), batch lengths, token-array inputs, usage invariants (xsai's: `prompt_tokens > 0`, `total >= prompt`), validation 400s (each case of section 5), `error` rule with `times`, request log record and `protocol` filter, model listing,
  API key check, `/embeddings` and prefixed paths, openai SDK `client.embeddings.create` in `sdk/node/openai.test.mjs`.
- `tests/chores.rs`: D1 with a Claude Code shaped Anthropic request (reminder-only user message after tool results), D2, D3 (three protocols), D5, D6 (Linux), D7 where cheap.
- Do not edit another suite's file except the listed additions; keep the existing 11 test files green.

## 8. Decisions beyond the brief

- `Source` gained `Trajectory` and `Policy` variants rather than overloading `Script`, so the request log can say which step answered. The filter vocabulary is unchanged (`source=script` matches all three).
- `Engine::resolve_async` is the server's entry; the sync `resolve` stays so the existing engine tests and embedders keep working (a policy rule is an error there).
- `trajectory`/`policy` are not checked against registered names at rule registration (order of registration should not matter); missing names are 400s per request.
- Trajectory `check_pairing`, `meta`, `keep_rest` and `ToolResultExpect.is_error` are additions; all optional.
- Registration is by name with `POST` upserting; `GET /trajectories/{name}` is the export route (JSON or JSONL).
- Policies: stderr is the log channel (ring of 200 lines in `PolicyInfo`); the module path travels in `FAKELLM_POLICY_MODULE`; `policy_error` is 400 (do not retry a buggy policy), timeout and crash are 500; a policy may return `trajectory`.
- Embeddings consult only `error` rules (no hang), so retry/limit handling can be tested; vectors cannot be scripted.
- `Config::policies` are dry-run in `Server::start` (async), not in `Control::new`; `reset` clears Config-supplied trajectories and policies like scripts.
- The error message of an invalid reply now reads "reply needs `text`, `tool_calls`, `error`, `trajectory` or `policy`" (`docs/control-plane.md` shows the old text; docs pass).
- Not in M3's Implement phase: `fakellm ctl trajectories|policies` subcommands, napi/Go/JS SDK helpers for the new routes (the SDKs' rule types should gain `trajectory`/`policy`/`last_user_raw`/`args_chunk_bytes`; (e) does the `sdk/js` types), `fakellm import hf`, cassettes (M4).

## 9. Notes for the docs pass

- `docs/agent.md` (Chinese): trajectory format and tools (sections 3.1-3.5), the policy contract (section 4) with the "read_file first, then edit_file; on a tool error retry once then give up" example, runtimes and their caveats, error codes.
- `docs/scripts.md`: `last_user` / `last_user_raw`, `trajectory` / `policy` replies, `stream.args_chunk_bytes`, the Anthropic usage paragraph (D5), path prefix.
- `docs/control-plane.md`: the new routes and status fields, the new error text; `README.md` flags (`--trajectory`, `--policy`, `--policy-runtime`, `--policy-timeout-ms`, `--path-prefix`), embeddings; `TODO.md`: close the consumer items done here, note SO_LINGER outcome.
