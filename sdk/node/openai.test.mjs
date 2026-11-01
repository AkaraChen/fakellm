import { test, before, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import OpenAI from 'openai';
import { startServer } from './helpers.mjs';

let srv;
let client;

before(async () => {
  srv = await startServer();
  client = new OpenAI({ baseURL: srv.baseURL, apiKey: 'test', maxRetries: 0 });
});
after(() => srv.stop());
beforeEach(async () => {
  await srv.ctl.post('/reset');
});

const user = (content) => [{ role: 'user', content }];

test('sync fallback reply', async () => {
  const r = await client.chat.completions.create({ model: 'gpt-4o', messages: user('hello') });
  assert.equal(typeof r.choices[0].message.content, 'string');
  assert.ok(r.choices[0].message.content.length > 0);
  assert.equal(r.usage.total_tokens, r.usage.prompt_tokens + r.usage.completion_tokens);
});

test('scripted rule text', async () => {
  await srv.ctl.put('/scripts', [{ match: { last_user: 'ping' }, reply: { text: 'pong' } }]);
  const r = await client.chat.completions.create({ model: 'gpt-4o', messages: user('ping') });
  assert.equal(r.choices[0].message.content, 'pong');
});

test('streaming with usage', async () => {
  const text = 'the quick brown fox jumps';
  await srv.ctl.put('/scripts', [{ reply: { text } }]);
  const stream = await client.chat.completions.create({
    model: 'gpt-4o',
    messages: user('go'),
    stream: true,
    stream_options: { include_usage: true },
  });
  const chunks = [];
  for await (const c of stream) chunks.push(c);
  assert.equal(chunks[0].choices[0].delta.role, 'assistant');
  assert.equal(chunks.map((c) => c.choices[0]?.delta?.content ?? '').join(''), text);
  assert.ok(chunks.some((c) => c.choices[0]?.finish_reason === 'stop'));
  const last = chunks.at(-1);
  assert.equal(last.choices.length, 0);
  assert.ok(last.usage.total_tokens > 0);
});

test('error rule 429 with retry_after', async () => {
  await srv.ctl.put('/scripts', [{ reply: { error: { status: 429, message: 'slow down', retry_after: 2 } } }]);
  await assert.rejects(
    client.chat.completions.create({ model: 'gpt-4o', messages: user('x') }),
    (e) => {
      assert.ok(e instanceof OpenAI.APIError);
      assert.equal(e.status, 429);
      assert.ok(e.code);
      return true;
    },
  );
});

const weather = { name: 'get_weather', arguments: { city: 'Paris' }, id: 'call_1' };

test('tool call sync', async () => {
  await srv.ctl.put('/scripts', [{ reply: { tool_calls: [weather] } }]);
  const r = await client.chat.completions.create({ model: 'gpt-4o', messages: user('weather?') });
  assert.equal(r.choices[0].finish_reason, 'tool_calls');
  const call = r.choices[0].message.tool_calls[0];
  assert.equal(call.function.name, 'get_weather');
  assert.deepEqual(JSON.parse(call.function.arguments), { city: 'Paris' });
});

test('tool call streamed reassembles by index', async () => {
  await srv.ctl.put('/scripts', [
    { reply: { tool_calls: [weather, { name: 'get_time', arguments: { tz: 'UTC' }, id: 'call_2' }] } },
  ]);
  const stream = await client.chat.completions.create({ model: 'gpt-4o', messages: user('weather?'), stream: true });
  const calls = [];
  let finish;
  for await (const c of stream) {
    for (const tc of c.choices[0]?.delta?.tool_calls ?? []) {
      const slot = (calls[tc.index] ??= { name: '', args: '' });
      slot.name += tc.function?.name ?? '';
      slot.args += tc.function?.arguments ?? '';
    }
    finish = c.choices[0]?.finish_reason ?? finish;
  }
  assert.equal(finish, 'tool_calls');
  assert.equal(calls.length, 2);
  assert.equal(calls[0].name, 'get_weather');
  assert.deepEqual(JSON.parse(calls[0].args), { city: 'Paris' });
  assert.equal(calls[1].name, 'get_time');
  assert.deepEqual(JSON.parse(calls[1].args), { tz: 'UTC' });
});

test('virtual clock gates the first chunk', async () => {
  await srv.ctl.post('/clock/freeze');
  await srv.ctl.put('/scripts', [{ reply: { text: 'late', stream: { ttft_ms: 500 } } }]);
  const stream = await client.chat.completions.create({ model: 'gpt-4o', messages: user('x'), stream: true });
  const it = stream[Symbol.asyncIterator]();
  const first = it.next();
  const timeout = new Promise((r) => setTimeout(() => r('timeout'), 100));
  assert.equal(await Promise.race([first, timeout]), 'timeout');
  await srv.ctl.post('/clock/advance', { by: '500ms' });
  const got = await first;
  assert.equal(got.done, false);
  assert.equal(got.value.choices[0].delta.role, 'assistant');
  for (;;) if ((await it.next()).done) break;
});

test('request log records messages and tools', async () => {
  await client.chat.completions.create({
    model: 'gpt-4o',
    messages: [
      { role: 'system', content: 'You are terse.' },
      { role: 'user', content: 'hi' },
    ],
    tools: [{ type: 'function', function: { name: 'lookup', parameters: { type: 'object', properties: {} } } }],
  });
  const log = await srv.ctl.get('/requests?model=gpt-4o');
  assert.equal(log.total, 1);
  const req = log.requests[0].request;
  assert.equal(req.messages[0].role, 'system');
  assert.match(JSON.stringify(req.messages[0].content), /You are terse\./);
  assert.match(JSON.stringify(req.tools), /lookup/);
});

test('models.list', async () => {
  const ids = [];
  for await (const m of client.models.list()) ids.push(m.id);
  assert.ok(ids.length > 0);
  assert.ok(ids.every((id) => typeof id === 'string'));
});

test('cut_after_chunks: stream ends early without a finish chunk', async () => {
  await srv.ctl.put('/scripts', [
    { reply: { text: 'one two three four five six', stream: { chunk_tokens: 1, cut_after_chunks: 2 } } },
  ]);
  const stream = await client.chat.completions.create({ model: 'gpt-4o', messages: user('go'), stream: true });
  const chunks = [];
  let failure;
  try {
    for await (const c of stream) chunks.push(c);
  } catch (e) {
    failure = e;
  }
  // Observed: the openai SDK ends iteration cleanly when the body ends without [DONE]; the only signal
  // is that no chunk carries a finish_reason.
  assert.ok(chunks.every((c) => !c.choices[0]?.finish_reason), JSON.stringify(chunks));
  assert.equal(chunks.filter((c) => c.choices[0]?.delta?.content).length, 2);
  assert.equal(failure, undefined);
});

test('streaming n=2 keeps choices separate', async () => {
  await srv.ctl.put('/scripts', [{ reply: { text: 'alpha beta gamma' } }]);
  const stream = await client.chat.completions.create({ model: 'gpt-4o', messages: user('go'), stream: true, n: 2 });
  const text = ['', ''];
  const finishes = [0, 0];
  for await (const c of stream) {
    for (const ch of c.choices) {
      text[ch.index] += ch.delta.content ?? '';
      if (ch.finish_reason) finishes[ch.index] += 1;
    }
  }
  assert.deepEqual(text, ['alpha beta gamma', 'alpha beta gamma']);
  assert.deepEqual(finishes, [1, 1]);
});

test('reset_after_chunks: the SDK throws a connection error mid-stream', async () => {
  await srv.ctl.put('/scripts', [
    { reply: { text: 'one two three four five six', stream: { chunk_tokens: 1, reset_after_chunks: 2 } } },
  ]);
  const stream = await client.chat.completions.create({ model: 'gpt-4o', messages: user('go'), stream: true });
  const chunks = [];
  let failure;
  try {
    for await (const c of stream) chunks.push(c);
  } catch (e) {
    failure = e;
  }
  assert.ok(failure, 'expected the stream to throw');
  // Observed: undici surfaces the abort as TypeError('terminated') with cause 'other side closed'.
  assert.match(failure.message, /terminated|connection|closed/i);
  assert.ok(chunks.length >= 1);
  assert.ok(chunks.every((c) => !c.choices[0]?.finish_reason));
  const log = await srv.ctl.get('/requests');
  assert.equal(log.requests.length, 1);
});
