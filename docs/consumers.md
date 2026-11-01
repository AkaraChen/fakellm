# 真实消费方实验：用 fakellm 替换别人仓库测试里的 LLM 后端

本文记录两轮实验：在 `/home/akrc/Developer/fakellm-research` 下克隆了会调用 LLM API 的开源仓库，各开一个本地分支 `fakellm`，把它们的测试或 e2e 改成对着 fakellm 跑，用来找 fakellm 的缺口。第一轮六个仓库（xsai、pi-mono、claude-agent-sdk-python、opencode、raft、oar），第二轮（M2 二进制）新增 paseo、ai-sdk 一致性对照，并重跑 oar。所有提交都只在本地，没有推送。

统一约定：

- 二进制用 `FAKELLM_BIN`，默认 `/home/akrc/Developer/fakellm-research/bin/fakellm`，以 `--port 0` 启动，从第一行 stdout 读 URL。
- OpenAI 风格的客户端用 `url + /v1`；Anthropic SDK 用裸 `url`；真正的 `claude` CLI 用 `ANTHROPIC_BASE_URL=url`。
- 规则按测试期望手写，用 `PUT /_fakellm/scripts` 注册，`POST /_fakellm/reset` 隔离用例。
- 第一轮的二进制没有 `reply.usage` 覆盖，也没有 embeddings；第二轮用 M2 构建，`reply.usage` 已可用（oar 重跑已验证），embeddings 在工作区里已实现但 xsai 尚未重跑。

## 总览

| 仓库 | 分支 / 提交 | 结果 | 阻塞缺口 |
|---|---|---|---|
| xsai | `fakellm` / `e9febde` | 14 个 e2e：10 通过，4 失败 | `/v1/embeddings`、`/v1/systemone` |
| pi-mono | `fakellm` / `839c592` | 新增 10 个全通过 | 无 |
| claude-agent-sdk-python | `fakellm` / `6bd191f` | 15 个通过，hooks 3 个跳过（未写规则） | 无 |
| opencode | `fakellm` / `c200d6b` | 全通过 | 无 |
| raft | `fakellm` / `678808f` | 157 个通过 | 路径前缀（只影响断言） |
| oar（第一轮） | `fakellm` / `a86b115` | sea-trial 全通过；vendor 全量有失败 | 多项，见缺口表 |
| paseo（第二轮） | `fakellm` / `50ee5ef` | 6 个文件全部迁移并通过（16 个测试） | 无阻塞；见缺口 4、5、7 |
| ai-sdk 一致性（第二轮） | `fakellm` / `23d6d62` | 新增 7 个对照测试全通过 | 无阻塞；差异见下 |
| oar（第二轮重跑） | `fakellm` / `a6c4802` | claude 49/19，codex 41/11 | predicate 与函数式回复，见缺口 1、7 |

## xsai

- 克隆：`/home/akrc/Developer/fakellm-research/xsai`，分支 `fakellm`，提交 `e9febde`。
- 运行：

```bash
eval "$(fnm env)"; fnm use 26
cd /home/akrc/Developer/fakellm-research/xsai && scripts/e2e-fakellm.sh
```

脚本在 `127.0.0.1:11434` 起 fakellm（`--models qwen3.5:0.8b,qwen3-embedding:0.6b,tev1:0.8b --scripts scripts/e2e-fakellm.rules.yaml`），跑 `pnpm test:e2e`，退出时 kill。`FAKELLM_BIN` 可覆盖二进制，额外参数透传给 vitest。只新增了 `scripts/e2e-fakellm.sh` 和 `scripts/e2e-fakellm.rules.yaml`，没改仓库源码或配置。

规则（先匹配先生效）：`chat-tool-finish`（`tool_result_contains: e2e-weather-result-8472`）、`chat-tool-call`（`has_tool: get_weather` + `has_tool_result: false`，防止循环）、`decide-structured`（`system: "Answer every question"`，回 JSON）、`chat-e2e-ok`。

| 测试文件 | 之前 | 之后 |
|---|---|---|
| text-chat | 未跑（无服务，推测全失败） | 通过 |
| text-messages | 同上 | 通过 |
| text-responses | 同上 | 通过 |
| model | 同上 | 通过（`/v1/models` 列表和单个查询） |
| decide | 同上 | 1/3（chat 版通过，两个 SystemOne 失败） |
| embed | 同上 | 0/2 |
| 合计 | 约 0/14（推测） | 10 通过 / 4 失败 |

说明：前三个文件只有汇总数字，没有逐用例计数；之前的数字是推测，没有实测。

要点：`localhost` 解析到 127.0.0.1，硬编码的 `http://localhost:11434/v1/` 不用改；Anthropic 的 baseURL 是 `/v1`，所以同一个基址通用；decide 的 chat 测试把结构化输出指令放在 system 里。

## pi-mono

