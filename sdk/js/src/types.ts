/**
 * Types mirroring the JSON documents of the script format (docs/scripts.md) and the control plane
 * (docs/control-plane.md). Field names are exactly the server's; unknown fields are rejected by it.
 */

export type Protocol = 'openai.chat' | 'openai.responses' | 'anthropic.messages' | 'openai.embeddings';

/** A regex-or-literal pattern string as understood by the server (see docs/scripts.md). */
export type Pattern = string;

export interface Match {
  model?: string;
  last_user?: Pattern;
  system?: Pattern;
  any_message?: Pattern;
  has_tool?: string;
  no_tools?: boolean;
  has_tool_result?: boolean;
  last_tool_result?: string;
  tool_result_contains?: Pattern;
  last_tool_call_id?: string;
  stream?: boolean;
  turn?: number;
  any?: boolean;
  protocol?: Protocol;
  probability?: number;
  every_nth?: number;
}

export interface ToolCallSpec {
  name: string;
  arguments?: unknown;
  id?: string;
}

export interface ErrorSpec {
  status: number;
  type?: string;
  code?: string;
  message?: string;
  param?: string;
  retry_after?: number;
  in_stream?: boolean;
  headers?: Record<string, string>;
}

export interface StreamSpec {
  ttft_ms?: number;
  tokens_per_sec?: number;
  chunk_tokens?: number;
  cut_after_chunks?: number;
  reset_after_chunks?: number;
}

export interface UsageSpec {
  prompt_tokens?: number;
  completion_tokens?: number;
  cached_tokens?: number;
  reasoning_tokens?: number;
  cache_creation_tokens?: number;
  /** Aliases accepted by the server. */
  input_tokens?: number;
  output_tokens?: number;
  cache_read_tokens?: number;
  cache_creation_input_tokens?: number;
}

export type FinishReason = 'stop' | 'length' | 'tool_calls' | 'content_filter';

export interface Reply {
  text?: string;
  tool_calls?: ToolCallSpec[];
  reasoning?: string;
  error?: ErrorSpec;
  finish_reason?: FinishReason;
  stream?: StreamSpec;
  delay_ms?: number;
  hang?: boolean;
  hang_ms?: number;
  reasoning_opaque?: string;
  usage?: UsageSpec;
  /** Abort the connection mid-body instead of replying (FIN, no terminating chunk; headers and 200 are already sent). */
  reset?: boolean;
  /** Answer with the next step of this registered trajectory. Excludes the content fields. */
  trajectory?: string;
  /** Let this registered policy decide the reply. Excludes the content fields. */
  policy?: string;
}

export interface Rule {
  name?: string;
  match?: Match;
  reply: Reply;
  times?: number;
}

/** A rule as returned by `scripts.list()`. */
export interface RuleInfo extends Rule {
  index: number;
  hits: number;
  reached: number;
  exhausted: boolean;
}

/** What a step requires of the tool results the client sent back. Every set field must hold on the same result. */
export interface ToolResultExpect {
  /** Tool name in trajectory terms (the tool map is applied before comparing). */
  name?: string;
  id?: string;
  /** Regex over the result text. */
  contains?: Pattern;
  equals?: string;
  is_error?: boolean;
}

export interface Expect {
  tool_results?: ToolResultExpect[];
  /** Regex over the text of any user message. */
  any_user?: Pattern;
}

/** One model turn: a leaf reply (no `trajectory` / `policy`) and what the request asking for it must look like. */
export interface Step {
  reply: Reply;
  expect?: Expect;
}

/** `{ "<trajectory tool>": { name, args?, keep_rest? } }`; an arg template `"$cmd"` copies, `"${cmd}"` interpolates. */
export type ToolMap = Record<string, { name: string; args?: Record<string, unknown>; keep_rest?: boolean }>;

export interface Trajectory {
  kind?: 'trajectory';
  /** Unique, no `/`. */
  name: string;
  steps: Step[];
  /** Default `strict` (400 on mismatch). */
  on_divergence?: 'strict' | 'lenient' | 'stop';
  stop_text?: string;
  /** `by_turn` (default) follows the replayed assistant messages; `by_hits` counts requests. */
  cursor?: 'by_turn' | 'by_hits';
  tool_map?: ToolMap;
  /** What to do with a step whose tool the client did not offer. Default `error`. */
  unknown_tool?: 'stop' | 'error' | 'pass';
  check_pairing?: boolean;
  meta?: Record<string, unknown>;
}

