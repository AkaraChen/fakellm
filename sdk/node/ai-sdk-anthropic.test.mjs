import { test, before, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { generateText, streamText, tool, stepCountIs } from 'ai';
import { createAnthropic } from '@ai-sdk/anthropic';
import { z } from 'zod';
import { startServer } from './helpers.mjs';

let srv;
let anthropic;

before(async () => {
  srv = await startServer();
  anthropic = createAnthropic({ baseURL: srv.url + '/v1', apiKey: 'test' });
});
after(() => srv.stop());
beforeEach(async () => {
  await srv.ctl.post('/reset');
});

const model = () => anthropic('claude-opus-4-7');

test('generateText', async () => {
  await srv.ctl.put('/scripts', [{ reply: { text: 'hello from script' } }]);
  const r = await generateText({ model: model(), prompt: 'hi' });
  assert.equal(r.text, 'hello from script');
  assert.ok(r.usage.totalTokens > 0);
});

test('streamText', async () => {
  const text = 'streamed words arrive one by one';
  await srv.ctl.put('/scripts', [{ reply: { text } }]);
  const result = streamText({ model: model(), prompt: 'hi' });
  let out = '';
  for await (const t of result.textStream) out += t;
  assert.equal(out, text);
  assert.equal(await result.finishReason, 'stop');
});

test('tool loop over two steps', async () => {
  await srv.ctl.put('/scripts', [
    { match: { has_tool_result: true }, reply: { text: 'It is sunny in Paris.' } },
    { reply: { tool_calls: [{ name: 'get_weather', arguments: { city: 'Paris' }, id: 'toolu_w' }] } },
  ]);
  const r = await generateText({
    model: model(),
    prompt: 'weather in Paris?',
    tools: { get_weather: tool({ inputSchema: z.object({ city: z.string() }), execute: async () => 'sunny' }) },
    stopWhen: stepCountIs(3),
  });
  assert.equal(r.text, 'It is sunny in Paris.');
  assert.equal(r.steps.length, 2);
});

test('thinking via providerOptions', async () => {
  await srv.ctl.put('/scripts', [{ reply: { reasoning: 'pondering', text: 'done' } }]);
  const r = await generateText({
    model: model(),
    prompt: 'why?',
    maxOutputTokens: 4096,
    providerOptions: { anthropic: { thinking: { type: 'enabled', budgetTokens: 1024 } } },
  });
  assert.equal(r.text, 'done');
  assert.equal(r.reasoningText, 'pondering');
});