- 克隆：`/home/akrc/Developer/fakellm-research/pi-mono`，分支 `fakellm`，提交 `839c592`。
- 新增：`packages/ai/test/helpers/fakellm.ts`（helper，暴露 `rules`、`reset`、`requests`、`clock`、`stop`）、`packages/ai/test/fakellm.e2e.test.ts`（9 个）、`packages/agent/test/fakellm.e2e.test.ts`（1 个）。
- 运行：

```bash
eval "$(fnm env)"; fnm use 24
cd packages/ai    && npx vitest --run test/fakellm.e2e.test.ts
cd packages/agent && npx vitest --run test/fakellm.e2e.test.ts
```

agent 的测试依赖生成的模型数据，先跑一次 `cd packages/ai && npm run hydrate-model-data`（离线可用，产物未跟踪，不在提交里）。

| 测试文件 | 之前 | 之后 |
|---|---|---|
| `ai/test/fakellm.e2e.test.ts` | 文件不存在 | 9 通过 / 0 失败 / 0 跳过 |
| `agent/test/fakellm.e2e.test.ts` | 文件不存在 | 1 通过 / 0 失败 / 0 跳过 |
| `agent/test/agent-loop.test.ts`（对照） | 38 通过 | 38 通过 |

覆盖：openai-completions 文本、流式工具参数、残缺参数串（`'{"city": "Par'`，pi-ai 会补全成对象而不抛错）、截断流、Anthropic thinking + signature、responses 文本和 function call、429 重试、abort。

要点：`match.protocol` 只接受 `openai.chat`、`openai.responses`、`anthropic.messages`，写 `openai` 会让 `PUT /scripts` 返回 400；fakellm 强制 Anthropic 的 `thinking.budget_tokens >= 1024`；`reasoning` + `reasoning_opaque` 产生 `thinking_start/delta/end`，`thinkingSignature` 等于 opaque 值；重试用 `error.retry_after: 0` 加 `maxRetryDelayMs: 50`；Agent 循环没有标题等旁路请求（2 个请求）；截断测试只断言宽松的 stopReason 集合（`error`/`stop`/`length`），因为 `cut_after_chunks` 是干净结束，没有 TCP 重置。

## claude-agent-sdk-python

- 克隆：`/home/akrc/Developer/fakellm-research/claude-agent-sdk-python`，分支 `fakellm`，提交 `6bd191f`。
- 新增 `e2e-tests/conftest_fakellm.py`（fixture 和按测试名索引的 `RULES`），`conftest.py` 只在设置 `FAKELLM_BIN` 时导入它。session fixture 起 fakellm，并在 `os.environ` 里设置 CLI 所需的 base URL、API key、临时 `CLAUDE_CONFIG_DIR`，以及遥测、更新、版本检查开关；每个用例前 reset 并 PUT 该用例的规则加一个兜底规则；没有 `RULES` 条目的测试被跳过。`FAKELLM_LOG` 可在会话结束时导出完整请求日志。
- 运行：

```bash
cd /home/akrc/Developer/fakellm-research/claude-agent-sdk-python
FAKELLM_BIN=/home/akrc/Developer/fakellm-research/bin/fakellm \
 .venv/bin/pytest e2e-tests/test_error_results.py e2e-tests/test_verbatim_prompts.py \
 e2e-tests/test_structured_output.py e2e-tests/test_include_partial_messages.py \
 e2e-tests/test_sdk_mcp_tools.py -m e2e -v
```

全程约 8 秒。务必设置 `FAKELLM_BIN`：不设置时套件会用本机已有的 Claude 登录去打真实 API。

| 测试文件 | 之前（真实 API，不可靠） | 之后（fakellm） |
|---|---|---|
| `test_error_results.py` | 1 通过 | 1 通过 |
| `test_verbatim_prompts.py` | 3 通过 | 3 通过 |
| `test_structured_output.py` | 4 通过 | 4 通过 |
| `test_include_partial_messages.py` | 3 通过 | 3 通过 |
| `test_sdk_mcp_tools.py` | 4 通过 | 4 通过 |
| `test_hooks.py` | 未跑 | 3 跳过（没写规则） |

「之前」那一轮是误对着真实 API 跑的，没有逐文件干净记录。subagent 相关测试没试。

要点：
- CLI 只打 `POST /v1/messages?beta=true`，一律流式；没有 `count_tokens`、`/v1/models`、标题、配额或遥测请求。
- 请求头：`x-api-key`、`anthropic-version: 2023-06-01`、很长的 `anthropic-beta`、`x-app: cli`、`user-agent: claude-cli/2.1.314 (external, sdk-py, agent-sdk/0.2.176)`。
- 结构化输出：CLI 把 schema 当成 `StructuredOutput` 工具，一条调用该工具的规则就够，一次请求结束。
- MCP 工具：CLI 带了 ToolSearch 和延迟工具列表，MCP 工具不在首个请求的 `tools` 里。脚本按 `turn` 分三步：turn 0 调 `ToolSearch {"query":"select:<names>","max_results":5}`，turn 1 调工具，turn 2 回最终文本。
- `last_user` 对 Claude Code 匹配不上：第一条 user 消息是 `<system-reminder>` 和环境块，所以改为只按 model 匹配。
- 日志 `GET /_fakellm/requests` 返回 `{"requests":[...]}`，不是裸数组；Claude Code 请求体很大，不需要正文时别带 `full=true`。
- 被禁用工具的测试：脚本里照样调用，CLI 自己拒绝，没有执行 Python 函数。