/** A trajectory as returned by `trajectories.list()`. */
export interface TrajectoryInfo {
  name: string;
  steps: number;
  on_divergence: 'strict' | 'lenient' | 'stop';
  cursor: 'by_turn' | 'by_hits';
  unknown_tool: 'stop' | 'error' | 'pass';
  hits: number;
  last_step: number | null;
}

/** A request in the IR (`requests.list()[i].request`); only `model` and `messages` are required in a cassette. */
export type IRRequest = { model: string; messages: Record<string, any>[] } & Record<string, any>;

/** The data of a recorded call: a reply or an error, in the shape of the IR outcome without delivery and source. */
export type CassetteOutcome =
  | { outcome: 'reply'; reply: Record<string, any> }
  | { outcome: 'error'; error: Record<string, any> };

/** One recorded round trip. */
export interface Exchange {
  request: IRRequest;
  outcome: CassetteOutcome;
  /** The protocol it was recorded under; replay works across protocols. */
  protocol?: Protocol;
  meta?: Record<string, unknown>;
  /** Request fields to ignore for this exchange, on top of the cassette's list. */
  ignore?: string[];
}

/** A recording: request/outcome pairs in IR form. Replays through every protocol. */
export interface Cassette {
  kind?: 'cassette';
  /** Unique, no `/`, not `recording`. */
  name: string;
  recorded_at?: string;
  upstream?: string;
  /** Request fields to ignore when matching, added to the defaults; `!field` removes a default. */
  ignore?: string[];
  /** Regex substitutions applied to every text of a request before comparing. */
  normalize?: { pattern: string; replace?: string }[];
  /** `false` leaves the default normalizers (timestamps, UUIDs, request ids) out. */
  normalize_defaults?: boolean;
  /** Compare tool parameter schemas too, not just tool names. */
  match_tool_schemas?: boolean;
  /** Glob over the request's model. Default `*`. */
  model_glob?: string;
  /** Answer with the nearest exchange when none matches exactly. */
  fuzzy?: boolean;
  /** Each exchange answers once, in order; a spent one is skipped. */
  once?: boolean;
  meta?: Record<string, unknown>;
  exchanges: Exchange[];
}

/** A cassette as returned by `cassettes.list()`; the in-memory recording of an `upstream` comes last with `recording: true`. */
export interface CassetteInfo {
  name: string;
  exchanges: number;
  once: boolean;
  fuzzy: boolean;
  hits: number;
  consumed: number;
  recording: boolean;
}

/** A snapshot as returned by `snapshots.create()`, `list()` and `restore()`; the numbers count what it holds. */
export interface SnapshotInfo {
  name: string;
  /** RFC 3339, server clock. */
  created: string;
  scripts: number;
  trajectories: number;
  policies: number;
  cassettes: number;
  responses: number;
}

export type PolicyRuntime = 'tjs' | 'node' | 'bun' | (string & {});

/** A policy script: an ES module whose default export is `decide(request, context)`. */
export interface Policy {
  /** Letters, digits, `.`, `_`, `-`. */
  name: string;
  source: string;
  /** Overrides the server's policy runtime for this policy. */
  runtime?: PolicyRuntime;
}

/** A policy as returned by `policies.list()`. */
export interface PolicyInfo {
  name: string;
  runtime: string;
  running: boolean;
  hits: number;
  restarts: number;
  state: unknown;
  log: string[];
}

export interface ClockState {
  /** RFC 3339, millisecond precision, server time. */
  now: string;
  frozen: boolean;
  shifted: boolean;
}

export interface Status {
  version: string;
  clock: ClockState;
  scripts: { count: number };
  trajectories: { count: number };
  policies: { count: number };
  cassettes: { count: number };
  snapshots: { count: number };
  /** `null` unless started with `upstream`. */
  upstream: { url: string; record: string | null; recorded: number } | null;
  responses: { count: number };
  strict: boolean;
  request_log: LogConfig;
}

/** Request log settings and occupancy (`GET /_fakellm/requests/config`). */
export interface LogConfig {
  enabled: boolean;
  capacity: number;
  stored: number;
  /** Requests that fell off the end because the log was full, since the last clear. */
  dropped: number;
}

