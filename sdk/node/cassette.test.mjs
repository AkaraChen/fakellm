import { test, before, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import OpenAI from 'openai';
import Anthropic from '@anthropic-ai/sdk';
import { startServer } from './helpers.mjs';


let srv;
let openai;
let anthropic;

before(async () => {
  srv = await startServer();
  openai = new OpenAI({ baseURL: srv.baseURL, apiKey: 'test', maxRetries: 0 });
  anthropic = new Anthropic({ baseURL: srv.url, apiKey: 'test', maxRetries: 0 });
});
after(() => srv.stop());
beforeEach(async () => {
  await srv.ctl.post('/reset');
});

const exchange = (user, text) => ({
  request: { model: 'gpt-4o', messages: [{ role: 'user', content: [{ type: 'text', text: user }] }] },
  outcome: {
    outcome: 'reply',
    reply: { text, finish: 'stop', usage: { prompt_tokens: 9, completion_tokens: 4 } },
  },
  protocol: 'openai.chat',
});

const tape = { kind: 'cassette', name: 'tape', exchanges: [exchange('what is the capital of France?', 'Paris, of course.')] };

test('a recorded chat exchange replays through the openai SDK', async () => {
  await srv.ctl.put('/cassettes', tape);
  const r = await openai.chat.completions.create({
    model: 'gpt-4o',
    temperature: 0.3,
    max_tokens: 40,
    messages: [{ role: 'user', content: 'what is the capital of France?' }],
  });
  assert.equal(r.choices[0].message.content, 'Paris, of course.');
  assert.equal(r.usage.prompt_tokens, 9);
  assert.equal(r.usage.completion_tokens, 4);
  const log = await srv.ctl.get('/requests?source=cassette');
  assert.equal(log.total, 1);
});

test('the same recording answers the Anthropic SDK and a stream', async () => {
  await srv.ctl.put('/cassettes', tape);
  const m = await anthropic.messages.create({
    model: 'claude-sonnet-4-5',
    max_tokens: 100,
    messages: [{ role: 'user', content: 'what is the capital of France?' }],
  });
  assert.equal(m.content[0].text, 'Paris, of course.');
  assert.equal(m.stop_reason, 'end_turn');

  const stream = await openai.chat.completions.create({
    model: 'gpt-4o',
    stream: true,
    messages: [{ role: 'user', content: 'what is the capital of France?' }],
  });
  let text = '';
  for await (const chunk of stream) text += chunk.choices[0]?.delta?.content ?? '';
  assert.equal(text, 'Paris, of course.');
});

test('a request the cassette does not know gets the default reply', async () => {
  await srv.ctl.put('/cassettes', tape);
  const r = await openai.chat.completions.create({
    model: 'gpt-4o',
    messages: [{ role: 'user', content: 'something else entirely' }],
  });
  assert.notEqual(r.choices[0].message.content, 'Paris, of course.');
  assert.equal((await srv.ctl.get('/requests?source=cassette')).total, 0);
});
