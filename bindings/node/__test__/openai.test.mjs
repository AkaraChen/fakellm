import assert from 'node:assert/strict'
import { test } from 'node:test'
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { createRequire } from 'node:module'
import OpenAI from 'openai'
import Anthropic from '@anthropic-ai/sdk'

const { start } = createRequire(import.meta.url)('../index.js')

async function withServer(options, fn) {
  const server = await start(options)
  try {
    await fn(server)
  } finally {
    await server.stop()
  }
}

const openai = (server) => new OpenAI({ baseURL: server.baseUrl, apiKey: 'test', maxRetries: 0 })
const anthropic = (server) => new Anthropic({ baseURL: server.anthropicBaseUrl, apiKey: 'test', maxRetries: 0 })

test('addresses', () =>
  withServer(undefined, (s) => {
    assert.match(s.endpoint, /^http:\/\/127\.0\.0\.1:\d+$/)
    assert.equal(s.baseUrl, `${s.endpoint}/v1`)
    assert.equal(s.anthropicBaseUrl, s.endpoint)
    assert.equal(s.host, '127.0.0.1')
    assert.notEqual(s.port, 0)
  }))

test('openai sync and stream with a scripted rule', () =>
  withServer({ scripts: [{ match: { last_user: 'ping' }, reply: { text: 'pong' } }] }, async (s) => {
    const c = openai(s)
    const r = await c.chat.completions.create({ model: 'gpt-4o', messages: [{ role: 'user', content: 'ping' }] })
    assert.equal(r.choices[0].message.content, 'pong')

    const stream = await c.chat.completions.create({
      model: 'gpt-4o',
      messages: [{ role: 'user', content: 'ping' }],
      stream: true,
    })
    let text = ''
    for await (const chunk of stream) text += chunk.choices[0]?.delta?.content ?? ''
    assert.equal(text, 'pong')
  }))

test('openai responses', () =>
  withServer(undefined, async (s) => {
    await s.scripts.put([{ reply: { text: 'from responses' } }])
    const r = await openai(s).responses.create({ model: 'gpt-4o', input: 'hi' })
    assert.equal(r.output_text, 'from responses')
    assert.equal(s.requests.list({ protocol: 'openai.responses' }).length, 1)
  }))

test('anthropic sync and stream', () =>
  withServer({ scripts: '- reply: {text: "claude says hi"}' }, async (s) => {
    const c = anthropic(s)
    const m = await c.messages.create({ model: 'claude-sonnet-4-5', max_tokens: 64, messages: [{ role: 'user', content: 'x' }] })
    assert.equal(m.content[0].text, 'claude says hi')

    const stream = c.messages.stream({ model: 'claude-sonnet-4-5', max_tokens: 64, messages: [{ role: 'user', content: 'x' }] })
    assert.equal((await stream.finalMessage()).content[0].text, 'claude says hi')
  }))

test('scripts put / add / list / clear, with hit counts and times', () =>
  withServer(undefined, async (s) => {
    assert.deepEqual(s.scripts.put([{ name: 'once', times: 1, reply: { text: 'first' } }]), { added: 1, count: 1 })
    assert.deepEqual(s.scripts.add('[{"name":"rest","reply":{"text":"later"}}]'), { added: 1, count: 2 })
    const c = openai(s)
    const ask = async () => (await c.chat.completions.create({ model: 'gpt-4o', messages: [{ role: 'user', content: 'q' }] })).choices[0].message.content
    assert.equal(await ask(), 'first')
    assert.equal(await ask(), 'later')
    const rules = s.scripts.list()
    assert.deepEqual(rules.map((r) => [r.name, r.hits, r.exhausted]), [['once', 1, true], ['rest', 1, false]])
    s.scripts.clear()
    assert.deepEqual(s.scripts.list(), [])
    assert.throws(() => s.scripts.put([{ reply: {} }]), /./)
  }))

test('scripted error becomes an SDK error', () =>
  withServer(undefined, async (s) => {
    s.scripts.put([{ reply: { error: { status: 429, message: 'slow down' } } }])
    await assert.rejects(
      openai(s).chat.completions.create({ model: 'gpt-4o', messages: [{ role: 'user', content: 'x' }] }),
      (e) => e instanceof OpenAI.APIError && e.status === 429,
    )
    assert.equal(s.requests.count({ status: '4xx' }), 1)
  }))