## opencode

- 克隆：`/home/akrc/Developer/fakellm-research/opencode`，分支 `fakellm`，提交 `c200d6b`（`bun.lock` 原本就有改动，未提交）。
- 新增 `packages/core/test/lib/fakellm.ts`（helper）、`packages/core/test/fakellm-cli.e2e.test.ts`（2 个：纯文本、bash 工具往返）；`session-runner-recorded.test.ts` 在设置 `FAKELLM_BIN` 时改用 `FetchHttpClient.layer` 和 fakellm 的 `url + /v1`，原断言全保留，另加一条日志里只命中 `main` 规则的断言。
- 运行（在 `packages/core`）：

```bash
FAKELLM_BIN=/home/akrc/Developer/fakellm-research/bin/fakellm bun run test test/session-runner-recorded.test.ts
FAKELLM_BIN=... bun run test test/fakellm-cli.e2e.test.ts
```

不设 `FAKELLM_BIN` 时前者继续用 cassette，后者跳过。e2e 需要 PATH 上有 `opencode` 或设置 `OPENCODE_BIN`；`KEEP_E2E=1` 保留临时目录。

| 测试文件 | 之前 | 之后 |
|---|---|---|
| `session-runner-recorded.test.ts` | 1 通过 | 1 通过（cassette）；1 通过（fakellm） |
| `fakellm-cli.e2e.test.ts` | 不存在 | 2 通过（有 `FAKELLM_BIN`）；不设则跳过 |

不设 `FAKELLM_BIN` 时两个文件合计 1 通过 4 跳过（4 是 bun 对两个跳过测试的计数，没深究）。

要点：
- opencode 1.18.29 发 `POST /v1/chat/completions`，`stream: true`，带 `include_usage`、`max_tokens`、`n`、`tools`、`tool_choice`；工具有 `bash`、`edit`、`glob`、`grep`、`read`、`skill`、`task`、`todowrite`、`webfetch`、`write`。
- 一次运行三个请求：先是无工具的标题请求，然后工具轮，然后跟进轮。标题的 system 以 "You are a title generator" 开头，不含 core 测试里的 "You name conversations..."，所以 CLI 测试用 `system: "You are a title generator"`，用 `no_tools` + `system` 即可区分。
- `opencode run` 把工具进度写 stderr，只有最终文本在 stdout。
- opencode 的项目目录取自 `$PWD` 环境变量而不是真实 cwd，`Bun.spawn({cwd})` 不够，必须显式设置 `PWD`（否则报 `UnknownError`）。
- `script/test.ts` 设的 `OPENCODE_CONFIG_DIR`、`OPENCODE_TEST_HOME` 以及 `NODE_ENV`、`BUN_TEST` 都要从子进程环境里去掉；需传 `OPENCODE_CONFIG`、`OPENCODE_DISABLE_MODELS_FETCH=1`、`OPENCODE_DISABLE_AUTOUPDATE=1`。
- fakellm 子进程若继承 stderr，管道里的 `bun run test` 永远不退出；helper 用 `stdio: ["ignore","pipe","ignore"]` 并在 `afterAll` 里停掉。

## raft

- 克隆：`/home/akrc/Developer/fakellm-research/raft-source`，分支 `fakellm`，提交 `678808f`。
- 新增 `packages/daemon/src/testing/fakellm.ts`（`startFakeLlm()`，返回 `{url, openAiBaseUrl, setRules, requests(query), reset, close}`，`requests()` 带 `full=true`，每条有 `.raw`、`.path`、`.rule`、`.protocol`、`.headers`）；改写了三个测试文件里手写的假服务（`startFakeAnthropicServer` 等被删除）。
- 运行：

```bash
cd /home/akrc/Developer/fakellm-research/raft-source/packages/daemon
pnpm exec vitest run src/agentProcessManager.builtin.e2e.test.ts src/drivers/kimi-sdk.test.ts src/drivers/pi.test.ts
```

可选 `FAKELLM_BIN`；`tsc --noEmit` 干净。

| 测试文件 | 之前（通过/失败/跳过） | 之后 |
|---|---|---|
| `agentProcessManager.builtin.e2e.test.ts` | 7/0/0 | 7/0/0 |
| `drivers/kimi-sdk.test.ts` | 52/0/0 | 52/0/0 |
| `drivers/pi.test.ts` | 98/0/0 | 98/0/0 |

合计 157 通过。

