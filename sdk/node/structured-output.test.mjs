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

const schema = {
  type: 'object',
  properties: {
    name: { type: 'string', minLength: 3 },
    age: { type: 'integer', minimum: 18, maximum: 99 },
    email: { type: 'string', format: 'email' },
    role: { type: 'string', enum: ['admin', 'user'] },
    tags: { type: 'array', items: { type: 'string' }, minItems: 1 },
  },
  required: ['name', 'age', 'email', 'role', 'tags'],
  additionalProperties: false,
};

function assertValid(v) {
  assert.deepEqual(Object.keys(v).sort(), Object.keys(schema.properties).sort());
  assert.equal(typeof v.name, 'string');
  assert.ok(v.name.length >= 3);
  assert.ok(Number.isInteger(v.age) && v.age >= 18 && v.age <= 99, `age ${v.age}`);
  assert.match(v.email, /@/);
  assert.ok(['admin', 'user'].includes(v.role));
  assert.ok(Array.isArray(v.tags) && v.tags.length >= 1 && v.tags.every((t) => typeof t === 'string'));
}

test('response_format json_schema gets a schema-valid object', async () => {
  const r = await client.chat.completions.create({
    model: 'gpt-4o',
    messages: [{ role: 'user', content: 'make a person' }],
    response_format: { type: 'json_schema', json_schema: { name: 'person', strict: true, schema } },
  });
  assert.equal(r.choices[0].finish_reason, 'stop');
  assertValid(JSON.parse(r.choices[0].message.content));
});

test('streamed json_schema output joins to a valid object', async () => {
  const stream = await client.chat.completions.create({
    model: 'gpt-4o',
    messages: [{ role: 'user', content: 'make a person' }],
    response_format: { type: 'json_schema', json_schema: { name: 'person', schema } },
    stream: true,
  });
  let text = '';
  for await (const chunk of stream) text += chunk.choices[0]?.delta?.content ?? '';
  assertValid(JSON.parse(text));
});

test('Responses text.format json_schema', async () => {
  const r = await client.responses.create({
    model: 'gpt-5.5',
    input: 'make a person',
    text: { format: { type: 'json_schema', name: 'person', strict: true, schema } },
  });
  assertValid(JSON.parse(r.output_text));
});

test('a forced tool gets arguments that fit its schema', async () => {
  const r = await client.chat.completions.create({
    model: 'gpt-4o',
    messages: [{ role: 'user', content: 'forecast' }],
    tools: [{ type: 'function', function: { name: 'forecast', parameters: schema } }],
    tool_choice: { type: 'function', function: { name: 'forecast' } },
  });
  const call = r.choices[0].message.tool_calls[0];
  assert.equal(call.function.name, 'forecast');
  assertValid(JSON.parse(call.function.arguments));
});