test('request log: list, filter, count, clear', () =>
  withServer(undefined, async (s) => {
    const c = openai(s)
    await c.chat.completions.create({ model: 'gpt-4o', messages: [{ role: 'user', content: 'one' }] })
    await c.chat.completions.create({ model: 'gpt-4o-mini', messages: [{ role: 'user', content: 'two' }], stream: true }).then(async (st) => {
      for await (const _ of st);
    })
    const all = s.requests.list()
    assert.equal(all.length, 2)
    assert.equal(all[0].path, '/v1/chat/completions')
    assert.equal(all[0].protocol, 'openai.chat')
    assert.equal(all[0].request.messages.at(-1).content[0].text, 'one')
    assert.equal(s.requests.count({ stream: true }), 1)
    assert.deepEqual(s.requests.list({ model: 'gpt-4o-mini' }).map((r) => r.model), ['gpt-4o-mini'])
    assert.equal(s.requests.list({ limit: 1 })[0].seq, all[1].seq)
    assert.ok(s.requests.list({ full: true })[0].headers.length > 0)
    s.requests.clear()
    assert.equal(s.requests.count(), 0)
  }))

test('clock gating: a frozen clock holds the reply until advanced', () =>
  withServer({ scripts: [{ reply: { text: 'late', delay_ms: 60000 } }] }, async (s) => {
    const start = s.clock.now()
    s.clock.freeze()
    assert.equal(s.clock.isFrozen(), true)
    const pending = openai(s).chat.completions.create({ model: 'gpt-4o', messages: [{ role: 'user', content: 'x' }] })
    let done = false
    pending.then(() => (done = true), () => (done = true))
    await new Promise((r) => setTimeout(r, 300))
    assert.equal(done, false)
    s.clock.advance('61s')
    assert.equal((await pending).choices[0].message.content, 'late')
    assert.ok(s.clock.now() - start >= 60000)

    s.clock.set(new Date('2030-01-01T00:00:00Z'))
    assert.equal(s.clock.now().toISOString(), '2030-01-01T00:00:00.000Z')
    s.clock.unfreeze()
    assert.equal(s.clock.isFrozen(), false)
    s.clock.reset()
    assert.equal(s.clock.isShifted(), false)
    const st = s.clock.freeze(new Date('2031-01-01T00:00:00Z'))
    assert.deepEqual({ ...st, now: st.now.toISOString() }, { now: '2031-01-01T00:00:00.000Z', frozen: true, shifted: true })
    assert.equal(s.clock.advance(1000).now.toISOString(), '2031-01-01T00:00:01.000Z')
    assert.equal(s.clock.state().frozen, true)
    assert.equal(s.clock.reset().shifted, false)
  }))

test('responses list / clear and reset options', () =>
  withServer(undefined, async (s) => {
    const c = openai(s)
    await c.responses.create({ model: 'gpt-4o', input: 'hi' })
    assert.equal(s.responses.list().length, 1)
    assert.equal(s.responses.clear(), 1)
    assert.equal(s.responses.list().length, 0)

    s.scripts.put([{ reply: { text: 'kept' } }])
    await c.responses.create({ model: 'gpt-4o', input: 'hi' })
    s.reset({ scripts: false })
    assert.equal(s.scripts.list().length, 1)
    assert.equal(s.requests.count(), 0)
    assert.equal(s.responses.list().length, 0)
    s.reset()
    assert.equal(s.scripts.list().length, 0)
  }))

test('options: apiKey, strict, models, redactAuth', () =>
  withServer({ apiKey: 'secret', strict: true, models: ['m1', 'm2'], redactAuth: true }, async (s) => {
    await assert.rejects(
      new OpenAI({ baseURL: s.baseUrl, apiKey: 'wrong', maxRetries: 0 }).models.list(),
      (e) => e.status === 401,
    )
    const c = new OpenAI({ baseURL: s.baseUrl, apiKey: 'secret', maxRetries: 0 })
    const ids = []
    for await (const m of c.models.list()) ids.push(m.id)
    assert.deepEqual(ids, ['m1', 'm2'])
    await assert.rejects(
      c.chat.completions.create({ model: 'm1', messages: [{ role: 'user', content: 'x' }] }),
      (e) => e.status === 400 && e.code === 'fakellm_no_matching_rule',
    )
    const rec = s.requests.list({ full: true }).at(-1)
    assert.ok(rec.headers.some(([k, v]) => k === 'authorization' && v === '<redacted>'))
  }))

