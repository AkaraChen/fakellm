import { test, before, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { startServer } from './helpers.mjs';

let srv;
let client;

before(async () => {
  srv = await startServer();
  client = new Anthropic({ baseURL: srv.url, apiKey: 'test', maxRetries: 0 });
});
after(() => srv.stop());
beforeEach(async () => {
  await srv.ctl.post('/reset');
});

const model = 'claude-opus-4-7';
const user = (content) => [{ role: 'user', content }];

test('sync fallback reply', async () => {
  const r = await client.messages.create({ model, max_tokens: 256, messages: user('hello') });
  assert.equal(r.content[0].type, 'text');
  assert.ok(r.content[0].text.length > 0);
  assert.ok(r.usage.input_tokens > 0);
  assert.equal(r.stop_reason, 'end_turn');
});

test('scripted rule text', async () => {
  await srv.ctl.put('/scripts', [{ match: { last_user: 'ping' }, reply: { text: 'pong' } }]);
  const r = await client.messages.create({ model, max_tokens: 64, messages: user('ping') });
  assert.equal(r.content[0].text, 'pong');
});

test('streaming collects text and final message', async () => {
  const text = 'the quick brown fox jumps';
  await srv.ctl.put('/scripts', [{ reply: { text } }]);
  const stream = client.messages.stream({ model, max_tokens: 64, messages: user('go') });
  let out = '';
  stream.on('text', (t) => (out += t));
  const final = await stream.finalMessage();
  assert.equal(out, text);
  assert.equal(final.stop_reason, 'end_turn');
  assert.ok(final.usage.output_tokens > 0);
});

test('tool_use block with parsed input', async () => {
  await srv.ctl.put('/scripts', [
    { reply: { tool_calls: [{ name: 'get_weather', arguments: { city: 'Paris' }, id: 'toolu_1' }] } },
  ]);
  const r = await client.messages.create({
    model,
    max_tokens: 128,
    messages: user('weather?'),
    tools: [{ name: 'get_weather', description: 'w', input_schema: { type: 'object', properties: { city: { type: 'string' } } } }],
  });
  assert.equal(r.stop_reason, 'tool_use');
  const block = r.content.find((b) => b.type === 'tool_use');
  assert.equal(block.name, 'get_weather');
  assert.deepEqual(block.input, { city: 'Paris' });
});

test('streamed tool_use reassembles input', async () => {
  await srv.ctl.put('/scripts', [
    { reply: { tool_calls: [{ name: 'get_weather', arguments: { city: 'Paris' }, id: 'toolu_1' }] } },
  ]);
  const stream = client.messages.stream({ model, max_tokens: 128, messages: user('weather?') });
  const final = await stream.finalMessage();
  const block = final.content.find((b) => b.type === 'tool_use');
  assert.deepEqual(block.input, { city: 'Paris' });
});

test('thinking block carries a signature', async () => {
  await srv.ctl.put('/scripts', [{ reply: { reasoning: 'let me think', text: 'answer' } }]);
  const r = await client.messages.create({
    model,
    max_tokens: 4096,
    thinking: { type: 'enabled', budget_tokens: 1024 },
    messages: user('why?'),
  });
  assert.equal(r.content[0].type, 'thinking');
  assert.equal(r.content[0].thinking, 'let me think');
  assert.ok(r.content[0].signature.length > 0);
  assert.equal(r.content[1].text, 'answer');
});

test('529 overloaded surfaces as APIError', async () => {
  await srv.ctl.put('/scripts', [{ reply: { error: { status: 529, message: 'busy' } } }]);
  await assert.rejects(client.messages.create({ model, max_tokens: 16, messages: user('x') }), (e) => {
    assert.ok(e instanceof Anthropic.APIError);
    assert.equal(e.status, 529);
    assert.equal(e.error.error.type, 'overloaded_error');
    return true;
  });
});

test('count_tokens', async () => {
  const r = await client.messages.countTokens({ model, messages: user('hello world') });
  assert.ok(r.input_tokens > 0);
});

test('virtual clock gates the first event', async () => {
  await srv.ctl.post('/clock/freeze');
  await srv.ctl.put('/scripts', [{ reply: { text: 'late', stream: { ttft_ms: 500 } } }]);
  const stream = await client.messages.create({ model, max_tokens: 16, messages: user('x'), stream: true });
  const it = stream[Symbol.asyncIterator]();
  const first = it.next();
  const timeout = new Promise((r) => setTimeout(() => r('timeout'), 100));
  const early = await Promise.race([first, timeout]);
  // message_start may arrive before the gate; the content must not.
  let seenText = early !== 'timeout' && early.value?.type === 'content_block_delta';
  assert.equal(seenText, false);
  await srv.ctl.post('/clock/advance', { by: '500ms' });
  let text = '';
  let ev = early === 'timeout' ? await first : early;
  for (;;) {
    if (ev.done) break;
    if (ev.value.type === 'content_block_delta') text += ev.value.delta.text;
    ev = await it.next();
  }
  assert.equal(text, 'late');
});

test('request log shows system and tools', async () => {
  await client.messages.create({
    model,
    max_tokens: 16,
    system: 'You are terse.',
    messages: user('hi'),
    tools: [{ name: 'lookup', description: 'd', input_schema: { type: 'object', properties: {} } }],
  });
  const log = await srv.ctl.get(`/requests?model=${model}`);
  assert.equal(log.total, 1);
  const req = log.requests[0].request;
  assert.match(JSON.stringify(req), /You are terse\./);
  assert.match(JSON.stringify(req.tools), /lookup/);
});
