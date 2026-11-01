/**
 * Spawns the fakellm binary and wraps its control plane (/_fakellm) so that any JS/TS test runner
 * can use it without native addons. All HTTP goes through global fetch.
 */
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import type {
  ClockState,
  RequestFilter,
  RequestRecord,
  ResetOptions,
  Rule,
  RuleInfo,
  StartOptions,
  Status,
  WireOptions,
  WireOutput,
  Reply,
  ErrorSpec,
  LogConfig,
  Policy,
  PolicyInfo,
  Trajectory,
  TrajectoryInfo,
} from './types.js';

export * from './types.js';

/** A failed control-plane call or server start. `status` is the HTTP status (0 when there was none). */
export class FakeLLMError extends Error {
  constructor(
    message: string,
    readonly status: number = 0,
  ) {
    super(message);
    this.name = 'FakeLLMError';
  }
}

const durationMs = (by: string | number) => (typeof by === 'number' ? `${Math.round(by * 1e6)}ns` : by);
const iso = (at: Date | string) => (at instanceof Date ? at.toISOString() : at);

function query(params: Record<string, unknown>): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined) q.set(k, v instanceof Date ? v.toISOString() : String(v));
  }
  const s = q.toString();
  return s ? `?${s}` : '';
}

/** Cleanups of the servers still running; one `exit` listener serves all of them, however many are alive. */
const live = new Set<() => void>();
function onProcessExit() {
  for (const cleanup of live) cleanup();
}

export class FakeLLM {
  readonly url: string;
  /** For OpenAI SDKs: `url + '/v1'`. */
  readonly baseURL: string;
  /** For the Anthropic SDK, which appends `/v1/messages` itself. */
  readonly anthropicBaseURL: string;
  readonly port: number;
  #child?: ChildProcess;
  #bin?: string;
  #tmp?: string;
  #stopped?: Promise<void>;
  #onExit?: () => void;

  /** @internal use `start()` or `connect()`. */
  constructor(url: string, child?: ChildProcess, bin?: string, tmp?: string) {
    this.url = url.replace(/\/+$/, '');
    this.baseURL = `${this.url}/v1`;
    this.anthropicBaseURL = this.url;
    this.port = Number(new URL(this.url).port);
    this.#child = child;
    this.#bin = bin;
    this.#tmp = tmp;
    if (child) {
      // A runner that dies or forgets stop() must not leave the server behind.
      this.#onExit = () => {
        child.kill();
        if (tmp) rmSync(tmp, { recursive: true, force: true });
      };
      if (live.size === 0) process.on('exit', onProcessExit);
      live.add(this.#onExit);
      // Not ref'd, so a test that fails before stop() cannot keep the runner alive; stop() refs it again to await the exit.
      child.unref();
      (child.stdout as unknown as { unref?: () => void } | null)?.unref?.();
    }
  }

