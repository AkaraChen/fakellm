import { test, before, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { generateText, tool, stepCountIs } from 'ai';
import { createOpenAI } from '@ai-sdk/openai';
import { z } from 'zod';
import { startServer } from './helpers.mjs';

let srv;
let provider;

before(async () => {
  srv = await startServer();
  provider = createOpenAI({ baseURL: srv.baseURL, apiKey: 'test' });
});
after(() => srv.stop());
beforeEach(async () => {
  await srv.ctl.post('/reset');
});

// read_file first, then edit_file; retry the edit once when it reports an error, then give up.
const POLICY = `
const resultText = (m) => m.content.map((p) => p.text ?? '').join('');
export default function decide(request, context) {
  const last = request.messages.at(-1);
  const state = context.state;
  if (last.role !== 'tool') {
    return { tool_calls: [{ name: 'read_file', arguments: { path: 'a.txt' } }], state: { stage: 'read', tries: 0 } };
  }
  if (state.stage === 'read') {
    return { tool_calls: [{ name: 'edit_file', arguments: { path: 'a.txt', text: 'fixed' } }], state: { stage: 'edit', tries: 1 } };
  }
  if (resultText(last).startsWith('ERROR')) {
    if (state.tries < 2) {
      return { tool_calls: [{ name: 'edit_file', arguments: { path: 'a.txt', text: 'fixed again' } }], state: { stage: 'edit', tries: state.tries + 1 } };
    }
    return { text: 'I give up.', state: { stage: 'done' } };
  }
  return { text: 'File edited.', state: { stage: 'done' } };
}
`;

function tools(editResults) {
  const calls = { read: [], edit: [] };
  return {
    calls,
    tools: {
      read_file: tool({
        inputSchema: z.object({ path: z.string() }),
        execute: async (a) => (calls.read.push(a), 'old contents'),
      }),
      edit_file: tool({
        inputSchema: z.object({ path: z.string(), text: z.string() }),
        execute: async (a) => (calls.edit.push(a), editResults.shift() ?? 'ok'),
      }),
    },
  };
}

async function registerPolicy() {
  await srv.ctl.put('/policies', { name: 'agent', source: POLICY });
  await srv.ctl.put('/scripts', [{ reply: { policy: 'agent' } }]);
}

test('policy drives a read, edit, answer loop', async () => {
  await registerPolicy();
  const t = tools([]);
  const r = await generateText({ model: provider.chat('gpt-4o'), prompt: 'fix a.txt', tools: t.tools, stopWhen: stepCountIs(4) });
  assert.equal(r.text, 'File edited.');
  assert.equal(r.steps.length, 3);
  assert.deepEqual(t.calls.read, [{ path: 'a.txt' }]);
  assert.deepEqual(t.calls.edit, [{ path: 'a.txt', text: 'fixed' }]);
  const info = await srv.ctl.get('/policies/agent');
  assert.equal(info.hits, 3);
  assert.equal(info.state.stage, 'done');
});

test('policy retries a failed edit once, then gives up', async () => {
  await registerPolicy();
  const t = tools(['ERROR: no match', 'ERROR: still no match']);
  const r = await generateText({ model: provider.chat('gpt-4o'), prompt: 'fix a.txt', tools: t.tools, stopWhen: stepCountIs(6) });
  assert.equal(r.text, 'I give up.');
  assert.equal(t.calls.edit.length, 2);
  assert.equal(r.steps.length, 4);
});

test('policy works over the Responses API', async () => {
  await registerPolicy();
  const t = tools([]);
  const r = await generateText({ model: provider('gpt-4o'), prompt: 'fix a.txt', tools: t.tools, stopWhen: stepCountIs(4) });
  assert.equal(r.text, 'File edited.');
  assert.equal(t.calls.edit.length, 1);
});

test('a failing policy surfaces as an API error, not a retry storm', async () => {
  await srv.ctl.put('/policies', { name: 'bad', source: "export default () => { throw new Error('policy bug'); };" });
  await srv.ctl.put('/scripts', [{ reply: { policy: 'bad' } }]);
  await assert.rejects(
    generateText({ model: provider.chat('gpt-4o'), prompt: 'x', maxRetries: 2 }),
    (e) => /policy bug/.test(e.message) || /policy bug/.test(JSON.stringify(e.responseBody ?? '')),
  );
  const info = await srv.ctl.get('/policies/bad');
  assert.equal(info.hits, 0);
});