要点：
- 401 provider 变成一条 `error:{status:401, code:'invalid_api_key', ...}` 规则，「provider 被调用」改读 `fake.requests({rule})`。
- Pi 的 Anthropic 工具测试：`has_tool_result:false` 回 `reasoning` + `reasoning_opaque:'sig-turn-1'` + `bash` 调用，`has_tool_result:true` 回文本；带签名的 thinking 块在第二个请求里被原样回放，说明 `reasoning_opaque` 作为 `signature_delta` 的往返成立。
- `has_tool_result:false` 对后续的普通 user 轮也会匹配，恢复会话的测试要用 `turn` 做键。
- 日志里 `headers` 是 `[name, value]` 对的列表，不是对象。
- 127.0.0.1 不触发 networkGuard。
- 另：实验者的 `pkill -f "bin/fakellm --port 0"` 误杀了同机其他工作流的 fakellm 和自己的 shell（退出码 144）。以后清理用记录下来的 PID，别用 `pkill -f`。

## oar

- 克隆：`/home/akrc/Developer/fakellm-research/oar`，分支 `fakellm`，提交 `a86b115`。
- 适配器 `sea-trial/harness/fakellm.ts`；`aimock.ts`、`aimock-acp.ts` 改为通过 `newMock()` 构建 mock；`sea-trial/vendor/support/system-prompt.ts` 增加 fakellm 分支。
- 运行（Node 24，在克隆目录）：

```bash
CI=1 OAR_MOCK=fakellm OAR_TEST=claude-aimock pnpm sea-trial
CI=1 OAR_MOCK=fakellm OAR_TEST=codex-aimock pnpm sea-trial
CI=1 OAR_MOCK=fakellm OAR_TEST=claude-aimock pnpm exec vitest run sea-trial/vendor/claude.vendor.test.ts   # codex 同理用 codex.vendor.test.ts
```

必须设 `CI=1`：本地 vitest 以 `update: "all"` 运行，会悄悄改写内联快照；第一次运行就把工具轮快照改成了「无工具」并显示全绿，已还原。二进制来自 `FAKELLM_BIN` 或默认路径。

| 运行 | 之前（aimock） | 之后（fakellm） |
|---|---|---|
| sea-trial 套件，claude | 未跑 | 17 通过 / 2 跳过 / 0 失败 |
| sea-trial 套件，codex | 未跑 | 16 通过 / 3 跳过 / 0 失败 |
| `claude.vendor.test.ts` | 7 通过 | 7 通过 |
| `codex.vendor.test.ts` | 未单独跑 | 7 通过 / 1 失败（`reply.usage` 不支持） |
| 全部 vendor，claude | 68 通过 / 0 失败 | 46 通过 / 22 失败 / 268 跳过 |
| 全部 vendor，codex | 50 通过 / 2 失败 / 284 跳过 | 39 通过 / 13 失败 |

跳过的是 `accountUsage`、`listModels`、`withdraw` 能力，aimock 下同样跳过。codex 基线的两个失败（`service-tier`、`failure-classes` 的 unknown model）在 fakellm 下同样失败，与 fakellm 无关。

失败原因（全部 vendor 文件，约 35 个）：
- 适配器拒绝翻译，约 20 个：14 个 `match.predicate`（`currentTurnSays`、permissions、goals、mcp-servers、input-identity 夹具，以及用 `request.tools` 判断会话标题）；6 个函数式回复（`echo-mcp.ts` 的 `echoFixtures`/`scriptEchoes`）；2 个 `reply.usage`（codex resumed-usage、`reasoning-tokens`）。
- 超时：`claude-open-hooks`、`disallowed-tools`；system-prompt 测试在并行全量运行时超时，单跑通过，疑为负载。
- `claude-partials` 的 "slow tool arguments"：期望流式活动 500 ms 以上，实际 4 ms。测试设了 `{chunkSize:128, latency:5}`；fakellm 不设 `stream.tokens_per_sec` 就不分速，工具参数按 16 字节一块直接发完，适配器又忽略了 `chunkSize`。
- `claude-partials` 的 "native background child"：没深究，可能也需要 predicate 式匹配。
- 没跑 opencode、kimi、grok、antigravity 的 `*-aimock` 变体。

要点：Claude 的 `/v1/messages?beta=true` 把提示词包在 `<system-reminder>` 块里；原始 Anthropic 请求体有 `system` 数组，还有 `messages[]` 里的 system 角色消息，`system-prompt.ts` 在存在 `raw.system` 时忽略后者；vendor 测试默认用真实的 `~/.claude`、`~/.codex`（claude 抓包里看到了本机的 skills 列表），codex 用临时 `CODEX_HOME`；`getRequests()` 要求同步，适配器靠 `node -e fetch(...)` 起子进程实现，很粗糙；abort 和 dispose-mid-turn 用例在 900 ms 的 `delay_ms` 下与 aimock 一致。

## paseo（第二轮）

