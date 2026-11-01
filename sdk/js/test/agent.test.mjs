// Trajectories, policies and custom endpoints through the Responses and Anthropic paths, plus the delivery
// and routing options that only the raw protocols show (argsChunkBytes, last_user_raw, path prefix).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { start } from '../dist/index.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const bin = process.env.FAKELLM_BIN ?? path.resolve(here, '../../../target/debug/fakellm');

async function withServer(options, fn) {
  const server = await start({ bin, ...options });
  try {
    await fn(server);
  } finally {
    await server.stop();
  }
}

const post = async (url, body, headers = {}) => {
  const r = await fetch(url, { method: 'POST', headers: { 'content-type': 'application/json', ...headers }, body: JSON.stringify(body) })
  return { status: r.status, text: await r.text() }
}
const json = async (url, body, headers) => {
  const r = await post(url, body, headers)
  assert.equal(r.status, 200, r.text)
  return JSON.parse(r.text)
}

const responses = (s) => `${s.baseURL}/responses`
const messages = (s) => `${s.url}/v1/messages`
const claude = { 'anthropic-version': '2023-06-01' }

// One tool round trip per protocol: the request that asks, and the one that carries the result back.
const rspTools = [{ type: 'function', name: 'Bash', parameters: { type: 'object' } }]
const rspAsk = (user) => ({ model: 'gpt-5', input: [{ role: 'user', content: user }], tools: rspTools })
const rspAnswer = (user, output) => ({
  model: 'gpt-5',
  input: [
    { role: 'user', content: user },
    { type: 'function_call', call_id: 'c1', name: 'Bash', arguments: '{"command":"echo hi"}' },
    { type: 'function_call_output', call_id: 'c1', output },
  ],
  tools: rspTools,
})
const antTools = [{ name: 'Bash', input_schema: { type: 'object' } }]
const antAsk = (user) => ({ model: 'claude-sonnet-4-5', max_tokens: 64, messages: [{ role: 'user', content: user }], tools: antTools })
const antAnswer = (user, output) => ({
  model: 'claude-sonnet-4-5',
  max_tokens: 64,
  messages: [
    { role: 'user', content: user },
    { role: 'assistant', content: [{ type: 'tool_use', id: 'c1', name: 'Bash', input: { command: 'echo hi' } }] },
    { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'c1', content: output }] },
  ],
  tools: antTools,
})

const trajectory = {
  name: 'walk',
  tool_map: { shell: { name: 'Bash', args: { command: '$cmd' } } },
  steps: [
    { reply: { tool_calls: [{ name: 'shell', arguments: { cmd: 'echo hi' }, id: 'c1' }] } },
    { reply: { text: 'all done' }, expect: { tool_results: [{ contains: 'hi' }] } },
  ],
}

test('trajectory over the Responses API', () =>
  withServer({ scripts: [{ reply: { trajectory: 'walk' } }], trajectories: [trajectory] }, async (s) => {
    const first = await json(responses(s), rspAsk('go'))
    const call = first.output.find((o) => o.type === 'function_call')
    assert.equal(call.name, 'Bash')
    assert.deepEqual(JSON.parse(call.arguments), { command: 'echo hi' })
    const second = await json(responses(s), rspAnswer('go', 'hi'))
    assert.equal(second.output_text ?? second.output.at(-1).content[0].text, 'all done')
    assert.equal((await s.trajectories.list())[0].hits, 2)
    const bad = await post(responses(s), rspAnswer('go', 'nope'))
    assert.equal(bad.status, 400)
    assert.match(bad.text, /fakellm_trajectory_diverged/)
  }))

test('trajectory over the Anthropic Messages API', () =>
  withServer({ scripts: [{ reply: { trajectory: 'walk' } }], trajectories: [trajectory] }, async (s) => {
    const first = await json(messages(s), antAsk('go'), claude)
    const use = first.content.find((b) => b.type === 'tool_use')
    assert.equal(use.name, 'Bash')
    assert.deepEqual(use.input, { command: 'echo hi' })
    assert.equal(first.stop_reason, 'tool_use')
    const second = await json(messages(s), antAnswer('go', 'hi'), claude)
    assert.equal(second.content[0].text, 'all done')
    const bad = await post(messages(s), antAnswer('go', 'nope'), claude)
    assert.equal(bad.status, 400)
    assert.match(bad.text, /diverged at step 1/)
  }))

