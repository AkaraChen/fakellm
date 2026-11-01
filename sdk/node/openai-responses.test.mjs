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

const recordedResponse = {
  id: 'resp_recorded',
  object: 'response',
  created_at: 1770933892,
  status: 'completed',
  model: 'gpt-4.1-nano-2025-04-14',
  output: [
    {
      id: 'msg_recorded',
      type: 'message',
      status: 'completed',
      role: 'assistant',
      content: [{ type: 'output_text', text: 'recorded answer', annotations: [] }],
    },
  ],
  usage: { input_tokens: 5, output_tokens: 3, total_tokens: 8 },
};

test('raw rule: a recorded response body', async () => {
  await srv.ctl.put('/scripts', [
    { match: { path: '/v1/responses' }, reply: { raw: { body: JSON.stringify(recordedResponse) } } },
  ]);
  const r = await client.responses.create({ model: 'gpt-4o', input: 'hi' });
  assert.equal(r.id, 'resp_recorded');
  assert.equal(r.output_text, 'recorded answer');
  assert.equal(r.usage.total_tokens, 8);
});

test('raw rule: recorded Responses events, one frame each', async () => {
  const frames = [
    ['response.created', { type: 'response.created', sequence_number: 0, response: { ...recordedResponse, status: 'in_progress', output: [] } }],
    ['response.output_text.delta', { type: 'response.output_text.delta', sequence_number: 1, item_id: 'msg_recorded', output_index: 0, content_index: 0, delta: 'recorded ' }],
    ['response.output_text.delta', { type: 'response.output_text.delta', sequence_number: 2, item_id: 'msg_recorded', output_index: 0, content_index: 0, delta: 'answer' }],
    ['response.completed', { type: 'response.completed', sequence_number: 3, response: recordedResponse }],
  ];
  const body = frames.map(([event, data]) => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`).join('');
  await srv.ctl.put('/scripts', [
    {
      match: { path: '/v1/responses', last_user: 'replay' },
      reply: { raw: { content_type: 'text/event-stream', body } },
    },
  ]);
  const stream = await client.responses.create({ model: 'gpt-4o', input: 'replay this', stream: true });
  const events = [];
  for await (const e of stream) events.push(e);
  assert.deepEqual(events.map((e) => e.type), frames.map(([event]) => event));
  assert.equal(
    events.filter((e) => e.type === 'response.output_text.delta').map((e) => e.delta).join(''),
    'recorded answer',
  );
  assert.equal(events.at(-1).response.id, 'resp_recorded');
  // A request the rule does not match gets the generated reply.
  const other = await client.responses.create({ model: 'gpt-4o', input: 'something else' });
  assert.notEqual(other.id, 'resp_recorded');
});
