# fakellm（JS / TS 客户端）

在 Node.js、Bun 里启动 [fakellm](../../README.md) 并控制它：注册剧本、读请求日志、操纵服务器时钟。
纯 TypeScript，编译成 ESM + `.d.ts`，没有原生扩展，vitest、bun test、node:test、Playwright（测试进程一侧）都能用。
它做的事就是启动 `fakellm` 二进制、从 stdout 读到地址、再通过 `fetch` 调用[控制面](../../docs/control-plane.md)。

## 安装

目前还没有发布到 npm，从 git 路径安装，并且需要一个 `fakellm` 二进制（`cargo build` 得到 `target/debug/fakellm`）：

```bash
npm install --save-dev "github:akarachen/fakellm#path:sdk/js"
```

二进制的查找顺序：`start({ bin })`，环境变量 `FAKELLM_BIN`，PATH 上的 `fakellm`。指定了却找不到会直接报错，不会悄悄换一个。

## 快速开始

vitest（bun test、node:test 写法相同）：

```ts
import { afterAll, beforeAll, beforeEach, expect, test } from 'vitest';
import OpenAI from 'openai';
import { start, text, toolCall, type FakeLLM } from 'fakellm';

let llm: FakeLLM;
beforeAll(async () => { llm = await start(); });   // 空闲端口，几毫秒
afterAll(() => llm.stop());
beforeEach(() => llm.reset());                      // 每个测试一个干净的服务

test('agent 调用工具', async () => {
  await llm.scripts.put([
    toolCall('read_file', { path: 'a.txt' }, { match: { has_tool_result: false } }),
    text('读完了', { match: { has_tool_result: true } }),
  ]);
  const client = new OpenAI({ baseURL: llm.baseURL, apiKey: 'x' });
  // ... 跑被测代码 ...
  expect(await llm.requests.count({ has_tool: 'read_file' })).toBe(2);
});
```

用时钟控制流：

```ts
await llm.clock.freeze();
await llm.scripts.put([{ reply: { text: 'hi', stream: { ttft_ms: 500 } } }]);
// 发起流式请求：拿到 200，但没有任何 chunk
await llm.clock.advance('500ms');   // 现在首个 chunk 到达
```

给真实工具（claude、codex、opencode）接线：`const { env, files } = await llm.wire('codex')`。

连一个已经在运行的服务（不管理进程）：`const llm = connect('http://127.0.0.1:4010')`。

## API

| 名字 | 说明 |
|---|---|
| `start(options?)` | 启动二进制，返回 `FakeLLM`。选项：`bin`、`host`、`port`（默认 0）、`apiKey`、`models`、`strict`、`scripts`（规则数组，或剧本文件路径）、`ttft`、`tokensPerSec`、`args`、`env`、`readyTimeoutMs` |
| `connect(url)` | 连接已运行的服务；`stop()` 和 `wire()` 对它无效 |
| `llm.url` / `baseURL` / `anthropicBaseURL` / `port` | 地址；`baseURL` 是 `url + '/v1'`（OpenAI SDK），`anthropicBaseURL` 是 `url`（Anthropic SDK 自己加 `/v1`） |
| `llm.scripts` | `put(rules, { keepHits })` 替换，`add(rules)` 追加，`list()`（带 `hits`、`reached`、`exhausted`），`clear()` |
| `llm.requests` | `list(filter?)`、`count(filter?)`、`clear()`、`config(set?)`（读 / 改日志的 `enabled`、`capacity`，返回 `LogConfig`）、`dropped()`；过滤字段与 `GET /_fakellm/requests` 的查询参数同名（`model`、`has_tool`、`stream`、`protocol`、`status`、`source`、`rule`、`path`、`since`、`after`、`limit`、`full`） |
| `llm.clock` | `now()`、`state()`、`freeze(at?)`、`unfreeze()`、`set(at)`、`advance(by)`（`'500ms'`、`'1h30m'` 或毫秒数，可以带小数）、`reset()` |
| `llm.responses` | `list()`、`clear()`（返回删除的个数）：保存的 Responses API 对象 |
| `llm.reset(opts?)` | 清空剧本、请求日志、时钟、Responses；`{ clock: false }` 等保留其中一项 |
| `llm.status()` | `GET /_fakellm` |
| `llm.wire(target, { format? })` | 调用二进制的 `wire` 子命令，默认返回解析后的 JSON（`env`、`files`、`snippet`）；只对 `start()` 出来的实例可用。CLI 的 `--file NAME` 不单独提供：取返回值 `files[NAME]` 即可（Go 的 `WireJSON`、node 绑定的 `wire()` 同理） |
| `llm.ctl(method, path, body?)` | 直接调用控制面，`path` 相对 `/_fakellm` |
| `llm.stop()` | 结束子进程并等它退出，可重复调用；也支持 `await using`。进程退出时（包括测试崩溃）会自动杀掉子进程；子进程不会阻止 Node 退出 |
| `text(t, extra?)`、`toolCall(name, args, extra?)`、`error(status, opts?)` | 构造 `Rule` 的小工具 |

类型 `Rule`、`Match`、`Reply`、`ErrorSpec`、`StreamSpec`、`RequestRecord`、`RequestFilter` 等与[剧本格式](../../docs/scripts.md)的字段一一对应。
控制面出错时抛 `FakeLLMError`，带 `status` 和服务器的错误信息。

注意：控制面会拒绝带 `Origin` 头的非 GET 请求，所以不要在浏览器页面里调用这个包，在测试进程里调用。

## 开发

```bash
npm install
npm test            # tsc 然后 node --test
bun test            # 直接运行 test/*.test.mjs
```

测试默认使用 `../../target/debug/fakellm`，可以用 `FAKELLM_BIN` 覆盖。`sdk/node` 的测试也通过本包启动服务。