const policySource = `export default (request, context) => {
  const last = request.messages.at(-1)
  if (last.role === 'tool') return { text: 'policy saw: ' + last.content.map((p) => p.text ?? '').join('') }
  return { tool_calls: [{ name: 'Bash', arguments: { command: 'echo hi' }, id: 'c1' }] }
}`

// The default runtime is the one embedded in the binary; 'node' is the subprocess path.
for (const policyRuntime of [undefined, 'node']) {
  test(`policy over the Responses API and the Anthropic Messages API (${policyRuntime ?? 'default runtime'})`, () =>
    withServer({ scripts: [{ reply: { policy: 'loop' } }], policyRuntime }, async (s) => {
      assert.deepEqual(await s.policies.put([{ name: 'loop', source: policySource }]), { added: 1, count: 1 })
      const ask = await json(responses(s), rspAsk('go'))
      assert.equal(ask.output.find((o) => o.type === 'function_call').name, 'Bash')
      const done = await json(responses(s), rspAnswer('go', 'hi'))
      assert.match(JSON.stringify(done.output), /policy saw: hi/)

      const ask2 = await json(messages(s), antAsk('go'), claude)
      assert.equal(ask2.content.find((b) => b.type === 'tool_use').name, 'Bash')
      const done2 = await json(messages(s), antAnswer('go', 'hi'), claude)
      assert.equal(done2.content[0].text, 'policy saw: hi')
      assert.equal((await s.policies.list())[0].hits, 4)
    }))
}

test('a policy can decline and let the next rule answer', () =>
  withServer({ scripts: [{ reply: { policy: 'picky' } }, { reply: { text: 'fallback rule' } }] }, async (s) => {
    await s.policies.put([{ name: 'picky', source: 'export default (r) => (r.tools?.length ? { text: "policy" } : null)' }])
    const withTools = await json(messages(s), { ...antAsk('x') }, claude)
    assert.equal(withTools.content[0].text, 'policy')
    const plain = await json(messages(s), { model: 'claude-sonnet-4-5', max_tokens: 8, messages: [{ role: 'user', content: 'x' }] }, claude)
    assert.equal(plain.content[0].text, 'fallback rule')
  }))

test('custom endpoints: bodies, per-protocol routing and the Anthropic path prefix', () =>
  withServer(
    {
      pathPrefix: '/anthropic',
      scripts: [
        { match: { path: '/v1/systemone', body: 'angry' }, reply: { body: { mood: 'x' }, status: 202, headers: { 'x-c': 'y' } } },
        { match: { protocol: 'anthropic.messages' }, reply: { text: 'via anthropic' } },
        { match: { protocol: 'openai.responses' }, reply: { text: 'via responses' } },
      ],
    },
    async (s) => {
      const r = await fetch(`${s.url}/anthropic/v1/systemone`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{"state":"angry"}' })
      assert.equal(r.status, 202)
      assert.equal(r.headers.get('x-c'), 'y')
      assert.deepEqual(await r.json(), { mood: 'x' })
      const a = await json(`${s.url}/anthropic/v1/messages`, { model: 'm', max_tokens: 8, messages: [{ role: 'user', content: 'hi' }] }, claude)
      assert.equal(a.content[0].text, 'via anthropic')
      const o = await json(responses(s), { model: 'gpt-5', input: 'hi' })
      assert.equal(o.output_text ?? o.output[0].content[0].text, 'via responses')
    },
  ))

test('argsChunkBytes splits streamed tool arguments on both protocols', async () => {
  const rule = { reply: { tool_calls: [{ name: 'Bash', arguments: { command: 'echo a-fairly-long-command-line' } }] } }
  await withServer({ scripts: [rule], argsChunkBytes: 4 }, async (s) => {
    const rsp = await fetch(responses(s), { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ ...rspAsk('go'), stream: true }) })
    const deltas = (await rsp.text()).split('\n').filter((l) => l.includes('"type":"response.function_call_arguments.delta"'))
    assert.ok(deltas.length > 3, `${deltas.length} deltas`)
    const ant = await fetch(messages(s), { method: 'POST', headers: { 'content-type': 'application/json', ...claude }, body: JSON.stringify({ ...antAsk('go'), stream: true }) })
    const parts = (await ant.text()).split('\n').filter((l) => l.includes('input_json_delta'))
    assert.ok(parts.length > 3, `${parts.length} deltas`)
  })
})

