import { test, before, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { generateText, streamText, tool, stepCountIs } from 'ai';
import { createOpenAI } from '@ai-sdk/openai';
import { createOpenAICompatible } from '@ai-sdk/openai-compatible';
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

// `.chat` pins Chat Completions; the default provider call targets the Responses API.
test('generateText', async () => {
  await srv.ctl.put('/scripts', [{ reply: { text: 'hello from script' } }]);
  const r = await generateText({ model: provider.chat('gpt-4o'), prompt: 'hi' });
  assert.equal(r.text, 'hello from script');
});

test('streamText', async () => {
  const text = 'streamed words arrive one by one';
  await srv.ctl.put('/scripts', [{ reply: { text } }]);
  const result = streamText({ model: provider.chat('gpt-4o'), prompt: 'hi' });
  let out = '';
  for await (const t of result.textStream) out += t;
  assert.equal(out, text);
  assert.ok((await result.usage).totalTokens > 0);
  assert.equal(await result.finishReason, 'stop');
});

test('tool loop over two steps', async () => {
  await srv.ctl.put('/scripts', [
    { match: { last_tool_result: 'get_weather' }, reply: { text: 'It is sunny in Paris.' } },
    { reply: { tool_calls: [{ name: 'get_weather', arguments: { city: 'Paris' }, id: 'call_w' }] } },
  ]);
  const r = await generateText({
    model: provider.chat('gpt-4o'),
    prompt: 'weather in Paris?',
    tools: {
      get_weather: tool({ inputSchema: z.object({ city: z.string() }), execute: async () => 'sunny' }),
    },
    stopWhen: stepCountIs(3),
  });
  assert.equal(r.text, 'It is sunny in Paris.');
  assert.equal(r.steps.length, 2);
});

test('openai-compatible provider', async () => {
  await srv.ctl.put('/scripts', [{ reply: { text: 'compat ok' } }]);
  const compat = createOpenAICompatible({ name: 'fake', baseURL: srv.baseURL, apiKey: 'test' });
  const r = await generateText({ model: compat.chatModel('gpt-4o'), prompt: 'hi' });
  assert.equal(r.text, 'compat ok');
  const result = streamText({ model: compat.chatModel('gpt-4o'), prompt: 'hi' });
  let out = '';
  for await (const t of result.textStream) out += t;
  assert.equal(out, 'compat ok');
});

test('cut_after_chunks: streamText sees no finish', async () => {
  await srv.ctl.put('/scripts', [
    { reply: { text: 'one two three four five six', stream: { chunk_tokens: 1, cut_after_chunks: 2 } } },
  ]);
  const parts = [];
  const r = streamText({
    model: provider.chat('gpt-4o'),
    prompt: 'go',
    onError: () => {},
  });
  try {
    for await (const p of r.fullStream) parts.push(p);
  } catch {}
  const deltas = parts.filter((p) => p.type === 'text-delta').map((p) => p.text ?? p.delta ?? '');
  assert.equal(deltas.join(''), 'one two');
  assert.ok(!parts.some((p) => p.type === 'finish' && p.finishReason === 'stop'), JSON.stringify(parts.map((p) => p.type)));
});