test('options: ttft delays the first token on the server clock', () =>
  withServer({ ttft: '300ms', tokensPerSec: 1000 }, async (s) => {
    const t0 = Date.now()
    await openai(s).chat.completions.create({ model: 'gpt-4o', messages: [{ role: 'user', content: 'x' }] })
    assert.ok(Date.now() - t0 >= 250)
  }))

test('wire', () =>
  withServer(undefined, (s) => {
    const w = s.wire('claude')
    assert.equal(w.env.ANTHROPIC_BASE_URL, s.endpoint)
    assert.throws(() => s.wire('nope'), /unknown target/)
  }))

test('requestLog: false records nothing; stop is idempotent and makes calls fail', async () => {
  const s = await start({ requestLog: false })
  await openai(s).chat.completions.create({ model: 'gpt-4o', messages: [{ role: 'user', content: 'x' }] })
  assert.equal(s.requests.count(), 0)
  await s.stop()
  await s.stop()
  assert.throws(() => s.scripts.list(), /stopped/)
})

test('servers are independent', async () => {
  const [a, b] = await Promise.all([start(), start()])
  try {
    assert.notEqual(a.port, b.port)
    a.scripts.put([{ reply: { text: 'a' } }])
    assert.equal(b.scripts.list().length, 0)
  } finally {
    await Promise.all([a.stop(), b.stop()])
  }
})

test('status, requests.config and wire format', () =>
  withServer(undefined, async (s) => {
    assert.equal(s.status().request_log.enabled, true)
    assert.deepEqual(s.requests.config({ capacity: 5, enabled: false }).enabled, false)
    assert.equal(s.requests.config().capacity, 5)
    assert.throws(() => s.requests.config({ capacity: 0 }), /capacity/)
    s.requests.config({ enabled: true })
    await openai(s).chat.completions.create({ model: 'gpt-4o', messages: [{ role: 'user', content: 'x' }] })
    assert.equal(s.requests.config().stored, 1)
    assert.match(s.wire('claude', 'shell'), /export ANTHROPIC_BASE_URL=/)
  }))

test('stop closes open connections and hung requests', async () => {
  const s = await start({ scripts: [{ reply: { text: 'x', hang: true } }] })
  const pending = fetch(`${s.baseUrl}/chat/completions`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ model: 'gpt-4o', messages: [{ role: 'user', content: 'x' }] }),
  }).then(
    () => 'answered',
    () => 'failed',
  )
  await new Promise((r) => setTimeout(r, 100))
  await s.stop()
  assert.ok(['answered', 'failed'].includes(await pending))
})

test('options: fallbackTokens, chunkTokens and argsChunkBytes reach the config', () =>
  withServer({ fallbackTokens: 3, chunkTokens: 2 }, async (s) => {
    const r = await openai(s).chat.completions.create({ model: 'm', messages: [{ role: 'user', content: 'hi' }] })
    assert.equal(r.usage.completion_tokens, 3)
    let chunks = 0
    for await (const c of await openai(s).chat.completions.create({ model: 'm', stream: true, messages: [{ role: 'user', content: 'hi' }] })) {
      if (c.choices[0]?.delta?.content) chunks++
    }
    assert.equal(chunks, 2)
  }))

test('a server that is garbage-collected stops listening', async (t) => {
  if (typeof globalThis.gc !== 'function') return t.skip('needs node --expose-gc')
  const url = await (async () => (await start()).baseUrl)()
  for (let i = 0; i < 20; i++) {
    globalThis.gc()
    await new Promise((r) => setTimeout(r, 20))
    if (await fetch(`${url}/models`).then(() => false, () => true)) return
  }
  assert.fail('server still listening after GC')
})

const policySource = "export default () => ({ text: 'from policy' });"

