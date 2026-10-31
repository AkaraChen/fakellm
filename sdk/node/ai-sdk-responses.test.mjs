import { test, before, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { generateText, streamText, tool, stepCountIs } from 'ai';
import { createOpenAI } from '@ai-sdk/openai';
import { z } from 'zod';
import { startServer } from './helpers.mjs';

let srv;
let openai;

before(async () => {
  srv = await startServer();
  openai = createOpenAI({ baseURL: srv.baseURL, apiKey: 'test' });
});
after(() => srv.stop());
beforeEach(async () => {
  await srv.ctl.post('/reset');
});

// The default provider call targets the Responses API.
test('generateText', async () => {
  await srv.ctl.put('/scripts', [{ reply: { text: 'hello from script' } }]);
  const r = await generateText({ model: openai('gpt-4o'), prompt: 'hi' });
  assert.equal(r.text, 'hello from script');
  const log = await srv.ctl.get('/requests?protocol=openai.responses');
  assert.equal(log.total, 1);
});

test('streamText', async () => {
  const text = 'streamed words arrive one by one';
  await srv.ctl.put('/scripts', [{ reply: { text } }]);
  const result = streamText({ model: openai('gpt-4o'), prompt: 'hi' });
  let out = '';
  for await (const t of result.textStream) out += t;
  assert.equal(out, text);
  assert.ok((await result.usage).totalTokens > 0);
  assert.equal(await result.finishReason, 'stop');
});

test('tool loop over two steps', async () => {
  await srv.ctl.put('/scripts', [
    { match: { has_tool_result: true }, reply: { text: 'It is sunny in Paris.' } },
    { reply: { tool_calls: [{ name: 'get_weather', arguments: { city: 'Paris' }, id: 'call_w' }] } },
  ]);
  const r = await generateText({
    model: openai('gpt-4o'),
    prompt: 'weather in Paris?',
    tools: { get_weather: tool({ inputSchema: z.object({ city: z.string() }), execute: async () => 'sunny' }) },
    stopWhen: stepCountIs(3),
  });
  assert.equal(r.text, 'It is sunny in Paris.');
  assert.equal(r.steps.length, 2);
});

test('reasoning summary', async () => {
  await srv.ctl.put('/scripts', [{ reply: { reasoning: 'weighing options', text: 'done' } }]);
  const r = await generateText({
    model: openai('gpt-5'),
    prompt: 'why?',
    providerOptions: { openai: { reasoningEffort: 'low', reasoningSummary: 'auto' } },
  });
  assert.equal(r.text, 'done');
  assert.equal(r.reasoningText, 'weighing options');
});
