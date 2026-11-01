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
