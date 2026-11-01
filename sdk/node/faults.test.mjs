// Every oar FailureClass through the official SDKs (openai, @anthropic-ai/sdk, ai-sdk over both providers):
// which error class, status and message each SDK surfaces, and how many requests it makes (retries).
// The observed behaviour is recorded in docs/scripts.md (故障注入).
import { test, before, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import OpenAI from 'openai';
import Anthropic from '@anthropic-ai/sdk';
import { generateText, streamText, APICallError, RetryError } from 'ai';
import { createOpenAI } from '@ai-sdk/openai';
import { createAnthropic } from '@ai-sdk/anthropic';
import { startServer } from './helpers.mjs';

let srv;
let oa;
let an;
let aoa;
let aan;

before(async () => {
  srv = await startServer();
  // Default retry settings on purpose: the retry behaviour is what is under test.
  oa = new OpenAI({ baseURL: srv.baseURL, apiKey: 'test' });
  an = new Anthropic({ baseURL: srv.url, apiKey: 'test' });
  aoa = createOpenAI({ baseURL: srv.baseURL, apiKey: 'test' });
  aan = createAnthropic({ baseURL: srv.url + '/v1', apiKey: 'test' });
});
after(() => srv.stop());
beforeEach(async () => {
  await srv.ctl.post('/reset');
});

const msgs = [{ role: 'user', content: 'x' }];
const FAST = { 'retry-after-ms': '5' }; // keeps the retried cases quick

// One call per SDK; each rejects with the SDK's own error.
const sdks = {
  openai: () => oa.chat.completions.create({ model: 'gpt-4o', messages: msgs }),
  anthropic: () => an.messages.create({ model: 'claude-opus-4-7', max_tokens: 8, messages: msgs }),
  aiOpenai: () => generateText({ model: aoa.chat('gpt-4o'), prompt: 'x' }),
  aiAnthropic: () => generateText({ model: aan('claude-opus-4-7'), prompt: 'x' }),
};
const protocolOf = { openai: 'oa', anthropic: 'an', aiOpenai: 'oa', aiAnthropic: 'an' };

async function observe(name, rule) {
  await srv.ctl.post('/reset');
  await srv.ctl.put('/scripts', [{ reply: { error: rule } }]);
  let err;
  try {
    await sdks[name]();
  } catch (e) {
    err = e;
  }
  assert.ok(err, `${name}: expected a rejection`);
  const attempts = (await srv.ctl.get('/requests')).total;
  // ai-sdk wraps exhausted retries in a RetryError around the last APICallError.
  const root = RetryError.isInstance(err) ? err.lastError : err;
  return {
    cls: RetryError.isInstance(err) ? 'RetryError' : err.constructor.name,
    status: root.status ?? root.statusCode,
    message: err.message,
    attempts,
    err,
    root,
  };
}

// Per failure class: the rule for each protocol, a message fragment, and per SDK the observed
// { cls, status, attempts } (attempts 3 = the default 2 retries were used).
const classes = [
  {
    name: 'auth 401',
    oa: { status: 401 },
    an: { status: 401 },
    msg: { oa: 'Incorrect API key', an: 'invalid x-api-key' },
    want: {
      openai: ['AuthenticationError', 401, 1],
      anthropic: ['AuthenticationError', 401, 1],
      aiOpenai: ['APICallError', 401, 1],
      aiAnthropic: ['APICallError', 401, 1],
    },
  },
  {
    // OpenAI's insufficient_quota is a 429: the SDKs cannot tell it from a rate limit and retry.
    name: 'quota',
    oa: { status: 429, type: 'insufficient_quota', code: 'insufficient_quota', message: 'You exceeded your current quota', headers: FAST },
    an: { status: 400, type: 'invalid_request_error', message: 'You have reached your specified workspace API usage limits.' },
    msg: { oa: 'You exceeded your current quota', an: 'workspace API usage limits' },
    want: {
      openai: ['RateLimitError', 429, 3],
      anthropic: ['BadRequestError', 400, 1],
      aiOpenai: ['RetryError', 429, 3],
      aiAnthropic: ['APICallError', 400, 1],
    },
  },
  {
    name: 'rate_limited (retried)',
    oa: { status: 429, headers: FAST },
    an: { status: 429, headers: FAST },
    msg: { oa: 'Rate limit reached', an: 'rate limit' },
    want: {
      openai: ['RateLimitError', 429, 3],
      anthropic: ['RateLimitError', 429, 3],
      aiOpenai: ['RetryError', 429, 3],
      aiAnthropic: ['RetryError', 429, 3],
    },
  },
  {
    // x-should-retry: false stops the openai and Anthropic SDKs; ai-sdk does not read it.
    name: 'rate_limited + x-should-retry false',
    oa: { status: 429, headers: { ...FAST, 'x-should-retry': 'false' } },
    an: { status: 429, headers: { ...FAST, 'x-should-retry': 'false' } },
    msg: { oa: 'Rate limit reached', an: 'rate limit' },
    want: {
      openai: ['RateLimitError', 429, 1],
      anthropic: ['RateLimitError', 429, 1],
      aiOpenai: ['RetryError', 429, 3],
      aiAnthropic: ['RetryError', 429, 3],
    },
  },
  {
    name: 'billing',
    oa: { status: 400, type: 'invalid_request_error', code: 'billing_hard_limit_reached', message: 'Billing hard limit has been reached.' },
    an: { status: 402, type: 'billing_error', message: 'There was an issue with your billing or payment information.' },
    msg: { oa: 'Billing hard limit', an: 'billing or payment' },
    want: {
      openai: ['BadRequestError', 400, 1],
      anthropic: ['APIError', 402, 1], // no dedicated class for 402
      aiOpenai: ['APICallError', 400, 1],
      aiAnthropic: ['APICallError', 402, 1],
    },
  },
  {
    name: 'model_unavailable',
    oa: { status: 404 },
    an: { status: 404, message: 'model: claude-x' },
    msg: { oa: 'does not exist', an: 'model: claude-x' },
    want: {
      openai: ['NotFoundError', 404, 1],
      anthropic: ['NotFoundError', 404, 1],
      aiOpenai: ['APICallError', 404, 1],
      aiAnthropic: ['APICallError', 404, 1],
    },
  },
  {
    name: 'input_too_large',
    oa: { status: 400, type: 'invalid_request_error', code: 'context_length_exceeded', param: 'messages', message: "This model's maximum context length is 128000 tokens" },
    an: { status: 400, type: 'invalid_request_error', message: 'prompt is too long: 213462 tokens > 200000 maximum' },
    msg: { oa: 'maximum context length', an: 'prompt is too long' },
    want: {
      openai: ['BadRequestError', 400, 1],
      anthropic: ['BadRequestError', 400, 1],
      aiOpenai: ['APICallError', 400, 1],
      aiAnthropic: ['APICallError', 400, 1],
    },
  },
  {
    name: 'overloaded',
    oa: { status: 503, headers: FAST },
    an: { status: 529, headers: FAST },
    msg: { oa: 'overloaded', an: 'Overloaded' },
    want: {
      openai: ['InternalServerError', 503, 3],
      anthropic: ['InternalServerError', 529, 3],
      aiOpenai: ['RetryError', 503, 3],
      aiAnthropic: ['RetryError', 529, 3],
    },
  },
  {
    name: 'provider 500',
    oa: { status: 500, headers: FAST },
    an: { status: 500, headers: FAST },
    msg: { oa: 'server had an error', an: 'Internal server error' },
    want: {
      openai: ['InternalServerError', 500, 3],
      anthropic: ['InternalServerError', 500, 3],
      aiOpenai: ['RetryError', 500, 3],
      aiAnthropic: ['RetryError', 500, 3],
    },
  },
];

for (const c of classes) {
  for (const sdk of Object.keys(sdks)) {
    test(`${c.name}: ${sdk}`, async () => {
      const p = protocolOf[sdk];
      const got = await observe(sdk, c[p]);
      const [cls, status, attempts] = c.want[sdk];
      assert.equal(got.cls, cls, got.message);
      assert.equal(got.status, status);
      assert.equal(got.attempts, attempts, 'requests made (retries)');
      assert.ok(got.message.includes(c.msg[p]), `${JSON.stringify(c.msg[p])} not in ${got.message}`);
      if (got.cls === 'RetryError') {
        assert.equal(got.err.reason, 'maxRetriesExceeded');
        assert.equal(got.err.errors.length, attempts);
      }
      if (sdk.startsWith('ai')) assert.ok(APICallError.isInstance(got.root));
    });
  }
}

test('error codes and types each SDK exposes', async () => {
  const q = await observe('openai', classes[1].oa);
  assert.equal(q.err.code, 'insufficient_quota');
  assert.equal(q.err.type, 'insufficient_quota');
  const big = await observe('openai', classes[6].oa);
  assert.equal(big.err.code, 'context_length_exceeded');
  assert.equal(big.err.param, 'messages');
  const bill = await observe('anthropic', classes[4].an);
  assert.equal(bill.err.error.error.type, 'billing_error');
  const over = await observe('anthropic', classes[7].an);
  assert.equal(over.err.error.error.type, 'overloaded_error');
  // ai-sdk marks only 408/409/429/5xx as retryable.
  const auth = await observe('aiOpenai', classes[0].oa);
  assert.equal(auth.root.isRetryable, false);
  const prov = await observe('aiAnthropic', classes[8].an);
  assert.equal(prov.root.isRetryable, true);
});

test('x-should-retry overrides the status-based decision (openai, Anthropic)', async () => {
  for (const sdk of ['openai', 'anthropic']) {
    const no = await observe(sdk, { status: 500, headers: { ...FAST, 'x-should-retry': 'false' } });
    assert.equal(no.attempts, 1, `${sdk}: 500 + false`);
    const yes = await observe(sdk, { status: 400, headers: { ...FAST, 'x-should-retry': 'true' } });
    assert.equal(yes.attempts, 3, `${sdk}: 400 + true`);
  }
  // ai-sdk ignores the header in both directions.
  const no = await observe('aiOpenai', { status: 500, headers: { ...FAST, 'x-should-retry': 'false' } });
  assert.equal(no.attempts, 3);
  const yes = await observe('aiOpenai', { status: 400, headers: { ...FAST, 'x-should-retry': 'true' } });
  assert.equal(yes.attempts, 1);
});

test('retry-after-ms is honoured: two retries with 5 ms waits finish quickly', async () => {
  const t0 = Date.now();
  const got = await observe('openai', { status: 429, headers: FAST });
  assert.equal(got.attempts, 3);
  // Without the header the SDK backs off ~0.5 s then ~1 s.
  assert.ok(Date.now() - t0 < 400, `took ${Date.now() - t0} ms`);
});

// ---- mid-stream failures -------------------------------------------------------------------

const text = 'one two three four five six';
const openaiStream = async (rules) => {
  await srv.ctl.put('/scripts', rules);
  const s = await oa.chat.completions.create({ model: 'gpt-4o', messages: msgs, stream: true });
  let out = '';
  for await (const c of s) out += c.choices[0]?.delta?.content ?? '';
  return out;
};
const anthropicFinal = async (rules) => {
  await srv.ctl.put('/scripts', rules);
  return an.messages.stream({ model: 'claude-opus-4-7', max_tokens: 8, messages: msgs }).finalMessage();
};
// Drains streamText; returns { text, error } where error is a thrown error or an `error` part.
const aiStream = async (model) => {
  const r = streamText({ model, prompt: 'x', onError() {} });
  let out = '';
  let error;
  try {
    for await (const part of r.fullStream) {
      if (part.type === 'text-delta') out += part.text;
      if (part.type === 'error') error = part.error;
    }
  } catch (e) {
    error = e;
  }
  return { text: out, error };
};

test('mid-stream cut (clean end, no terminator): the stream just stops', async () => {
  const rules = [{ reply: { text, stream: { cut_after_chunks: 2 } } }];
  const out = await openaiStream(rules);
  assert.ok(out.length > 0 && out.length < text.length, out);
  await assert.rejects(anthropicFinal(rules), (e) => {
    assert.equal(e.constructor.name, 'AnthropicError');
    assert.match(e.message, /stream ended without producing a Message/);
    return true;
  });
  await srv.ctl.put('/scripts', rules);
  for (const model of [aoa.chat('gpt-4o'), aan('claude-opus-4-7')]) {
    const r = await aiStream(model);
    assert.ok(r.text.length < text.length, r.text);
  }
  assert.equal((await srv.ctl.get('/requests')).total, 4, 'no retries after the stream started');
});

test('mid-stream RST: the connection error reaches the caller, never retried', async () => {
  const rules = [{ reply: { text, stream: { reset_after_chunks: 2 } } }];
  await assert.rejects(openaiStream(rules), (e) => {
    // Node (undici): TypeError "terminated" with cause ECONNRESET; Bun reports ECONNRESET directly.
    assert.match(`${e.message} ${e.code} ${e.cause?.code}`, /ECONNRESET/i);
    if (typeof Bun === 'undefined') assert.equal(e.message, 'terminated');
    return true;
  });
  await assert.rejects(anthropicFinal(rules), (e) => {
    assert.equal(e.constructor.name, 'AnthropicError');
    assert.match(e.message, /terminated|ECONNRESET/);
    return true;
  });
  for (const model of [aoa.chat('gpt-4o'), aan('claude-opus-4-7')]) {
    const r = await aiStream(model);
    assert.ok(r.error, 'ai-sdk reports the broken connection');
  }
  assert.equal((await srv.ctl.get('/requests')).total, 4, 'not retried');
});

test('in-stream error: openai and Anthropic SDKs throw APIError without status, never retry', async () => {
  const oaRule = [{ reply: { error: { status: 503, in_stream: true, headers: FAST } } }];
  await assert.rejects(openaiStream(oaRule), (e) => {
    assert.equal(e.constructor.name, 'APIError');
    assert.equal(e.status, undefined);
    assert.equal(e.type, 'server_error');
    assert.match(e.message, /overloaded/);
    return true;
  });
  const anRule = [{ reply: { error: { status: 529, in_stream: true, headers: FAST } } }];
  await assert.rejects(anthropicFinal(anRule), (e) => {
    assert.equal(e.constructor.name, 'APIError');
    assert.equal(e.status, undefined);
    assert.equal(e.type, 'overloaded_error');
    return true;
  });
  assert.equal((await srv.ctl.get('/requests')).total, 2);
});

test('in-stream error through ai-sdk: OpenAI is retried, Anthropic surfaces a StreamProviderError part', async () => {
  await srv.ctl.put('/scripts', [{ reply: { error: { status: 503, in_stream: true, headers: FAST } } }]);
  const a = await aiStream(aoa.chat('gpt-4o'));
  assert.ok(RetryError.isInstance(a.error), String(a.error));
  assert.equal((await srv.ctl.get('/requests')).total, 3);

  await srv.ctl.post('/reset');
  await srv.ctl.put('/scripts', [{ reply: { error: { status: 529, in_stream: true, headers: FAST } } }]);
  const b = await aiStream(aan('claude-opus-4-7'));
  assert.equal(b.error.name, 'AI_StreamProviderError');
  assert.equal(b.error.type, 'overloaded_error');
  assert.equal(b.error.statusCode, 529);
  assert.equal((await srv.ctl.get('/requests')).total, 1);
});

// ---- hang released by the clock ------------------------------------------------------------

for (const sdk of Object.keys(sdks)) {
  test(`hang_ms held by the frozen clock, released by advance: ${sdk}`, async () => {
    await srv.ctl.post('/clock/freeze');
    await srv.ctl.put('/scripts', [{ reply: { text: 'late', hang_ms: 30000 } }]);
    let settled = false;
    const p = sdks[sdk]().finally(() => (settled = true));
    for (let i = 0; i < 100; i++) {
      if ((await srv.ctl.get('/requests')).requests[0]?.hung) break;
      await new Promise((r) => setTimeout(r, 20));
    }
    assert.equal((await srv.ctl.get('/requests')).requests[0].hung, true);
    await new Promise((r) => setTimeout(r, 100));
    assert.equal(settled, false, 'still waiting for the response headers');
    await srv.ctl.post('/clock/advance', { by: '30s' });
    const r = await p;
    assert.match(JSON.stringify(r), /late/);
    assert.equal((await srv.ctl.get('/requests')).total, 1);
  });
}