test('last_user_raw matches the user message as sent, reminder blocks included', () =>
  withServer(
    { scripts: [{ match: { last_user_raw: '^<system-reminder>' }, reply: { text: 'raw' } }, { reply: { text: 'cooked' } }] },
    async (s) => {
      const body = (content) => ({ model: 'claude-sonnet-4-5', max_tokens: 8, messages: [{ role: 'user', content }] })
      const raw = await json(messages(s), body([{ type: 'text', text: '<system-reminder>hi</system-reminder>' }, { type: 'text', text: 'real question' }]), claude)
      assert.equal(raw.content[0].text, 'raw')
      const plain = await json(messages(s), body('real question'), claude)
      assert.equal(plain.content[0].text, 'cooked')
    },
  ))

test('match.after sequences rules by another rule\'s hits', () =>
  withServer(
    {
      scripts: [
        { name: 'first', match: { last_user: '^go$' }, times: 1, reply: { text: 'one' } },
        { match: { after: 'first', after_count: 1 }, reply: { text: 'two' } },
        { reply: { text: 'zero' } },
      ],
    },
    async (s) => {
      const say = async (content) =>
        (await json(`${s.baseURL}/chat/completions`, { model: 'm', messages: [{ role: 'user', content }] })).choices[0].message.content;
      assert.equal(await say('other'), 'zero');
      assert.equal(await say('go'), 'one');
      assert.equal(await say('other'), 'two');
    },
  ));

test('cassette.sample serves a corpus reply for a prompt it has not seen', () =>
  withServer({}, async (s) => {
    await s.cassettes.put([
      {
        name: 'corpus',
        sample: true,
        exchanges: [
          {
            request: { model: 'm', messages: [{ role: 'user', content: [{ type: 'text', text: 'recorded question' }] }] },
            outcome: { outcome: 'reply', reply: { text: 'from corpus', finish: 'stop', usage: { prompt_tokens: 5, completion_tokens: 2 } } },
          },
        ],
      },
    ]);
    const r = await json(`${s.baseURL}/chat/completions`, { model: 'm', messages: [{ role: 'user', content: 'something else entirely' }] });
    assert.equal(r.choices[0].message.content, 'from corpus');
  }));

test('recordRedact, trajectoryFiles and policyFiles reach the binary', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'fakellm-sdk-'));
  try {
    await withServer({ scripts: [{ reply: { text: 'the key is secret-123' } }] }, async (upstream) => {
      const record = path.join(dir, 'rec.jsonl');
      const traj = path.join(dir, 'walk.json');
      writeFileSync(traj, JSON.stringify({ name: 'walk', steps: [{ reply: { text: 'from file' } }] }));
      const pol = path.join(dir, 'filer.mjs');
      writeFileSync(pol, "export default () => ({ text: 'policy file' })");
      await withServer(
        { upstream: upstream.url, record, recordRedact: ['secret-\\d+'], trajectoryFiles: [traj], policyFiles: [pol] },
        async (s) => {
          const r = await json(`${s.baseURL}/chat/completions`, { model: 'm', messages: [{ role: 'user', content: 'ask' }] });
          assert.match(r.choices[0].message.content, /secret-123/); // the client still gets the real reply
          assert.equal((await s.trajectories.list())[0].name, 'walk');
          assert.equal((await s.policies.list())[0].name, 'filer');
        },
      );
      const recorded = readFileSync(record, 'utf8');
      assert.match(recorded, /<redacted>/);
      assert.doesNotMatch(recorded, /secret-123/);
    });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