  /** Raw control-plane call: `path` is relative to `/_fakellm`. Returns parsed JSON (null for an empty body). */
  async ctl(method: string, path: string, body?: unknown, headers: Record<string, string> = {}): Promise<any> {
    let res: Response;
    try {
      res = await fetch(`${this.url}/_fakellm${path}`, {
        method,
        headers: { 'content-type': 'application/json', ...headers },
        body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body),
      });
    } catch (e) {
      throw new FakeLLMError(`${method} ${path}: ${(e as Error).message}`);
    }
    const text = await res.text();
    if (!res.ok) {
      let msg = text;
      try {
        msg = JSON.parse(text).error.message;
      } catch {}
      throw new FakeLLMError(`${method} ${path} -> ${res.status}: ${msg}`, res.status);
    }
    return text ? JSON.parse(text) : null;
  }

  readonly scripts = {
    /** Replaces all rules. Hit counts restart unless `keepHits`. */
    put: async (rules: Rule[], opts: { keepHits?: boolean } = {}): Promise<{ added: number; count: number }> =>
      this.ctl('PUT', `/scripts${query({ keep_hits: opts.keepHits ? true : undefined })}`, rules),
    add: async (rules: Rule[]): Promise<{ added: number; count: number }> => this.ctl('POST', '/scripts', rules),
    list: async (): Promise<RuleInfo[]> => (await this.ctl('GET', '/scripts')).rules,
    clear: async (): Promise<void> => void (await this.ctl('DELETE', '/scripts')),
  };

  readonly trajectories = {
    /** Replaces all trajectories; hit counters restart. */
    put: async (items: Trajectory[]): Promise<{ added: number; count: number }> => this.ctl('PUT', '/trajectories', items),
    /** Adds trajectories; one with a registered name replaces it in place. */
    add: async (items: Trajectory[]): Promise<{ added: number; count: number }> => this.ctl('POST', '/trajectories', items),
    list: async (): Promise<TrajectoryInfo[]> => (await this.ctl('GET', '/trajectories')).trajectories,
    clear: async (): Promise<void> => void (await this.ctl('DELETE', '/trajectories')),
  };

  readonly policies = {
    /** Replaces all policies; each module is loaded first and a load error rejects without changing anything. */
    put: async (items: Policy[]): Promise<{ added: number; count: number }> => this.ctl('PUT', '/policies', items),
    add: async (items: Policy[]): Promise<{ added: number; count: number }> => this.ctl('POST', '/policies', items),
    list: async (): Promise<PolicyInfo[]> => (await this.ctl('GET', '/policies')).policies,
    clear: async (): Promise<void> => void (await this.ctl('DELETE', '/policies')),
  };

  readonly requests = {
    list: async (filter: RequestFilter = {}): Promise<RequestRecord[]> => (await this.ctl('GET', `/requests${query({ ...filter })}`)).requests,
    /** Number of matches (before `limit`). */
    count: async (filter: RequestFilter = {}): Promise<number> =>
      (await this.ctl('GET', `/requests${query({ ...filter, limit: 0 })}`)).total,
    clear: async (): Promise<void> => void (await this.ctl('DELETE', '/requests')),
    /** The log's settings and occupancy; with `set`, changes `enabled` and/or `capacity` first. */
    config: async (set?: { enabled?: boolean; capacity?: number }): Promise<LogConfig> =>
      set ? this.ctl('PUT', '/requests/config', set) : this.ctl('GET', '/requests/config'),
    /** Requests that fell off the end of the log because it was full. */
    dropped: async (): Promise<number> => (await this.ctl('GET', '/requests/config')).dropped,
  };

  readonly clock = {
    now: async (): Promise<Date> => new Date((await this.ctl('GET', '/clock')).now),
    state: async (): Promise<ClockState> => this.ctl('GET', '/clock'),
    freeze: async (at?: Date): Promise<ClockState> => this.ctl('POST', '/clock/freeze', at ? { at: iso(at) } : {}),
    unfreeze: async (): Promise<ClockState> => this.ctl('POST', '/clock/unfreeze'),
    set: async (at: Date): Promise<ClockState> => this.ctl('POST', '/clock/set', { at: iso(at) }),
    /** A duration string (`'500ms'`, `'1h30m'`, `'-2w'`) or a number of milliseconds (fractions allowed). */
    advance: async (by: string | number): Promise<ClockState> => this.ctl('POST', '/clock/advance', { by: durationMs(by) }),
    reset: async (): Promise<ClockState> => this.ctl('POST', '/clock/reset'),
  };

  readonly responses = {
    list: async (): Promise<{ id: string; created_at: string }[]> => (await this.ctl('GET', '/responses')).responses,
    /** Forgets all stored responses; returns how many there were. */
    clear: async (): Promise<number> => (await this.ctl('DELETE', '/responses')).deleted,
  };

  /** Clears scripts, request log, clock and stored responses (each can be kept with `false`). */
  async reset(opts: ResetOptions = {}): Promise<void> {
    await this.ctl('POST', `/reset${query({ ...opts })}`);
  }

  status(): Promise<Status> {
    return this.ctl('GET', '');
  }

  /**
   * Client configuration for a real tool (`claude`, `codex`, `opencode`, `pi`, `openai-sdk`, ...), from the
   * binary's `wire` subcommand. Only available when this instance spawned the binary.
   */
  async wire(target: string, opts: WireOptions = {}): Promise<WireOutput | string> {
    if (!this.#bin) throw new FakeLLMError('wire() needs the fakellm binary, which a connect()ed instance does not have');
    const format = opts.format ?? 'json';
    const r = spawnSync(this.#bin, ['wire', target, '--endpoint', this.url, '--format', format], { encoding: 'utf8', timeout: 10_000 });
    if (r.error) throw new FakeLLMError(`fakellm wire: ${r.error.message}`);
    if (r.status !== 0) throw new FakeLLMError(`fakellm wire ${target} failed: ${r.stderr.trim()}`);
    return format === 'json' ? (JSON.parse(r.stdout) as WireOutput) : r.stdout;
  }

  /** Kills the child process and waits for it to exit. Idempotent; a no-op for `connect()`ed instances. */
  stop(): Promise<void> {
    this.#stopped ??= (async () => {
      if (this.#onExit) {
        live.delete(this.#onExit);
        if (live.size === 0) process.removeListener('exit', onProcessExit);
      }
      const child = this.#child;
      if (child && child.exitCode === null && child.signalCode === null) {
        const exited = new Promise<void>((r) => child.once('exit', () => r()));
        child.ref();
        child.kill();
        await exited;
      }
      if (this.#tmp) rmSync(this.#tmp, { recursive: true, force: true });
    })();
    return this.#stopped;
  }

  [Symbol.asyncDispose](): Promise<void> {
    return this.stop();
  }
}

/** Attaches to a server that is already running (no process is managed). */
export function connect(url: string): FakeLLM {
  return new FakeLLM(url);
}

/** The first of options.bin, $FAKELLM_BIN, PATH. A configured but missing binary is an error, not a reason to fall back. */
function resolveBin(opts: StartOptions): { bin: string; how: string } {
  if (opts.bin) return { bin: opts.bin, how: 'options.bin' };
  if (process.env.FAKELLM_BIN) return { bin: process.env.FAKELLM_BIN, how: 'FAKELLM_BIN' };
  return { bin: 'fakellm', how: 'PATH' };
}

/** Spawns fakellm on a free port and resolves once it prints its endpoint. */
export async function start(opts: StartOptions = {}): Promise<FakeLLM> {
  const args = ['--host', opts.host ?? '127.0.0.1', '--port', String(opts.port ?? 0)];
  let tmp: string | undefined;
  if (opts.apiKey !== undefined) args.push('--api-key', opts.apiKey);
  if (opts.models?.length) args.push('--models', opts.models.join(','));
  if (opts.strict) args.push('--strict');
  if (opts.ttft !== undefined) args.push('--ttft', typeof opts.ttft === 'number' ? `${opts.ttft}ms` : opts.ttft);
  if (opts.fallbackTokens !== undefined) args.push('--fallback-tokens', String(opts.fallbackTokens));
  if (opts.chunkTokens !== undefined) args.push('--chunk-tokens', String(opts.chunkTokens));
  if (opts.argsChunkBytes !== undefined) args.push('--args-chunk-bytes', String(opts.argsChunkBytes));
  if (opts.tokensPerSec !== undefined) args.push('--tokens-per-sec', String(opts.tokensPerSec));
  if (opts.requestLog === false) args.push('--no-request-log');
  if (opts.requestLogCapacity !== undefined) args.push('--request-log-capacity', String(opts.requestLogCapacity));
  if (opts.redactAuth) args.push('--redact-auth');
  if (opts.policyRuntime) args.push('--policy-runtime', opts.policyRuntime);
  if (opts.pathPrefix) args.push('--path-prefix', opts.pathPrefix);
  if (typeof opts.scripts === 'string') {
    args.push('--scripts', opts.scripts);
  } else if (opts.scripts) {
    tmp = mkdtempSync(join(tmpdir(), 'fakellm-'));
    const file = join(tmp, 'scripts.json');
    writeFileSync(file, JSON.stringify(opts.scripts));
    args.push('--scripts', file);
  }
  args.push(...(opts.args ?? []));

  const { bin, how } = resolveBin(opts);
  const child = spawn(bin, args, { stdio: ['ignore', 'pipe', 'inherit'], env: { ...process.env, ...opts.env } });
  const outcome = await ready(child, opts.readyTimeoutMs ?? 10_000);
  if (outcome.kind === 'ok') {
    const srv = new FakeLLM(reachable(outcome.url), child, bin, tmp);
    try {
      // Through the control plane rather than flags: policies carry per-policy runtimes, and a load error comes back readable.
      if (opts.trajectories?.length) await srv.trajectories.put(opts.trajectories);
      if (opts.policies?.length) await srv.policies.put(opts.policies);
    } catch (e) {
      await srv.stop();
      throw e;
    }
    return srv;
  }
  if (tmp) rmSync(tmp, { recursive: true, force: true });
  if (outcome.kind === 'enoent') {
    throw new FakeLLMError(
      `could not run the fakellm binary '${bin}' (from ${how}). Resolution order: options.bin, FAKELLM_BIN, 'fakellm' on PATH ` +
        `(cargo build gives target/debug/fakellm).`,
    );
  }
  throw new FakeLLMError(`${bin}: ${outcome.message}`);
}

/** An unspecified bind address (`0.0.0.0`, `[::]`) is not something a client can connect to everywhere. */
function reachable(url: string): string {
  const u = new URL(url);
  if (u.hostname === '0.0.0.0') u.hostname = '127.0.0.1';
  else if (u.hostname === '[::]') u.hostname = '[::1]';
  return u.origin;
}

type Ready = { kind: 'ok'; url: string } | { kind: 'enoent' } | { kind: 'fail'; message: string };

function ready(child: ChildProcess, timeoutMs: number): Promise<Ready> {
  return new Promise((resolve) => {
    const done = (r: Ready) => {
      clearTimeout(timer);
      rl.close();
      child.removeListener('error', onError);
      child.removeListener('exit', onExit);
      if (r.kind !== 'ok') child.kill();
      else child.stdout!.resume(); // keep draining so the pipe never fills up
      resolve(r);
    };
    const timer = setTimeout(() => done({ kind: 'fail', message: `not ready after ${timeoutMs}ms` }), timeoutMs);
    const rl = createInterface({ input: child.stdout! });
    rl.on('line', (line) => {
      if (/^http:\/\/\S+$/.test(line.trim())) done({ kind: 'ok', url: line.trim() });
    });
    const onError = (e: NodeJS.ErrnoException) => done(e.code === 'ENOENT' ? { kind: 'enoent' } : { kind: 'fail', message: e.message });
    const onExit = (code: number | null, sig: NodeJS.Signals | null) =>
      done({ kind: 'fail', message: `exited before ready (${code ?? sig}); see stderr above` });
    child.once('error', onError);
    child.once('exit', onExit);
  });
}

/** Sugar: `{reply: {text}}`. */
export const text = (t: string, extra: Partial<Rule> = {}): Rule => ({ ...extra, reply: { text: t } });

/** Sugar: a rule whose reply is one tool call. */
export const toolCall = (name: string, args: unknown = {}, extra: Partial<Rule> = {}): Rule => ({
  ...extra,
  reply: { tool_calls: [{ name, arguments: args }] },
});

/** Sugar: a rule whose reply is an error with the given HTTP status. */
export const error = (status: number, opts: Omit<ErrorSpec, 'status'> & Partial<Pick<Rule, 'name' | 'match' | 'times'>> = {}): Rule => {
  const { name, match, times, ...spec } = opts;
  const reply: Reply = { error: { status, ...spec } };
  return { ...(name !== undefined && { name }), ...(match && { match }), ...(times !== undefined && { times }), reply };
};