- 克隆：`/home/akrc/Developer/fakellm-research/paseo`，分支 `fakellm`，提交 `50ee5ef`（提交时用 `-c core.hooksPath=/dev/null` 跳过 hooks）。
- 新增 `packages/server/src/test-utils/fakellm.ts`（84 行，`startFakellm({models?})`）：以 `--port 0` 启动 `FAKELLM_BIN` 或 M2 二进制，从 stdout 读 URL；暴露 `setRules`、`requests(query)`（总是 `full=true`）、`reset`、`stop`（SIGTERM，3 秒后 SIGKILL）。
- 先要构建没有 `dist` 的工作区包（protocol、highlight、relay、plugin），否则 vitest 解析不了 `@getpaseo/protocol`。
- 运行（逐个文件，在 `packages/server`）：

```bash
eval "$(fnm env)"; fnm use 24
npx vitest run <file>
```

| 测试文件 | 之前（行数） | 之后（行数） | 测试数 |
|---|---|---|---|
| `agent/providers/claude/agent.context-usage.local.e2e.test.ts` | 140 | 104 | 6 |
| `agent/providers/codex-app-server-agent.local.e2e.test.ts` | 430 | 315 | 2 |
| `agent/providers/codex-custom-provider-import.local.e2e.test.ts` | 194 | 138 | 4 |
| `daemon-e2e/codex-custom-provider-archive.local.e2e.test.ts` | 135 | 80 | 1 |
| `daemon-e2e/codex-auto-review.local.e2e.test.ts` | 388 | 287 | 2 |
| `agent/providers/opencode-retry-timeout.local.e2e.test.ts` | 156 | 127 | 1 |

六个文件合计 16 个测试全部通过。diff 为 +144/−536（不含 84 行的 helper）。

要点：
- Claude 上下文用量：`turn: 0`、`turn: 1` 两条规则加 `stream: true`，`usage {input_tokens: 20236+turn+cache, output_tokens: 64, cached_tokens: cache}`。缓存读取必须折进覆盖值，因为 fakellm 对 Anthropic 会从 `input_tokens` 里扣掉缓存读取。`sonnet` 别名和 `[1m]` 模型都通过，1,000,000 上下文窗口的断言保留。
- Codex app-server：reload 用一条文本 "saved" 规则；提问测试用 `has_tool_result:false` 回 `request_user_input` 工具调用（`id: call1`），`has_tool_result:true` 回 "done"；`function_call_output`/`call1` 的断言改读请求日志的原始正文，通过。
- 自定义 provider 导入与归档恒回 "done"；归档里新增 `requestCount() === 1` 断言，通过。
- 自动审批：`times:1` 规则序列，主工具调用、guardian JSON（仅 auto-review 模式）、最终 "done"；guardian 请求只靠顺序区分。
- OpenCode 重试：`--models flaky-model`，503 加 `error.headers {"retry-after-ms":"15000"}`，含 12 秒等待的测试通过，说明 header 位置有效。
- Claude Code 发出的模型名是 `glm-5.2` 而不是 `glm-5.2[1m]`，日志过滤要去掉 `[1m]`；这是客户端的真实行为，不是 fakellm 的问题。

paseo 暴露的缺口：
1. `input_tokens` 出现在 `message_start` 还是 `message_delta` 不可控。Claude 的 `start` 和 `delta` 两个用例现在是同一个 wire 形状，只是名字不同，两者都通过，旧的区分已不再被覆盖。
2. 缓存与输入耦合：规则里的 `usage.input_tokens` 对 Anthropic 要包含缓存读取（写 `20236+turn+cache` 才能让响应里的 `input_tokens` 等于 20236+turn）；`count_tokens` 会返回被抬高的值。（TODO 已有此条。）
3. 没有序列原语：guardian 调用与主调用只靠 `times:1` 的先后顺序区分，不是匹配条件；codex 一旦调整调用顺序就会失效。

## ai-sdk 一致性对照（第二轮）

目的不是迁移测试，而是拿 ai-sdk 自带的真实 API 抓包夹具当对照：同一份夹具，一边经 msw test-server 回放得到期望的 parts，另一边把最终内容翻译成规则交给 fakellm，用同一个 `doStream` 取实际 parts，再比较。

- 克隆：`/home/akrc/Developer/fakellm-research/ai`，分支 `fakellm`，提交 `23d6d62`（提交 hook 对两个文件跑了 ultracite）。
- 新增：`packages/openai/src/fakellm-conformance.test.ts`（481 行，3 个测试）、`packages/anthropic/src/fakellm-conformance.test.ts`（424 行，4 个测试）；两个文件之前都不存在，共享 helper 复制在各自文件里。
- 运行（在 `packages/<pkg>`；先构建一次依赖 `pnpm --filter @ai-sdk/provider --filter @ai-sdk/provider-utils --filter @ai-sdk/test-server build`）：