export interface ResetOptions {
  /** Each defaults to true; pass false to keep that part. */
  clock?: boolean;
  requests?: boolean;
  scripts?: boolean;
  responses?: boolean;
  /** Default false: snapshots survive a reset, so "snapshot once, reset or restore per test" works. */
  snapshots?: boolean;
}

export interface RequestFilter {
  /** Glob with `*` and `?`. */
  model?: string;
  has_tool?: string;
  stream?: boolean;
  protocol?: Protocol;
  /** Exact (`404`, `0`), class (`'4xx'`) or `'error'`. */
  status?: number | string;
  source?: 'script' | 'fallback' | 'strict' | 'rejected';
  rule?: string;
  /** Path prefix. */
  path?: string;
  /** RFC 3339 server time, or a Date. */
  since?: string | Date;
  /** Only records with `seq` greater than this. */
  after?: number;
  /** Keep only the latest N matches. */
  limit?: number;
  /** Include `headers`, `raw` and `raw_text`. */
  full?: boolean;
}

export type Source = { kind: 'script'; index: number; name?: string | null } | { kind: 'fallback' } | { kind: 'strict' } | { kind: 'rejected' };

export interface RequestRecord {
  seq: number;
  time: string;
  method: string;
  path: string;
  status: number;
  hung?: boolean;
  latency_us: number;
  protocol: Protocol | null;
  model: string | null;
  stream: boolean;
  source: Source;
  rule: string | null;
  /** Parsed request (the server's IR); null when parsing failed. */
  request: Record<string, any> | null;
  /** The decided reply; shape differs between success and error replies. */
  reply: Record<string, any> | null;
  /** Only with `full`. */
  headers?: [string, string][];
  raw?: unknown;
  raw_text?: string;
}

export interface RequestList {
  requests: RequestRecord[];
  /** Matches before `limit`. */
  total: number;
  dropped: number;
}

export interface WireOptions {
  /** `json` (default, parsed), or `env` / `toml` / `shell` which return the text. */
  format?: 'json' | 'env' | 'toml' | 'shell';
}

export interface WireOutput {
  env: Record<string, string>;
  files: Record<string, string>;
  snippet: string;
}

export interface StartOptions {
  /** Path of the fakellm binary; else `$FAKELLM_BIN`, else `fakellm` on PATH. */
  bin?: string;
  host?: string;
  /** 0 (default) picks a free port. */
  port?: number;
  apiKey?: string;
  models?: string[];
  strict?: boolean;
  /** Rules to load at start, or the path of a JSON/YAML script file. */
  scripts?: Rule[] | string;
  /** Trajectories registered at start. */
  trajectories?: Trajectory[];
  /** Cassettes loaded at start: objects, or the path of a JSON / JSONL / YAML cassette file. */
  cassettes?: Cassette[] | string;
  /** Relay requests no rule and no cassette answers to this OpenAI-compatible or Anthropic server, and record what it says. */
  upstream?: string;
  /** Key sent to the upstream instead of the client's own credentials. Needs `upstream`. */
  upstreamApiKey?: string;
  /** Append every recorded exchange to this JSONL file. Needs `upstream`. */
  record?: string;
  /** `false` stops reloading `scripts` / `cassettes` files when they change (default: watched). */
  watch?: boolean;
  /** Policies registered at start; a module that does not load makes `start()` reject. */
  policies?: Policy[];
  /** Runtime for policies that name none: `tjs`, `node`, `bun` or a path. Default: tjs, node, bun, whichever is on PATH first. */
  policyRuntime?: PolicyRuntime;
  /** Also serve the API under this prefix, e.g. `/anthropic`. */
  pathPrefix?: string;
  /** Default time to first token: milliseconds, or text such as `'300ms'`. */
  ttft?: number | string;
  tokensPerSec?: number;
  /** Length of the generated fallback text, in tokens. Default 32. */
  fallbackTokens?: number;
  /** Tokens per streamed content chunk. Default 1. */
  chunkTokens?: number;
  /** Bytes of tool-call arguments per streamed delta. Default 16. */
  argsChunkBytes?: number;
  /** Record API requests for `requests`. Default true. */
  requestLog?: boolean;
  /** How many requests the log keeps; the oldest are dropped first. */
  requestLogCapacity?: number;
  /** Store `"<redacted>"` instead of credential header values in the request log. */
  redactAuth?: boolean;
  /** Extra command line arguments. */
  args?: string[];
  env?: Record<string, string>;
  /** Default 10000. */
  readyTimeoutMs?: number;
}
