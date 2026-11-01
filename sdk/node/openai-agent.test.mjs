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

const TOOLS = ['Read', 'Edit'].map((name) => ({
  type: 'function',
  function: { name, parameters: { type: 'object', properties: {} } },
}));

const TRAJECTORY = {
  name: 'fix-bug',
  stop_text: 'Trajectory ended.',
  tool_map: {
    read_file: { name: 'Read', args: { file_path: '$path' } },
    edit_file: { name: 'Edit', args: { file_path: '$path', new_text: '${text}!' } },
  },
  steps: [
    { reply: { tool_calls: [{ name: 'read_file', arguments: { path: 'a.rs' } }] } },
    {
      reply: { tool_calls: [{ name: 'edit_file', arguments: { path: 'a.rs', text: 'patched' } }] },
      expect: { tool_results: [{ name: 'read_file', contains: 'fn main' }] },
    },
    { reply: { text: 'Fixed it.' }, expect: { tool_results: [{ name: 'edit_file', contains: 'ok' }] } },
  ],
};

// The caller owns the loop: run the tool, append assistant call and tool result, ask again.
async function run(prompt, results) {
  const messages = [{ role: 'user', content: prompt }];
  const calls = [];
  for (let turn = 0; turn < 6; turn++) {
    const r = await client.chat.completions.create({ model: 'gpt-4o', messages, tools: TOOLS });
    const msg = r.choices[0].message;
    if (!msg.tool_calls?.length) return { text: msg.content, calls, finish: r.choices[0].finish_reason };
    messages.push(msg);
    for (const c of msg.tool_calls) {
      calls.push({ name: c.function.name, args: JSON.parse(c.function.arguments) });
      messages.push({ role: 'tool', tool_call_id: c.id, content: results[c.function.name] ?? 'ok' });
    }
  }
  throw new Error('loop did not finish');
}

test('trajectory loop with mapped tool names and arguments', async () => {
  await srv.ctl.put('/trajectories', TRAJECTORY);
  await srv.ctl.put('/scripts', [{ reply: { trajectory: 'fix-bug' } }]);
  const out = await run('fix the bug', { Read: 'fn main() {}', Edit: 'ok' });
  assert.equal(out.text, 'Fixed it.');
  assert.equal(out.finish, 'stop');
  assert.deepEqual(out.calls, [
    { name: 'Read', args: { file_path: 'a.rs' } },
    { name: 'Edit', args: { file_path: 'a.rs', new_text: 'patched!' } },
  ]);
  const list = await srv.ctl.get('/trajectories');
  assert.equal(list.trajectories[0].hits, 3);
});

test('streamed trajectory steps carry the mapped call', async () => {
  await srv.ctl.put('/trajectories', TRAJECTORY);
  await srv.ctl.put('/scripts', [{ reply: { trajectory: 'fix-bug', stream: { args_chunk_bytes: 6 } } }]);
  const stream = await client.chat.completions.create({
    model: 'gpt-4o',
    messages: [{ role: 'user', content: 'go' }],
    tools: TOOLS,
    stream: true,
  });
  let name = '';
  let args = '';
  let fragments = 0;
  for await (const chunk of stream) {
    for (const tc of chunk.choices[0]?.delta?.tool_calls ?? []) {
      name += tc.function?.name ?? '';
      if (tc.function?.arguments) {
        args += tc.function.arguments;
        fragments++;
      }
    }
  }
  assert.equal(name, 'Read');
  assert.deepEqual(JSON.parse(args), { file_path: 'a.rs' });
  assert.ok(fragments > 1, `arguments arrived in ${fragments} fragment(s)`);
});

test('divergence is a 400 the SDK reports without retrying', async () => {
  await srv.ctl.put('/trajectories', TRAJECTORY);
  await srv.ctl.put('/scripts', [{ reply: { trajectory: 'fix-bug' } }]);
  await assert.rejects(run('fix the bug', { Read: 'something else' }), (e) => {
    assert.equal(e.status, 400);
    assert.equal(e.code, 'fakellm_trajectory_diverged');
    assert.match(e.message, /expected/);
    return true;
  });
});

test('divergence in stop mode ends the loop politely', async () => {
  await srv.ctl.put('/trajectories', { ...TRAJECTORY, on_divergence: 'stop' });
  await srv.ctl.put('/scripts', [{ reply: { trajectory: 'fix-bug' } }]);
  const out = await run('fix the bug', { Read: 'unrelated output', Edit: 'ok' });
  assert.equal(out.text, 'Trajectory ended.');
  assert.equal(out.calls.length, 1);
});

test('unknown tool names the offered tools', async () => {
  await srv.ctl.put('/trajectories', { ...TRAJECTORY, tool_map: {} });
  await srv.ctl.put('/scripts', [{ reply: { trajectory: 'fix-bug' } }]);
  await assert.rejects(client.chat.completions.create({ model: 'gpt-4o', messages: [{ role: 'user', content: 'x' }], tools: TOOLS }), (e) => {
    assert.equal(e.code, 'fakellm_unknown_tool');
    assert.match(e.message, /Read/);
    return true;
  });
});

test('embeddings.create: floats, base64 and batches', async () => {
  const one = await client.embeddings.create({ model: 'text-embedding-3-small', input: 'hello', dimensions: 64 });
  assert.equal(one.data.length, 1);
  assert.equal(one.data[0].embedding.length, 64);
  assert.ok(one.usage.prompt_tokens > 0);
  assert.ok(one.usage.total_tokens >= one.usage.prompt_tokens);

  const again = await client.embeddings.create({ model: 'text-embedding-3-small', input: 'hello', dimensions: 64 });
  assert.deepEqual(again.data[0].embedding, one.data[0].embedding);

  const b64 = await client.embeddings.create({ model: 'text-embedding-3-small', input: 'hello', dimensions: 64, encoding_format: 'base64' });
  assert.equal(typeof b64.data[0].embedding, 'string');
  const raw = Buffer.from(b64.data[0].embedding, 'base64');
  const f32 = new Float32Array(raw.buffer.slice(raw.byteOffset, raw.byteOffset + raw.length));
  assert.equal(f32.length, 64);
  for (let i = 0; i < 64; i++) assert.ok(Math.abs(f32[i] - one.data[0].embedding[i]) < 1e-6);

  const batch = await client.embeddings.create({ model: 'text-embedding-3-large', input: ['a', 'b', 'c'] });
  assert.equal(batch.data.length, 3);
  assert.equal(batch.data[0].embedding.length, 1536);
  assert.notDeepEqual(batch.data[0].embedding, batch.data[1].embedding);
});

test('embeddings errors can be scripted and are retried by the SDK', async () => {
  await srv.ctl.put('/scripts', [{ times: 1, reply: { error: { status: 429, message: 'rate limited' } } }]);
  const retrying = new OpenAI({ baseURL: srv.baseURL, apiKey: 'test', maxRetries: 2 });
  const r = await retrying.embeddings.create({ model: 'text-embedding-3-small', input: 'x', dimensions: 4 });
  assert.equal(r.data[0].embedding.length, 4);
  const log = await srv.ctl.get('/requests?path=/v1/embeddings');
  assert.deepEqual(
    log.requests.map((q) => q.status),
    [429, 200],
  );
});

test('embeddings validation error reaches the SDK', async () => {
  await assert.rejects(client.embeddings.create({ model: 'text-embedding-3-small', input: '' }), (e) => {
    assert.equal(e.status, 400);
    assert.equal(e.param, 'input');
    return true;
  });
});