test('trajectories: put/add/list/clear and replay through a rule', () =>
  withServer({ scripts: [{ reply: { trajectory: 't1' } }] }, async (s) => {
    const traj = {
      name: 't1',
      tool_map: { shell: { name: 'Bash', args: { command: '$cmd' } } },
      steps: [
        { reply: { tool_calls: [{ name: 'shell', arguments: { cmd: 'echo hi' }, id: 'c1' }] } },
        { reply: { text: 'all done' }, expect: { tool_results: [{ contains: 'hi' }] } },
      ],
    }
    assert.deepEqual(s.trajectories.put([traj]), { added: 1, count: 1 })
    const c = openai(s)
    const tools = [{ type: 'function', function: { name: 'Bash', parameters: {} } }]
    const user = { role: 'user', content: 'go' }
    const r1 = await c.chat.completions.create({ model: 'm', messages: [user], tools })
    const call = r1.choices[0].message.tool_calls[0]
    assert.equal(call.function.name, 'Bash')
    assert.deepEqual(JSON.parse(call.function.arguments), { command: 'echo hi' })
    const r2 = await c.chat.completions.create({
      model: 'm',
      messages: [user, r1.choices[0].message, { role: 'tool', tool_call_id: 'c1', content: 'hi' }],
      tools,
    })
    assert.equal(r2.choices[0].message.content, 'all done')
    const [info] = s.trajectories.list()
    assert.equal(info.hits, 2)
    assert.equal(s.status().trajectories.count, 1)
    assert.deepEqual(s.trajectories.add(JSON.stringify([{ ...traj, name: 't2' }])), { added: 1, count: 2 })
    assert.throws(() => s.trajectories.put([{ name: 'a/b', steps: [{ reply: { text: 'x' } }] }]), /name/)
    s.trajectories.clear()
    assert.equal(s.trajectories.list().length, 0)
  }))

test('policies: put/add/list/clear and a load error', () =>
  withServer({ scripts: [{ reply: { policy: 'p1' } }] }, async (s) => {
    assert.deepEqual(s.policies.put([{ name: 'p1', source: policySource, runtime: 'node' }]), { added: 1, count: 1 })
    const r = await openai(s).chat.completions.create({ model: 'm', messages: [{ role: 'user', content: 'hi' }] })
    assert.equal(r.choices[0].message.content, 'from policy')
    const [info] = s.policies.list()
    assert.equal(info.runtime, 'node')
    assert.equal(info.hits, 1)
    assert.equal(s.policies.add([{ name: 'p2', source: policySource, runtime: 'node' }]).count, 2)
    assert.throws(() => s.policies.put([{ name: 'bad', source: 'export default 1 +', runtime: 'node' }]))
    assert.equal(s.policies.list().length, 2)
    s.policies.clear()
    assert.equal(s.policies.list().length, 0)
  }))

test('start: trajectories, policies, policyRuntime and pathPrefix', async () => {
  await withServer(
    {
      trajectories: [{ name: 'boot', steps: [{ reply: { text: 'from trajectory' } }] }],
      policies: [{ name: 'pol', source: policySource }],
      policyRuntime: 'node',
      pathPrefix: '/anthropic',
      scripts: [
        { match: { last_user: 'traj' }, reply: { trajectory: 'boot' } },
        { match: { last_user: 'pol' }, reply: { policy: 'pol' } },
      ],
    },
    async (s) => {
      const say = async (content) =>
        (await openai(s).chat.completions.create({ model: 'm', messages: [{ role: 'user', content }] })).choices[0].message.content
      assert.equal(await say('traj'), 'from trajectory')
      assert.equal(await say('pol'), 'from policy')
      assert.equal(s.policies.list()[0].runtime, 'node')
      assert.equal((await fetch(`${s.endpoint}/anthropic/v1/models`)).status, 200)
    },
  )
  await assert.rejects(start({ policyRuntime: 'node', policies: [{ name: 'bad', source: 'export default 1 +' }] }))
})

const tape = (name, user, answer) => ({
  kind: 'cassette',
  name,
  exchanges: [
    {
      request: { model: 'm', messages: [{ role: 'user', content: [{ type: 'text', text: user }] }] },
      outcome: { outcome: 'reply', reply: { text: answer, finish: 'stop', usage: { prompt_tokens: 5, completion_tokens: 2 } } },
      protocol: 'openai.chat',
    },
  ],
})
const say = async (s, content) =>
  (await openai(s).chat.completions.create({ model: 'm', messages: [{ role: 'user', content }] })).choices[0].message.content

test('cassettes: put/add/list/clear and replay', () =>
  withServer(undefined, async (s) => {
    assert.deepEqual(s.cassettes.put(tape('c1', 'capital?', 'Paris')), { added: 1, count: 1 })
    assert.equal(await say(s, 'capital?'), 'Paris')
    assert.deepEqual(s.cassettes.add([tape('c2', 'other?', 'Rome')]), { added: 1, count: 2 })
    const list = s.cassettes.list()
    assert.deepEqual(list.map((c) => [c.name, c.exchanges, c.recording]), [['c1', 1, false], ['c2', 1, false]])
    assert.equal(list[0].hits, 1)
    assert.equal(s.status().cassettes.count, 2)
    assert.throws(() => s.cassettes.put([tape('a/b', 'x', 'y')]), /name/)
    assert.equal(s.cassettes.list().length, 2)
    s.cassettes.clear()
    assert.equal(s.cassettes.list().length, 0)
  }))