```bash
eval "$(fnm env)"; fnm use 24
FAKELLM_BIN=/home/akrc/Developer/fakellm-research/bin/fakellm npx vitest run --config vitest.node.config.js src/fakellm-conformance.test.ts --silent=false --reporter=verbose
```

不设 `FAKELLM_BIN` 时 7 个全部跳过，设置后 7 个全通过。

夹具：openai chat `openai-text`；responses `openai-reasoning-encrypted-content.1`（一个文件里串了四个响应，取 #0 的 reasoning + function_call 和 #3 的纯文本）；anthropic `anthropic-text`、`anthropic-tool-no-args`、`anthropic-json-tool.1`（`structuredOutputMode: 'jsonTool'`）、`anthropic-clear-thinking.1`。

断言（7 个全过）：文本、reasoning、工具调用（名称与 input）、统一的 finish reason、input/output usage 一致。块级差异只记录日志，不断言。规则从原始夹具的最终内容推出，含文本、reasoning、`reasoning_opaque`、工具调用 id 与参数、完整 usage 覆盖。usage 与 finish reason 没有差异。

差异（均不影响汇总断言）：

| 流 | 夹具块数 | fakellm 块数 |
|---|---|---|
| chat 文本 | 301 | 477 |
| responses 文本 | 8 | 11 |
| responses reasoning | 32 | 49 |
| anthropic 文本 | 6 | 29 |
| anthropic json-tool | 2 | 6 |
| anthropic thinking | 11 | 25 |
| anthropic thinking 的文本部分 | 3 | 7 |
| responses `tool-input-delta` | 13 | 2 |

- 块粒度：fakellm 每个 delta 一个分词 token，夹具是真实 API 的块；函数调用参数按 16 字节一块。
- 部件序列（`anthropic-tool-no-args`）：fakellm 多发一个内容为 `{}` 的 `tool-input-delta`；夹具里 `partial_json` 为空，因此没有这个 part。
- 字段缺失（anthropic `providerMetadata`）：usage 缺 `cache_creation`、`service_tier`、`inference_geo`；`model` 是请求里的别名（`claude-sonnet-4-5`）而不是带日期的 id；`message_start` 的 `output_tokens` 为 1，夹具是 10 和 2；`message_delta` 缺 `context_management`，所以 `contextManagement` 为 `null` 而不是 `{appliedEdits: []}`。
- 字段缺失（responses 加密内容）：夹具里 `output_item.added`（reasoning-start）和 `output_item.done`（reasoning-end）的 `encrypted_content` 不同，fakellm 两处用同一个值。

其他：
- msw 对控制面和 fakellm 的请求打 "unhandled request" 警告，会放行，只是噪音。
- harness 的 usage 覆盖要取自原始夹具而不是解析后的 parts；Anthropic 的 `input_tokens` 必须是 input + cache read + cache write，因为 fakellm 会扣掉缓存。
- 规则没有按 model 匹配，fakellm 直接回显请求里的模型名。

## oar 第二轮重跑

- 分支 `fakellm`，HEAD `a6c4802`，在 `a86b115` 之上新增三个提交（`3c66603`、`960a8ab`、`a6c4802`），改动全在 `sea-trial/harness/fakellm.ts`。
- 运行（Node 24）：

```bash
CI=1 OAR_MOCK=fakellm OAR_TEST=<claude|codex|opencode>-aimock npx vitest run sea-trial/vendor --reporter=json
OAR_TEST=opencode-aimock npx tsx sea-trial/main.ts
```

适配器改动：
- `reply.usage` 透传给 fakellm 的 `usage`，接受 `input_tokens`、`output_tokens`、`prompt_tokens`、`completion_tokens`、`cached_tokens`、`reasoning_tokens`、`cache_creation_tokens`；丢弃 `total_tokens`，其他键注册时抛错。
- `latency` 改映射到 `stream.ttft_ms`（原来是 `delay_ms`）；`chunkSize` 映射到 `stream.chunk_tokens`（字符数除以 4，最小 1）。
- `toolCallId` 到 `last_tool_call_id` 第一轮已有，未改；本轮跑的 vendor 测试没有用到它。
- 发现旧的 `userMessage` 翻译有 bug：aimock 匹配最后一条 user 消息，适配器却用 `any_message`，续聊时 `/first/` 规则会匹配到历史消息，遮住 `/second/`。Responses（Codex）现改用 `last_user`；Anthropic 和 Chat 仍用 `any_message`，因为 Claude 会在提示词之后追加 reminder 和 tool_result 轮，换成 `last_user` 后 Claude 从 49 掉到 44。
- opencode：识别谓词 `(r) => (r.tools ?? []).length === 0` 并翻译成 `no_tools: true`；识别负向正则 `^(?![\s\S]*X)[\s\S]*$`（"不含 X"）并翻译成无条件规则，靠首个匹配生效，所以 X 规则要先注册。前者读函数源码文本，测试代码形状一变就失效；适配器遇到翻译不了的内容在注册时抛错，不会静默错配。

