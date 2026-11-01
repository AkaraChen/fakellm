import { test, before, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import OpenAI from 'openai';
import { start, connect, text, toolCall, error, FakeLLMError } from '../dist/index.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const bin = process.env.FAKELLM_BIN ?? path.resolve(here, '../../../target/debug/fakellm');
let srv;
let client;

before(async () => {
  srv = await start({ bin, models: ['m1', 'm2'] });
  client = new OpenAI({ baseURL: srv.baseURL, apiKey: 'k', maxRetries: 0 });
});
after(() => srv.stop());
beforeEach(() => srv.reset());

const ask = (content = 'hi', extra = {}) => client.chat.completions.create({ model: 'm1', messages: [{ role: 'user', content }], ...extra });

test('start/stop: urls, status, idempotent stop', async () => {
  const s = await start({ bin });
  assert.match(s.url, /^http:\/\/127\.0\.0\.1:\d+$/);
  assert.equal(s.baseURL, `${s.url}/v1`);
  assert.equal(s.anthropicBaseURL, s.url);
  assert.ok(s.port > 0);
  assert.equal((await s.status()).strict, false);
  await s.stop();
  await s.stop();
  await assert.rejects(s.status(), FakeLLMError);
});

test('start: options and rules at start', async () => {
  const s = await start({ bin, strict: true, scripts: [text('boot', { match: { last_user: 'x' } })], apiKey: 'sekret' });
  try {
    const c = new OpenAI({ baseURL: s.baseURL, apiKey: 'sekret', maxRetries: 0 });
    const r = await c.chat.completions.create({ model: 'm', messages: [{ role: 'user', content: 'x' }] });
    assert.equal(r.choices[0].message.content, 'boot');
    assert.equal((await s.status()).strict, true);
    await assert.rejects(new OpenAI({ baseURL: s.baseURL, apiKey: 'bad', maxRetries: 0 }).models.list());
  } finally {
    await s.stop();
  }
});

test('start: missing binary lists what was tried', async () => {
  await assert.rejects(start({ bin: '/nonexistent/fakellm-x' }), (e) => e instanceof FakeLLMError && e.message.includes('/nonexistent/fakellm-x'));
});

test('scripts: put/add/list/clear and keepHits', async () => {
  await srv.scripts.put([text('one', { name: 'a', match: { last_user: 'a' } })]);
  await srv.scripts.add([text('two', { name: 'b' })]);
  assert.equal((await ask('a')).choices[0].message.content, 'one');
  assert.equal((await ask('zzz')).choices[0].message.content, 'two');
  let list = await srv.scripts.list();
  assert.deepEqual(list.map((r) => [r.name, r.hits]), [['a', 1], ['b', 1]]);
  await srv.scripts.put([text('uno', { name: 'a', match: { last_user: 'a' } })], { keepHits: true });
  assert.equal((await srv.scripts.list())[0].hits, 1);
  await srv.scripts.clear();
  assert.equal((await srv.scripts.list()).length, 0);
});

test('scripts: tool call and error helpers', async () => {
  await srv.scripts.put([error(429, { code: 'rate_limit_exceeded', retry_after: 2, times: 1 }), toolCall('read_file', { path: 'a' })]);
  await assert.rejects(ask(), (e) => e.status === 429);
  const r = await ask();
  assert.equal(r.choices[0].message.tool_calls[0].function.name, 'read_file');
});

test('scripts: bad rules throw FakeLLMError with status and message', async () => {
  await assert.rejects(srv.scripts.put([{ reply: {} }]), (e) => e instanceof FakeLLMError && e.status === 400 && /reply needs/.test(e.message));
});

test('requests: filter, count, clear', async () => {
  await srv.scripts.put([error(500, { match: { last_user: 'boom' }, name: 'boom' })]);
  await ask('ok');
  await assert.rejects(ask('boom'));
  await ask('tools', { tools: [{ type: 'function', function: { name: 'f', parameters: {} } }] });
  assert.equal(await srv.requests.count(), 3);
  assert.equal(await srv.requests.count({ status: 'error' }), 1);
  assert.equal((await srv.requests.list({ rule: 'boom' }))[0].status, 500);
  assert.equal((await srv.requests.list({ has_tool: 'f' })).length, 1);
  assert.equal((await srv.requests.list({ limit: 2 })).length, 2);
  assert.equal(await srv.requests.count({ limit: 1 }), 3);
  const [full] = await srv.requests.list({ full: true, limit: 1 });
  assert.ok(Array.isArray(full.headers));
  await srv.requests.clear();
  assert.equal(await srv.requests.count(), 0);
});

test('clock: a frozen clock gates a stream until advanced', async () => {
  await srv.clock.freeze(new Date('2030-01-02T03:04:05Z'));
  assert.equal((await srv.clock.now()).toISOString(), '2030-01-02T03:04:05.000Z');
  await srv.scripts.put([{ reply: { text: 'late', stream: { ttft_ms: 500 } } }]);
  const stream = await ask('x', { stream: true });
  const it = stream[Symbol.asyncIterator]();
  const first = it.next();
  const early = await Promise.race([first, new Promise((r) => setTimeout(() => r('timeout'), 150))]);
  assert.equal(early, 'timeout');
  await srv.clock.advance(500);
  const got = await first;
  assert.equal(got.done, false);
  let out = got.value.choices[0]?.delta?.content ?? '';
  for await (const c of { [Symbol.asyncIterator]: () => it }) out += c.choices[0]?.delta?.content ?? '';
  assert.match(out, /late/);
  await srv.clock.advance('1h');
  assert.equal((await srv.clock.now()).toISOString(), '2030-01-02T04:04:05.500Z');
  await srv.clock.set(new Date('2031-01-01T00:00:00Z'));
  assert.equal((await srv.clock.state()).frozen, true);
  await srv.clock.unfreeze();
  assert.equal((await srv.clock.state()).frozen, false);
  await srv.clock.reset();
  assert.equal((await srv.clock.state()).shifted, false);
});

test('responses and reset options', async () => {
  await client.responses.create({ model: 'm', input: 'hi' });
  assert.equal((await srv.responses.list()).length, 1);
  await srv.scripts.put([text('keep')]);
  await srv.reset({ scripts: false });
  assert.equal((await srv.scripts.list()).length, 1);
  assert.equal((await srv.responses.list()).length, 0);
  await srv.responses.clear();
});

test('wire: codex config, other formats, connect() cannot wire', async () => {
  const w = await srv.wire('codex');
  assert.ok(w.files['config.toml'].includes(srv.baseURL));
  assert.ok(w.env.FAKELLM_API_KEY !== undefined || typeof w.snippet === 'string');
  assert.match(await srv.wire('claude', { format: 'env' }), new RegExp(srv.url.replace(/\./g, '\\.')));
  await assert.rejects(srv.wire('nope'), FakeLLMError);
  await assert.rejects(connect(srv.url).wire('codex'), FakeLLMError);
});

test('connect(): controls an existing server without owning it', async () => {
  const c = connect(srv.url + '/');
  assert.equal(c.port, srv.port);
  await c.scripts.put([text('via connect')]);
  assert.equal((await ask()).choices[0].message.content, 'via connect');
  await c.stop();
  assert.equal((await srv.status()).version.length > 0, true);
});

test('requests.config, responses.clear count, fractional advance, 0.0.0.0 host', async () => {
  assert.equal((await srv.requests.config()).enabled, true);
  assert.equal((await srv.requests.config({ capacity: 1 })).capacity, 1);
  await ask();
  await ask();
  assert.equal(await srv.requests.dropped(), 1);
  await srv.requests.config({ capacity: 100 });
  await client.responses.create({ model: 'm', input: 'hi' });
  assert.equal(await srv.responses.clear(), 1);
  await srv.clock.freeze();
  await srv.clock.advance(1.5);
  await srv.clock.advance(1e21).catch(() => {});
  const any = await start({ bin, host: '0.0.0.0', requestLog: false, requestLogCapacity: 5, redactAuth: true });
  try {
    assert.match(any.url, /^http:\/\/127\.0\.0\.1:\d+$/);
    assert.equal((await any.status()).request_log.enabled, false);
  } finally {
    await any.stop();
  }
});

const askTools = (messages) =>
  client.chat.completions.create({
    model: 'm1',
    messages,
    tools: [{ type: 'function', function: { name: 'Bash', parameters: {} } }],
  });

const traj = {
  name: 't1',
  tool_map: { shell: { name: 'Bash', args: { command: '$cmd' } } },
  steps: [
    { reply: { tool_calls: [{ name: 'shell', arguments: { cmd: 'echo hi' }, id: 'c1' }] } },
    { reply: { text: 'all done' }, expect: { tool_results: [{ contains: 'hi' }] } },
  ],
};

test('trajectories: put/add/list/clear and replay through a rule', async () => {
  assert.deepEqual(await srv.trajectories.put([traj]), { added: 1, count: 1 });
  await srv.scripts.put([{ reply: { trajectory: 't1' } }]);
  const user = { role: 'user', content: 'go' };
  const r1 = await askTools([user]);
  const call = r1.choices[0].message.tool_calls[0];
  assert.equal(call.function.name, 'Bash');
  assert.deepEqual(JSON.parse(call.function.arguments), { command: 'echo hi' });
  const r2 = await askTools([user, r1.choices[0].message, { role: 'tool', tool_call_id: 'c1', content: 'hi' }]);
  assert.equal(r2.choices[0].message.content, 'all done');
  const [info] = await srv.trajectories.list();
  assert.equal(info.name, 't1');
  assert.equal(info.steps, 2);
  assert.equal(info.hits, 2);
  assert.equal((await srv.status()).trajectories.count, 1);
  assert.deepEqual(await srv.trajectories.add([{ ...traj, name: 't2' }]), { added: 1, count: 2 });
  await srv.trajectories.clear();
  assert.equal((await srv.trajectories.list()).length, 0);
});

test('trajectories: an invalid one throws FakeLLMError', async () => {
  await assert.rejects(srv.trajectories.put([{ name: 'a/b', steps: [{ reply: { text: 'x' } }] }]), (e) => e instanceof FakeLLMError && e.status === 400);
});

const policySource = "export default () => ({ text: 'from policy' });";

test('policies: put/add/list/clear and a load error', async () => {
  // No runtime named: the one embedded in the binary runs it in-process.
  assert.deepEqual(await srv.policies.put([{ name: 'p1', source: policySource }]), { added: 1, count: 1 });
  await srv.scripts.put([{ reply: { policy: 'p1' } }]);
  assert.equal((await ask()).choices[0].message.content, 'from policy');
  const [info] = await srv.policies.list();
  assert.equal(info.name, 'p1');
  assert.equal(info.runtime, 'tjs');
  assert.equal(info.in_process, true);
  assert.equal(info.hits, 1);
  // 'node' is the explicit subprocess runtime.
  assert.equal((await srv.policies.add([{ name: 'p2', source: policySource, runtime: 'node' }])).count, 2);
  const node = (await srv.policies.list()).find((p) => p.name === 'p2');
  assert.deepEqual([node.runtime, node.in_process], ['node', false]);
  await assert.rejects(srv.policies.put([{ name: 'bad', source: 'export default 1 +' }]), (e) => e instanceof FakeLLMError && e.status === 400);
  assert.equal((await srv.policies.list()).length, 2);
  await srv.policies.clear();
  assert.equal((await srv.policies.list()).length, 0);
});

test('start: trajectories, policies in both runtimes and pathPrefix', async () => {
  const s = await start({
    bin,
    trajectories: [{ name: 'boot', steps: [{ reply: { text: 'from trajectory' } }] }],
    policies: [{ name: 'pol', source: policySource }, { name: 'pol-node', source: policySource, runtime: 'node' }],
    pathPrefix: '/anthropic',
    scripts: [{ match: { last_user: 'traj' }, reply: { trajectory: 'boot' } }, { match: { last_user: 'pol' }, reply: { policy: 'pol' } }],
  });
  try {
    const c = new OpenAI({ baseURL: s.baseURL, apiKey: 'k', maxRetries: 0 });
    const say = async (content) => (await c.chat.completions.create({ model: 'm', messages: [{ role: 'user', content }] })).choices[0].message.content;
    assert.equal(await say('traj'), 'from trajectory');
    assert.equal(await say('pol'), 'from policy');
    assert.deepEqual((await s.policies.list()).map((p) => p.runtime), ['tjs', 'node']);
    const res = await fetch(`${s.url}/anthropic/v1/models`);
    assert.equal(res.status, 200);
  } finally {
    await s.stop();
  }
  for (const policyRuntime of [undefined, 'node']) {
    await assert.rejects(start({ bin, policyRuntime, policies: [{ name: 'bad', source: 'export default 1 +' }] }), (e) => e instanceof FakeLLMError);
  }
});

const tape = (name, user, answer, extra = {}) => ({
  kind: 'cassette',
  name,
  ...extra,
  exchanges: [
    {
      request: { model: 'm1', messages: [{ role: 'user', content: [{ type: 'text', text: user }] }] },
      outcome: { outcome: 'reply', reply: { text: answer, finish: 'stop', usage: { prompt_tokens: 5, completion_tokens: 2 } } },
      protocol: 'openai.chat',
    },
  ],
});

test('cassettes: put/add/list/clear and replay', async () => {
  assert.deepEqual(await srv.cassettes.put(tape('c1', 'capital?', 'Paris')), { added: 1, count: 1 });
  assert.equal((await ask('capital?')).choices[0].message.content, 'Paris');
  assert.deepEqual(await srv.cassettes.add([tape('c2', 'other?', 'Rome')]), { added: 1, count: 2 });
  const list = await srv.cassettes.list();
  assert.deepEqual(list.map((c) => [c.name, c.exchanges, c.recording]), [['c1', 1, false], ['c2', 1, false]]);
  assert.equal(list[0].hits, 1);
  assert.equal((await srv.status()).cassettes.count, 2);
  // JSONL text: header line, then one exchange per line.
  const [header, ex] = [{ ...tape('c3', 'x', 'y'), exchanges: undefined }, tape('c3', 'jsonl?', 'from jsonl').exchanges[0]];
  assert.deepEqual(await srv.cassettes.put(`${JSON.stringify(header)}\n${JSON.stringify(ex)}\n`), { added: 1, count: 1 });
  assert.equal((await ask('jsonl?')).choices[0].message.content, 'from jsonl');
  await srv.cassettes.clear();
  assert.equal((await srv.cassettes.list()).length, 0);
});

test('cassettes: an invalid one throws FakeLLMError', async () => {
  await assert.rejects(srv.cassettes.put([tape('a/b', 'x', 'y')]), (e) => e instanceof FakeLLMError && e.status === 400);
});

test('snapshots: create/list/restore/delete', async () => {
  await srv.scripts.put([text('one')]);
  await srv.cassettes.put(tape('c1', 'capital?', 'Paris'));
  const snap = await srv.snapshots.create('base');
  assert.equal(snap.name, 'base');
  assert.equal(snap.scripts, 1);
  assert.equal(snap.cassettes, 1);
  await assert.rejects(srv.snapshots.create('base'), (e) => e instanceof FakeLLMError && e.status === 409);
  await srv.scripts.put([text('two')]);
  await srv.cassettes.clear();
  assert.equal((await ask()).choices[0].message.content, 'two');
  assert.equal((await srv.snapshots.restore('base')).name, 'base');
  assert.equal((await ask()).choices[0].message.content, 'one');
  assert.equal((await srv.cassettes.list()).length, 1);
  assert.equal((await srv.snapshots.create()).name.startsWith('snapshot-'), true);
  assert.equal((await srv.snapshots.list()).length, 2);
  assert.equal((await srv.status()).snapshots.count, 2);
  await srv.reset();
  assert.equal((await srv.snapshots.list()).length, 2, 'reset keeps snapshots');
  await srv.reset({ snapshots: true });
  assert.equal((await srv.snapshots.list()).length, 0);
  await assert.rejects(srv.snapshots.restore('base'), (e) => e instanceof FakeLLMError && e.status === 404);
  await srv.snapshots.create('x');
  await srv.snapshots.delete('x');
  await assert.rejects(srv.snapshots.delete('x'), (e) => e.status === 404);
});

test('start: cassettes, upstream and record', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'fakellm-test-'));
  const up = await start({ bin, scripts: [text('UPSTREAM_SAYS_HI')] });
  const rec = path.join(dir, 'rec.jsonl');
  const proxy = await start({ bin, upstream: up.url, record: rec, cassettes: [tape('boot', 'capital?', 'Paris')] });
  try {
    const c = new OpenAI({ baseURL: proxy.baseURL, apiKey: 'k', maxRetries: 0 });
    const ask2 = async (q) => (await c.chat.completions.create({ model: 'm1', messages: [{ role: 'user', content: q }] })).choices[0].message.content;
    assert.equal(await ask2('capital?'), 'Paris');
    assert.equal(await ask2('something new'), 'UPSTREAM_SAYS_HI');
    const st = await proxy.status();
    assert.equal(st.upstream.url.startsWith(up.url), true);
    assert.equal(st.upstream.recorded, 1);
    assert.equal((await proxy.cassettes.list()).some((c) => c.recording), true);
    const replay = await start({ bin, strict: true, cassettes: rec, watch: false });
    try {
      const c2 = new OpenAI({ baseURL: replay.baseURL, apiKey: 'k', maxRetries: 0 });
      const r = await c2.chat.completions.create({ model: 'm1', messages: [{ role: 'user', content: 'something new' }] });
      assert.equal(r.choices[0].message.content, 'UPSTREAM_SAYS_HI');
    } finally {
      await replay.stop();
    }
  } finally {
    await proxy.stop();
    await up.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test('raw rule: typed fields replay an SSE body on a standard route', async () => {
  const body = 'data: {"n":1}\n\ndata: [DONE]\n\n';
  /** @type {import('../dist/index.js').Rule} */
  const rule = {
    match: { path: '/v1/chat/completions', method: 'POST', header: { 'x-replay': '^yes$' }, nth: 1 },
    reply: { raw: { status: 203, content_type: 'text/event-stream', body, headers: { 'x-c': 'y' }, stream: true } },
  };
  await srv.scripts.put([rule]);
  const post = (replay) =>
    fetch(`${srv.url}/v1/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'x-replay': replay },
      body: JSON.stringify({ model: 'm', stream: true, messages: [{ role: 'user', content: 'hi' }] }),
    });
  const r = await post('yes');
  assert.equal(r.status, 203);
  assert.equal(r.headers.get('x-c'), 'y');
  assert.equal(await r.text(), body);
  assert.notEqual(await (await post('yes')).text(), body);
});

test('custom endpoint rule: match.path/body and reply body/status/headers', async () => {
  /** @type {import('../dist/index.js').Rule} */
  const rule = { match: { path: '/v1/systemone', body: 'angry' }, reply: { body: { mood: 'x' }, status: 202, headers: { 'x-c': 'y' } } };
  await srv.scripts.put([rule]);
  const r = await fetch(`${srv.url}/v1/systemone`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{"state":"angry"}' });
  assert.equal(r.status, 202);
  assert.equal(r.headers.get('x-c'), 'y');
  assert.deepEqual(await r.json(), { mood: 'x' });
});