test('snapshots: create/list/restore/delete', () =>
  withServer({ scripts: [{ reply: { text: 'one' } }] }, async (s) => {
    s.cassettes.put(tape('c1', 'capital?', 'Paris'))
    const snap = s.snapshots.create('base')
    assert.equal(snap.name, 'base')
    assert.equal(snap.scripts, 1)
    assert.equal(snap.cassettes, 1)
    assert.throws(() => s.snapshots.create('base'), /exists/)
    s.scripts.put([{ reply: { text: 'two' } }])
    s.cassettes.clear()
    assert.equal(await say(s, 'x'), 'two')
    assert.equal(s.snapshots.restore('base').name, 'base')
    assert.equal(await say(s, 'x'), 'one')
    assert.equal(s.cassettes.list().length, 1)
    assert.ok(s.snapshots.create().name.startsWith('snapshot-'))
    assert.equal(s.snapshots.list().length, 2)
    assert.equal(s.status().snapshots.count, 2)
    s.reset()
    assert.equal(s.snapshots.list().length, 2, 'reset keeps snapshots')
    s.snapshots.delete('base')
    assert.throws(() => s.snapshots.restore('base'), /no snapshot/)
    s.snapshots.delete()
    assert.equal(s.snapshots.list().length, 0)
    s.snapshots.create('y')
    s.reset({ snapshots: true })
    assert.equal(s.snapshots.list().length, 0)
  }))

test('start: cassettes, upstream and record', () =>
  withServer({ scripts: [{ reply: { text: 'UPSTREAM_SAYS_HI' } }] }, async (up) => {
    const dir = mkdtempSync(path.join(tmpdir(), 'fakellm-napi-'))
    const rec = path.join(dir, 'rec.jsonl')
    try {
      await withServer({ upstream: up.endpoint, record: rec, cassettes: [tape('boot', 'capital?', 'Paris')] }, async (proxy) => {
        assert.equal(await say(proxy, 'capital?'), 'Paris')
        assert.equal(await say(proxy, 'something new'), 'UPSTREAM_SAYS_HI')
        const st = proxy.status()
        assert.equal(st.upstream.recorded, 1)
        assert.ok(proxy.cassettes.list().some((c) => c.recording))
      })
      await withServer({ strict: true, cassettes: readFileSync(rec, 'utf8') }, async (replay) => {
        assert.equal(await say(replay, 'something new'), 'UPSTREAM_SAYS_HI')
      })
    } finally {
      rmSync(dir, { recursive: true, force: true })
    }
  }))

test('start: watch reloads a cassette file', () =>
  withServer(undefined, async () => {
    const dir = mkdtempSync(path.join(tmpdir(), 'fakellm-napi-'))
    const file = path.join(dir, 'tape.json')
    writeFileSync(file, JSON.stringify([tape('t', 'q?', 'before')]))
    try {
      await withServer({ cassettes: readFileSync(file, 'utf8'), watch: { cassettes: [file] } }, async (s) => {
        assert.equal(await say(s, 'q?'), 'before')
        writeFileSync(file, JSON.stringify([tape('t', 'q?', 'after the edit')]))
        let got
        for (let i = 0; i < 50 && got !== 'after the edit'; i++) {
          await new Promise((r) => setTimeout(r, 100))
          got = await say(s, 'q?')
        }
        assert.equal(got, 'after the edit')
      })
    } finally {
      rmSync(dir, { recursive: true, force: true })
    }
  }))

test('custom endpoint rule: match.path/body and reply body/status/headers', () =>
  withServer(undefined, async (s) => {
    s.scripts.put([{ match: { path: '/v1/systemone', body: 'angry' }, reply: { body: { mood: 'x' }, status: 202, headers: { 'x-c': 'y' } } }])
    const r = await fetch(`${s.endpoint}/v1/systemone`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{"state":"angry"}' })
    assert.equal(r.status, 202)
    assert.equal(r.headers.get('x-c'), 'y')
    assert.deepEqual(await r.json(), { mood: 'x' })
  }))