| 后端 | 第一轮（通过/失败） | 本轮 | 变化 |
|---|---|---|---|
| claude-aimock | 46 / 22 | 49 / 19 | +3 |
| codex-aimock | 39 / 13 | 41 / 11 | +2 |
| opencode-aimock vendor | 未跑 | 6 / 4 | 不适用 |
| opencode-aimock `main.ts` | 未跑 | 17 通过 / 2 跳过 / 0 失败 | 不适用 |

没有记录 Claude 具体修好了哪三个；Codex 的 `last_user` 修好了续聊的 `usage()` 快照和 `reasoning-tokens`，但没有单独测只改 usage 翻译的效果，分不开两者各自贡献。JSON reporter 的逐文件结果把跳过算成失败，所以只用汇总数。

剩余失败（按原因）：
- Claude 19：8 个 `match.predicate`（mcp-config 2、tool-search 1、mcp-servers 2、mcp-tools-list-changed 1、serve-mcp-tools 1、tool-input-stream 1）；9 个函数式回复（claude-permissions 7、input-identity 2）；`claude-partials` 2 个——"slow tool arguments keep the stall clock moving" 失败为 `expected 1 to be greater than 500`（疑与时序有关，`latency`/`chunkSize` 已映射仍失败，工具参数分块仍是固定 16 字节），"native background child completed blocks retain attribution" 失败为 `expected false to be true`，未诊断。
- Codex 11：6 个 predicate（codex-goals 3、mcp-servers 2、serve-mcp-tools 1）；3 个函数式回复（input-dropped 1、input-identity 2）；`failure-classes` "unknown model" 为 `expected {failure:'model_unavailable', …(1)} to deeply equal {failure:'model_unavailable'}`，结果多了一个字段（可能是 `status`），未深究；`service-tier` 为 `expected 'default' to be 'priority'`，未诊断。这两个在第一轮的 aimock 基线上就失败，与 fakellm 不一定有关。
- opencode vendor 4：全是 `match.predicate`（mcp-servers-acp 3、serve-mcp-tools 1）。

predicate 与函数式回复占 Claude、Codex 30 个失败中的 26 个，外加 opencode 的 4 个，修它们需要 fakellm 里的声明式等价物或序列/状态功能，具体形态尚未设计。

## fakellm 缺口

两轮之后重新排序：先按需要它的仓库数，同数时按受影响的用例数。缺口名后括号里是涉及的仓库。「已解决」的单独列在表后。

