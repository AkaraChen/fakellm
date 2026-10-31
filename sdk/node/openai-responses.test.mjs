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

test('sync create', async () => {
  await srv.ctl.put('/scripts', [{ reply: { text: 'hello there' } }]);
  const r = await client.responses.create({ model: 'gpt-4o', input: 'hi' });
  assert.equal(r.output_text, 'hello there');
  assert.equal(r.status, 'completed');
  assert.ok(r.usage.input_tokens > 0 && r.usage.output_tokens > 0);
});

test('stream events', async () => {
  const text = 'streamed words arrive one by one';
  await srv.ctl.put('/scripts', [{ reply: { text } }]);
  const stream = await client.responses.create({ model: 'gpt-4o', input: 'hi', stream: true });
  const events = [];
  for await (const e of stream) events.push(e);
  assert.equal(events[0].type, 'response.created');
  assert.equal(
    events.filter((e) => e.type === 'response.output_text.delta').map((e) => e.delta).join(''),
    text,
  );
  const last = events.at(-1);
  assert.equal(last.type, 'response.completed');
  assert.ok(last.response.usage.total_tokens > 0);
});

test('function_call item', async () => {
  await srv.ctl.put('/scripts', [
    { reply: { tool_calls: [{ name: 'get_weather', arguments: { city: 'Paris' }, id: 'call_1' }] } },
  ]);
  const r = await client.responses.create({
    model: 'gpt-4o',
    input: 'weather?',
    tools: [{ type: 'function', name: 'get_weather', parameters: { type: 'object', properties: {} }, strict: false }],
  });
  const item = r.output.find((o) => o.type === 'function_call');
  assert.equal(item.name, 'get_weather');
  assert.deepEqual(JSON.parse(item.arguments), { city: 'Paris' });
});

test('previous_response_id chaining', async () => {
  await srv.ctl.put('/scripts', [{ reply: { text: 'first answer' } }]);
  const r1 = await client.responses.create({ model: 'gpt-4o', input: 'question one' });
  const r2 = await client.responses.create({ model: 'gpt-4o', input: 'question two', previous_response_id: r1.id });
  assert.equal(r2.previous_response_id, r1.id);
  assert.ok(r2.usage.input_tokens > r1.usage.input_tokens);
  const log = await srv.ctl.get('/requests?full=true');
  const ir = JSON.stringify(log.requests.at(-1).request);
  assert.ok(ir.includes('question one') && ir.includes('first answer') && ir.includes('question two'), ir);
});

test('retrieve and delete', async () => {
  const r = await client.responses.create({ model: 'gpt-4o', input: 'hi' });
  const got = await client.responses.retrieve(r.id);
  assert.equal(got.id, r.id);
  assert.equal(got.output_text, r.output_text);
  await client.responses.delete(r.id);
  await assert.rejects(client.responses.retrieve(r.id), (e) => e.status === 404);
});
