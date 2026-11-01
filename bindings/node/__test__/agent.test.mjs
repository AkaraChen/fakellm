// Trajectories, policies and custom endpoints through the Responses and Anthropic paths, plus the delivery
// and routing options that only the raw protocols show (args_chunk_bytes, last_user_raw, path prefix).
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { createRequire } from 'node:module'

const { start } = createRequire(import.meta.url)('../index.js')

async function withServer(options, fn) {
  const server = await start(options)
  try {
    await fn(server)
  } finally {
    await server.stop()
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

const responses = (s) => `${s.baseUrl}/responses`
const messages = (s) => `${s.endpoint}/v1/messages`
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
    assert.equal(s.trajectories.list()[0].hits, 2)
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

// The default runtime is the one linked into the addon; 'node' is the subprocess path.
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
      assert.equal(s.policies.list()[0].hits, 4)
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
      const r = await fetch(`${s.endpoint}/anthropic/v1/systemone`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{"state":"angry"}' })
      assert.equal(r.status, 202)
      assert.equal(r.headers.get('x-c'), 'y')
      assert.deepEqual(await r.json(), { mood: 'x' })
      const a = await json(`${s.endpoint}/anthropic/v1/messages`, { model: 'm', max_tokens: 8, messages: [{ role: 'user', content: 'hi' }] }, claude)
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

test('file options load files and are watched by default; watch: false turns it off', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'fakellm-napi-watch-'))
  const file = path.join(dir, 'rules.json')
  const say = async (s) => (await json(messages(s), { model: 'm', max_tokens: 8, messages: [{ role: 'user', content: 'q' }] }, claude)).content[0].text
  const reloaded = async (s, expected) => {
    let got
    for (let i = 0; i < 40 && got !== expected; i++) {
      await new Promise((r) => setTimeout(r, 100))
      got = await say(s)
    }
    return got
  }
  try {
    writeFileSync(file, JSON.stringify([{ reply: { text: 'v1' } }]))
    await withServer({ scriptsFile: file }, async (s) => {
      assert.equal(await say(s), 'v1')
      writeFileSync(file, JSON.stringify([{ reply: { text: 'v2' } }]))
      assert.equal(await reloaded(s, 'v2'), 'v2')
    })
    writeFileSync(file, JSON.stringify([{ reply: { text: 'v1' } }]))
    await withServer({ scriptsFile: file, watch: false }, async (s) => {
      writeFileSync(file, JSON.stringify([{ reply: { text: 'v2' } }]))
      await new Promise((r) => setTimeout(r, 1000))
      assert.equal(await say(s), 'v1')
    })
    // Policy files: named after the file stem.
    const pfile = path.join(dir, 'echoer.mjs')
    writeFileSync(pfile, "export default () => ({ text: 'from policy file' })")
    await withServer({ policyFiles: [pfile], scripts: [{ reply: { policy: 'echoer' } }] }, async (s) => {
      assert.equal(await say(s), 'from policy file')
    })
    await assert.rejects(start({ scriptsFile: path.join(dir, 'missing.json') }), /missing\.json/)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('maxSnapshots and policyTimeoutMs reach the config', () =>
  withServer({ maxSnapshots: 1, policyTimeoutMs: 300, scripts: [{ reply: { policy: 'slow' } }] }, async (s) => {
    s.snapshots.create('one')
    assert.throws(() => s.snapshots.create('two'))
    await s.policies.put([{ name: 'slow', source: 'export default () => new Promise(() => {})' }])
    const r = await post(messages(s), { model: 'm', max_tokens: 8, messages: [{ role: 'user', content: 'x' }] }, claude)
    assert.notEqual(r.status, 200)
  }))

test('recordRedact scrubs the recording but not the reply; match.after sequences rules', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'fakellm-napi-redact-'))
  try {
    await withServer({ scripts: [{ reply: { text: 'the key is secret-123' } }] }, async (upstream) => {
      const record = path.join(dir, 'rec.jsonl')
      await withServer({ upstream: upstream.endpoint, record, recordRedact: ['secret-\\d+'] }, async (s) => {
        const r = await json(`${s.baseUrl}/chat/completions`, { model: 'm', messages: [{ role: 'user', content: 'ask' }] })
        assert.match(r.choices[0].message.content, /secret-123/)
      })
      const recorded = readFileSync(record, 'utf8')
      assert.match(recorded, /<redacted>/)
      assert.doesNotMatch(recorded, /secret-123/)
    })
    await withServer(
      { scripts: [{ name: 'first', match: { last_user: '^go$' }, times: 1, reply: { text: 'one' } }, { match: { after: 'first' }, reply: { text: 'two' } }, { reply: { text: 'zero' } }] },
      async (s) => {
        const say = async (content) => (await json(`${s.baseUrl}/chat/completions`, { model: 'm', messages: [{ role: 'user', content }] })).choices[0].message.content
        assert.deepEqual([await say('x'), await say('go'), await say('x')], ['zero', 'one', 'two'])
      },
    )
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

for (const policyRuntime of [undefined, 'node']) {
test(`a slow policies.put does not make the JS thread wait behind a pending stop() (${policyRuntime ?? 'default runtime'})`, async () => {
  const s = await start({ policyTimeoutMs: 1500, policyRuntime })
  // The module is still loading when the timeout hits, so the load takes the whole timeout.
  const put = s.policies.put([{ name: 'hang', source: 'await new Promise((r) => setTimeout(r, 60000)); export default () => null' }]).catch(() => 'rejected')
  await new Promise((r) => setTimeout(r, 200))
  const stop = s.stop()
  await new Promise((r) => setTimeout(r, 50))
  const t0 = Date.now()
  try {
    s.scripts.list()
  } catch {
    // stopped already: fine
  }
  assert.ok(Date.now() - t0 < 500, `scripts.list blocked for ${Date.now() - t0} ms`)
  await stop
  await put
})
}