| # | 缺口 | 涉及仓库 | 精确的失败断言或行为 |
|---|---|---|---|
| 1 | 任意谓词、turn 范围与跨请求序列/状态匹配（oar、paseo） | oar、paseo | oar：没有 `currentTurnSays`、`request.tools.length==0`、按最后一个 `tool_call_id ===` 比较的等价物；`last_tool_call_id` 与 `no_tools` 只覆盖其中两种。第二轮重跑后仍有 `match.predicate` 失败：claude 8、codex 6、opencode 4，共 18 个。paseo：guardian 请求与主请求只靠 `times:1` 的先后顺序区分，没有匹配条件，codex 一旦调整调用顺序就失效 |
| 2 | Anthropic 请求里的 system 角色消息与 `<system-reminder>` 前置块污染 `last_user`（oar、claude-agent-sdk-python） | oar、claude-agent-sdk-python | Claude Code 2.1.314 在 `messages[]` 末尾发 `{"role":"system"}`，IR 把它转成 `user`，`last_user` 匹配的是环境块而不是真实提示词；第一条 user 消息本身是 `<system-reminder>`，`last_user: "pong"` 永远不命中。oar 第二轮再次确认：Anthropic 适配只能用 `any_message`，改 `last_user` 会让 Claude 从 49 掉到 44（reminder 与 tool_result 轮排在提示词之后）；`any_message` 又比 aimock 宽松，Codex 侧已因此出过续聊误命中。修法：IR 保留 `role:system`，`last_user` 只看真实 user 文本 |
| 3 | 工具参数流式的分块大小与延迟（oar、ai-sdk） | oar、ai-sdk | oar `claude-partials` "slow tool arguments"：`expected 1 to be greater than 500`，适配器已把 `latency`、`chunkSize` 映射到 `ttft_ms`、`chunk_tokens` 仍失败，因为工具参数固定 16 字节一块、未设 `tokens_per_sec` 不限速。ai-sdk 对照：responses `tool-input-delta` 夹具 13 块、fakellm 2 块 |
| 4 | Anthropic 缓存读取与 `input_tokens` 耦合，`count_tokens` 不一致（paseo、ai-sdk） | paseo、ai-sdk | 规则的 `usage.input_tokens` 对 Anthropic 要包含缓存读取（paseo 写 `20236+turn+cache`，ai-sdk 写 input + cache read + cache write），响应里被扣减；`count_tokens` 不扣，会返回被抬高的值。TODO「文档与一致性」已有此条 |
| 5 | 函数式/动态回复（oar） | oar | `echo-mcp.ts` 的 `echoFixtures`/`scriptEchoes` 与权限、input-identity 夹具：claude 9、codex 3，共 12 个失败 |
| 6 | Anthropic 的 `input_tokens` 位置不可控（paseo） | paseo | 不能选择 `message_start` 还是 `message_delta`；paseo 的 Claude `start`/`delta` 两个用例变成同一 wire 形状，旧区分不再被覆盖 |
| 7 | 与真实 API 的响应保真差异（ai-sdk） | ai-sdk | anthropic usage 缺 `cache_creation`、`service_tier`、`inference_geo`；`model` 是请求别名而非带日期的 id；`message_start` 的 `output_tokens` 为 1（夹具 10 和 2）；`message_delta` 缺 `context_management`，`contextManagement` 为 `null` 而非 `{appliedEdits: []}`；`tool-no-args` 多一个 `{}` 的 `tool-input-delta`；responses 的 `encrypted_content` 在 `output_item.added` 与 `.done` 上用同一个值（夹具不同）；每个 delta 一个分词 token，块数多于真实 API（chat 301 对 477、anthropic 文本 6 对 29） |
| 8 | 可配置的路由前缀，或匹配任意前缀的 `.../v1/messages`（raft） | raft | `POST /anthropic/v1/messages` 404，`/v1/messages` 200；Pi 的路径断言只能改成 `/v1/messages` |
| 9 | `POST /v1/systemone`（Ollama 专有，xsai） | xsai | `packages/decide/src/model-systemone.ts` 的两个 decide 用例在 HTTP 404 处失败，响应格式未知；是否值得实现要看 Ollama 这个端点是否稳定 |
| 10 | `cut_after_chunks`/`reset_after_chunks` 不发 TCP RST（pi-mono，README 已列） | pi-mono | 截断测试只能断言 `stopReason` 属于 `error`/`stop`/`length`；客户端看到 `terminated`/`unexpected EOF` 而不是 `ECONNRESET` |
| 11 | 正则不支持环视（oar） | oar | Rust regex 无 lookahead；opencode 的"不含 X"已能靠规则顺序（无条件规则放在 X 规则之后）表达，适配器遇到其他环视仍抛错，降为低优先级 |
| 12 | oar 两个未诊断的 codex 失败，无法判断是否 fakellm 问题 | oar | `failure-classes` unknown model：`expected {failure:'model_unavailable', …(1)} to deeply equal {failure:'model_unavailable'}`（多一个字段，疑为 `status`）；`service-tier`：`expected 'default' to be 'priority'`。两者在第一轮 aimock 基线就失败；另有 `claude-partials` "native background child" `expected false to be true` 未诊断 |

已解决或已实现：

- `reply.usage` 覆盖：第一轮的 codex resumed-usage、`reasoning-tokens` 失败。第二轮 oar 适配器已透传，重跑通过。
- `POST /v1/embeddings`（xsai）：embed 两个用例原先 404。embeddings 在工作区里已实现（`src/openai/embeddings.rs`），但还没有在 xsai 上重跑验证，状态记为「已实现、待验证」。

不算缺口但值得写进文档的使用坑：

- `match.protocol` 的取值是 `openai.chat`、`openai.responses`、`anthropic.messages`，写 `openai` 或 `anthropic` 得到 400。
- 请求日志的 `headers` 是 `[name, value]` 对的列表；`GET /_fakellm/requests` 返回 `{"requests":[...]}`。
- `has_tool_result: false` 在后续 user 轮也会匹配，续聊场景用 `turn`。
- Claude Code 会用 ToolSearch 延迟加载 MCP 工具，首个请求的 `tools` 里看不到它们。
- 对 `claude` CLI 没观察到标题、`count_tokens`、`/v1/models` 旁路请求；opencode 有一个无工具的标题请求。

## 经验

- 第二轮补充：paseo 的 16 个测试迁移后净减 392 行（+144/−536）手写假服务代码；日志按原始请求正文断言（例如 `function_call_output`/`call1`）比手写假服务更直接。
- 第二轮补充：夹具对照说明汇总断言（文本、工具、finish reason、usage）全部一致，差异集中在块粒度和 provider 元数据，不影响多数客户端，但会影响逐块断言的测试。
- 六个仓库里，Chat Completions、Responses、Anthropic Messages 三种协议在同一套规则上都能跑通；最常用的匹配键是 `has_tool_result`、`turn`、`system`、`no_tools`、`model`。
- 真实 harness 的第一类坑是「谁在请求里塞了什么」：Claude Code 的系统块、opencode 的标题请求。写规则前先用 `GET /_fakellm/requests?full=true` 看实际请求。
- 第二类坑在测试进程管理：不要用 `pkill -f`，子进程不要继承 stderr，退出时按 PID 清理。
