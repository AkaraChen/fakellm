import { test, before, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
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
