# 真实消费方实验：用 fakellm 替换别人仓库测试里的 LLM 后端

本文记录一轮实验：在 `/home/akrc/Developer/fakellm-research` 下克隆了六个会调用 LLM API 的开源仓库，各开一个本地分支 `fakellm`，把它们的测试或 e2e 改成对着 fakellm 跑，用来找 fakellm 的缺口。所有提交都只在本地，没有推送。

统一约定：

- 二进制用 `FAKELLM_BIN`，默认 `/home/akrc/Developer/fakellm-research/bin/fakellm`，以 `--port 0` 启动，从第一行 stdout 读 URL。
- OpenAI 风格的客户端用 `url + /v1`；Anthropic SDK 用裸 `url`；真正的 `claude` CLI 用 `ANTHROPIC_BASE_URL=url`。
- 规则按测试期望手写，用 `PUT /_fakellm/scripts` 注册，`POST /_fakellm/reset` 隔离用例。
- 实验时的二进制没有 `reply.usage` 覆盖（在另一个工作流里加），也没有 embeddings。

## 总览

| 仓库 | 分支 / 提交 | 结果 | 阻塞缺口 |
|---|---|---|---|
| xsai | `fakellm` / `e9febde` | 14 个 e2e：10 通过，4 失败 | `/v1/embeddings`、`/v1/systemone` |
| pi-mono | `fakellm` / `839c592` | 新增 10 个全通过 | 无 |
| claude-agent-sdk-python | `fakellm` / `6bd191f` | 15 个通过，hooks 3 个跳过（未写规则） | 无 |
| opencode | `fakellm` / `c200d6b` | 全通过 | 无 |
| raft | `fakellm` / `678808f` | 157 个通过 | 路径前缀（只影响断言） |
| oar | `fakellm` / `a86b115` | sea-trial 全通过；vendor 全量有失败 | 多项，见缺口表 |

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

## fakellm 缺口

按需要它的仓库数从多到少排序。缺口名后括号里是涉及的仓库。

| # | 缺口 | 涉及仓库 | 精确的失败断言或行为 |
|---|---|---|---|
| 1 | Anthropic 请求里的 system 角色消息与 `<system-reminder>` 前置块污染 `last_user`（2 个） | oar、claude-agent-sdk-python | Claude Code 2.1.314 在 `messages[]` 末尾发 `{"role":"system"}`（"# Environment ... Today's date"），IR 把它转成 `user`，于是 `last_user` 匹配的是这段文字而不是真正的提示词；oar 第一次工具轮拿到兜底回复、没有工具调用。另外第一条 user 消息本身是 `<system-reminder>` 和环境块，claude-agent-sdk-python 里 `last_user: "pong"` 永远不命中，只好改成只按 model 匹配。oar 临时把 aimock 的 `userMessage` 映射成 `any_message`，比 aimock 宽松（连 system 提示和前面的轮次都会看到）。修法：IR 里保留 `role:system`，`last_user` 只看真实的 user 文本（可忽略 `<system-reminder>` 块） |
| 2 | `reply.usage` 覆盖（1 个，已在另一工作流中开发） | oar | `reply.usage` 返回 400 "unknown field"；codex resumed-usage 与 `reasoning-tokens` 失败。近似分词器足够那些不钉死数字的断言 |
| 3 | `POST /v1/embeddings`（1 个） | xsai | embed 两个用例 404 "Invalid URL"。期望：`embedding.length > 0`、数值有限、`usage.inputTokens > 0` 且 `totalTokens >= inputTokens`；不同输入向量不同；同一批里长度一致 |
| 4 | `POST /v1/systemone`（Ollama 专有，1 个） | xsai | `packages/decide/src/model-systemone.ts` 的 `systemone()`；两个 decide 用例在 HTTP 404 处就失败，响应格式未知。后续断言包括 damaged 的 `probability > 0.5`、`choice === 'shipping'`、`probabilities` 的键为 `['0','1','2']`、usage tokens 大于 0。是否值得实现要看 Ollama 这个端点的稳定性 |
| 5 | 可配置的路由前缀（1 个） | raft | `POST /anthropic/v1/messages` 404，`/v1/messages` 200。Pi 的网关 baseUrl 只能改为 fakellm 的 origin，路径断言从 `/anthropic/v1/messages` 改成 `/v1/messages`。需要路由前缀配置，或匹配任意前缀的 `.../v1/messages` |
| 6 | 无任意谓词或 turn 范围匹配（1 个） | oar | 没有 `currentTurnSays`、`request.tools.length==0`、按最后一个 `tool_call_id ===` 比较的等价物。`last_tool_call_id` 覆盖了 `toolCallId` 的情形，`no_tools` 可顶替标题请求的判断。14 个 `match.predicate` 失败来自这里 |
| 7 | 回复里没有函数式/动态回复（1 个） | oar | `echo-mcp.ts` 的 `echoFixtures`/`scriptEchoes`（根据请求内容算出回复）有 6 个失败 |
| 8 | 工具参数流式不分速（1 个） | oar | `claude-partials` "slow tool arguments"：期望 500 ms 以上，实际 4 ms。工具参数固定 16 字节一块，且未设 `tokens_per_sec` 时不限速；测试配置是 `{chunkSize:128, latency:5}`。需要一个对工具参数也生效的分块大小和延迟 |
| 9 | 正则不支持环视（1 个） | oar | Rust regex 无 lookahead，aimock 的 `baseFixtures`「非 slow」规则只能靠规则顺序表达，适配器遇到环视直接抛错 |
| 10 | `cut_after_chunks` 不做 TCP 重置（1 个，README 已列） | pi-mono | 截断测试只能断言 `stopReason` 属于 `error`/`stop`/`length`；pi-ai 对连接中途断开的处理没被覆盖 |

不算缺口但值得写进文档的使用坑：

- `match.protocol` 的取值是 `openai.chat`、`openai.responses`、`anthropic.messages`，写 `openai` 或 `anthropic` 得到 400。
- 请求日志的 `headers` 是 `[name, value]` 对的列表；`GET /_fakellm/requests` 返回 `{"requests":[...]}`。
- `has_tool_result: false` 在后续 user 轮也会匹配，续聊场景用 `turn`。
- Claude Code 会用 ToolSearch 延迟加载 MCP 工具，首个请求的 `tools` 里看不到它们。
- 对 `claude` CLI 没观察到标题、`count_tokens`、`/v1/models` 旁路请求；opencode 有一个无工具的标题请求。

## 经验

- 六个仓库里，Chat Completions、Responses、Anthropic Messages 三种协议在同一套规则上都能跑通；最常用的匹配键是 `has_tool_result`、`turn`、`system`、`no_tools`、`model`。
- 真实 harness 的第一类坑是「谁在请求里塞了什么」：Claude Code 的系统块、opencode 的标题请求。写规则前先用 `GET /_fakellm/requests?full=true` 看实际请求。
- 第二类坑在测试进程管理：不要用 `pkill -f`，子进程不要继承 stderr，退出时按 PID 清理。
