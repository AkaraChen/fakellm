# 真实消费方实验：用 fakellm 替换别人仓库测试里的 LLM 后端

> 本文是作者本机的一次性实验记录：`$RESEARCH_DIR`（即 fakellm-research 目录）不随仓库发布，其中的路径和本地分支不可直接复现。下面写的命令是当时的做法，换了机器要自己克隆这些仓库、重新建分支。

在 `$RESEARCH_DIR` 下克隆了八个会调用 LLM API 的开源仓库（后面又加了 claude-agent-sdk-typescript），各开一个本地分支 `fakellm`，把它们的测试或 e2e 改成对着 fakellm 跑，用来找 fakellm 的缺口。实验分三轮，每轮用当时最新的二进制：

- 第一轮（M1 二进制）：xsai、pi-mono、claude-agent-sdk-python、opencode、raft、oar 六个仓库。
- 第二轮（M2 二进制）：新增 paseo 和 ai-sdk 一致性对照，重跑 oar。
- 第三轮（M3 二进制：trajectory、JS 策略脚本、路径前缀、`args_chunk_bytes`、embeddings）：重跑 oar、xsai、raft、pi-mono。
- 补充（M4 之后）：自定义端点规则让 xsai 的最后两个用例通过（`83f149e`，用当前工作区的二进制重跑确认）。
- 收尾一轮（consumers）：oar 改用策略 decline 并重跑三个后端，codex 用 Rust SDK 重写一个 CLI 场景，paseo 改用 `match.after`。
- 替换原有测试（`reply.raw` 之后）：不再手写规则，而是让原有的 wiremock / msw 夹具直接换成 fakellm 的原样回放；见[替换原有测试](#替换原有测试)。

所有提交都只在本地，没有推送。

统一约定：

- 二进制用 `FAKELLM_BIN`，默认 `$RESEARCH_DIR/bin/fakellm`，以 `--port 0` 启动，从第一行 stdout 读 URL。
- OpenAI 风格的客户端用 `url + /v1`；Anthropic SDK 用裸 `url`；真正的 `claude` CLI 用 `ANTHROPIC_BASE_URL=url`。
- 规则按测试期望手写，用 `PUT /_fakellm/scripts` 注册，`POST /_fakellm/reset` 隔离用例。
- 第一轮的二进制没有 `reply.usage` 覆盖，也没有 embeddings；第二轮用 M2 构建，`reply.usage` 已可用（oar 重跑已验证），embeddings 在第三轮由 M3 二进制实现，xsai 重跑已通过。

## 总览

每格是该轮跑到的结果（通过 / 失败），`—` 表示这一轮没有跑这个仓库。

| 仓库 | 第一轮 | 第二轮 | 第三轮 | 补充 | 现状 |
|---|---|---|---|---|---|
| xsai | 10 / 4（共 14 个 e2e） | — | 12 / 2 | 14 / 0 | 全通过 |
| pi-mono | ai 9 + agent 1，全通过 | — | ai 10 + agent 4，全通过 | — | 全通过 |
| claude-agent-sdk-python | 15 通过，hooks 3 个跳过（没写规则） | — | — | — | 全通过 |
| opencode | 全通过（session-runner 1 个，真实 CLI e2e 2 个） | — | — | — | 全通过 |
| raft | 157 / 0 | — | 158 / 0（新增 1 个 reset 用例） | — | 全通过 |
| oar（vendor 全量） | claude 46 / 22，codex 39 / 13；sea-trial 套件全通过 | claude 49 / 19，codex 41 / 11，opencode 6 / 4 | claude 66 / 2（改映射后应为 67 / 1），codex 50 / 2，opencode 10 / 0 | claude 68 / 0，codex 50 / 2，opencode 10 / 0（`883154b`，decline 适配器） | 剩下的 2 个 codex 失败在 aimock 基线上同样失败，与 fakellm 无关 |
| paseo | — | 6 个文件、16 个测试全部迁移并通过 | — | codex-auto-review 的顺序改用 `match.after`，2 / 2（`40fdc00`） | 全通过 |
| codex（Rust SDK） | — | — | — | 新增 `fakellm-e2e`：cli_stream 的 `responses_mode_stream_cli` 用进程内 `fakellm::Server` 重写，1 / 1（`831fe79`） | 通过 |
| ai-sdk 一致性 | — | 7 个对照测试全通过 | — | — | 全通过，差异见缺口 3 |
| codex（核心测试套件） | — | — | — | `CODEX_TEST_FAKELLM=1`：`cli_stream` 9 / 9，85 个只用 SSE 挂载的模块 259 / 283（`e79b7d8`） | 部分通过，差异见缺口 11 |
| ai-sdk（原有测试） | — | — | — | `AI_SDK_TEST_SERVER=fakellm`：openai 994 / 995，anthropic 792 / 792，未改测试文件（`4465312`） | 1 个 `controlled-stream` 用例未支持 |
| claude-agent-sdk-typescript | — | — | — | 新增 `e2e-fakellm`：6 / 6（`1f77bed`） | 通过 |
| oar-codex（诊断） | — | — | — | codex 50 / 2 → 52 / 0，claude 68 / 0，opencode 10 / 0（`18f262b`，只改两个测试文件） | 全通过 |

阅读方法：第一轮的失败几乎全是 fakellm 缺的东西（`reply.usage`、embeddings、路径前缀、谓词与函数式回复、自定义端点）；到收尾一轮，没有已知的 fakellm 缺口会让这些仓库的测试失败，oar 剩下的 2 个 codex 失败已证明与 fakellm 无关（见[收尾一轮](#收尾一轮consumers)）。

## 本地分支与提交

分支名都是 `fakellm`，基于各仓库的默认分支（opencode 是 `dev`，其余是 `main`）；下表是分支上比默认分支多出来的提交，从旧到新。

| 仓库（`$RESEARCH_DIR/` 下） | 提交 |
|---|---|
| `xsai` | `e9febde` 跑 e2e 套件（`scripts/e2e-fakellm.sh` 加规则文件）；`83f149e` 用自定义端点规则回答 `/v1/systemone` |
| `pi-mono` | `839c592` pi-ai 适配器与 agent 循环的 e2e；`9cc6a5a` reset、trajectory、策略用例 |
| `claude-agent-sdk-python` | `6bd191f` 可选的 fakellm 后端（`FAKELLM_BIN`） |
| `opencode` | `c200d6b` session-runner 与真实 opencode CLI 对着 fakellm 跑 |
| `raft-source` | `678808f` provider wire 测试改用 fakellm；`0adb751` 恢复 `/anthropic` 前缀，新增 reset 代理用例 |
| `oar` | `a86b115` `OAR_MOCK=fakellm` 适配器；`3c66603` usage、latency、chunkSize 映射；`960a8ab` `last_user` / `any_message` 选择；`a6c4802` 无工具谓词与否定正则；`fee3096` 每个动态夹具一个策略加回调；`883154b` 改用策略 decline，Anthropic 用 `last_user`，文本按 aimock 的 20 字符分块；`18f262b` 修两个过期的 codex 版本期望（只改 `failure-classes` 与 `service-tier` 的 vendor 测试） |
| `paseo` | `50ee5ef` codex / claude / opencode 本地 e2e 改用 fakellm；`40fdc00` codex-auto-review 的 guardian / 主请求顺序改用 `match.after` |
| `codex` | `831fe79` 独立 crate `fakellm-e2e`（自带 `[workspace]`），用 Rust SDK 重写 `cli_stream.rs` 的一个场景；`e79b7d8` `core_test_support` 的 fakellm 后端（`CODEX_TEST_FAKELLM=1`） |
| `claude-agent-sdk-typescript` | `1f77bed` 新项目 `e2e-fakellm`（6 个用例） |
| `ai` | `23d6d62` openai 与 anthropic 的夹具一致性对照；`4465312` `createTestServer` 的 fakellm 垫片（`AI_SDK_TEST_SERVER=fakellm`） |

## 如何复现

前提：`export RESEARCH_DIR=<克隆目录>`，`export FAKELLM_BIN=<fakellm 仓库>/target/debug/fakellm`（`cargo build` 得到），Node 版本见各节。命令的完整形式、环境变量和注意事项在各仓库自己的一节里。

| 仓库 | 命令（在该仓库的克隆目录里） | 详见 |
|---|---|---|
| xsai | `scripts/e2e-fakellm.sh`（加 `--reporter=verbose` 看逐条） | [xsai](#xsai) |
| pi-mono | `cd packages/ai && npx vitest --run test/fakellm.e2e.test.ts`；`cd packages/agent && npx vitest --run test/fakellm.e2e.test.ts` | [pi-mono](#pi-mono) |
| claude-agent-sdk-python | `.venv/bin/pytest e2e-tests/test_*.py -m e2e -v`（必须设 `FAKELLM_BIN`，否则会打真实 API） | [claude-agent-sdk-python](#claude-agent-sdk-python) |
| opencode | `cd packages/core && bun run test test/session-runner-recorded.test.ts test/fakellm-cli.e2e.test.ts` | [opencode](#opencode) |
| raft | `cd packages/daemon && pnpm exec vitest run src/agentProcessManager.builtin.e2e.test.ts src/drivers/kimi-sdk.test.ts src/drivers/pi.test.ts` | [raft](#raft) |
| oar | `CI=1 OAR_MOCK=fakellm OAR_TEST=<claude\|codex\|opencode>-aimock npx vitest run sea-trial/vendor`（必须 `CI=1`） | [oar](#oar)、[第三轮](#第三轮m3-二进制) |
| paseo | `cd packages/server && npx vitest run <文件>`（先构建没有 `dist` 的工作区包） | [paseo（第二轮）](#paseo第二轮) |
| ai-sdk | `cd packages/<openai\|anthropic> && npx vitest run --config vitest.node.config.js src/fakellm-conformance.test.ts` | [ai-sdk 一致性对照（第二轮）](#ai-sdk-一致性对照第二轮) |
| codex 核心套件 | `cd codex-rs && CODEX_TEST_FAKELLM=1 RUST_MIN_STACK=8388608 cargo test -p codex-core --test all cli_stream` | [替换原有测试](#替换原有测试) |
| ai-sdk 原有测试 | 先在 `packages/test-server` 里 `npx tsdown`；`cd packages/<openai\|anthropic> && FAKELLM_BIN=<二进制> AI_SDK_TEST_SERVER=fakellm npx vitest run --config vitest.node.config.js` | [替换原有测试](#替换原有测试) |
| claude-agent-sdk-typescript | `cd e2e-fakellm && npm install && node --test --test-concurrency=1 test/*.test.mjs` | [替换原有测试](#替换原有测试) |

## 结论

fakellm 解决了的：

- **手写假服务不用再写**。paseo 迁移六个文件净减 392 行（+144/−536）；raft 删掉三个文件里各自手写的假 provider；opencode、claude-agent-sdk-python 的真实 CLI 也能直接连上。三种协议在同一套规则上都跑通，最常用的匹配键是 `has_tool_result`、`turn`、`system`、`no_tools`、`model`。
- **请求日志取代手写计数**：按规则、协议、原始正文断言（例如 `function_call_output`），比各家手写的内存数组直接。
- **三轮里补上的功能都有对应的通过用例**：`reply.usage` 覆盖、embeddings（xsai）、路径前缀（raft）、`stream.args_chunk_bytes`（oar 的慢参数用例）、真正的 RST（pi-mono、raft）、trajectory 与策略（pi-mono 的 agent 循环）、谓词与函数式回复经策略实现（oar 从 49 / 41 提到 66 / 50）、自定义端点（xsai 14 / 14）。
- **一致性对照**：汇总断言（文本、工具、finish reason、usage）与真实 API 抓包夹具一致。
- **原样回放能直接替换原有的 HTTP mock**：ai-sdk 的 1786 / 1787 个原有测试不改测试文件就通过（见[替换原有测试](#替换原有测试)）。

还没解决的：

- 无。oar 的 claude-partials 失败、适配器改用 `decline`、paseo 的序列匹配都在收尾一轮解决；codex 的 unknown model 与 service-tier 两个失败在 aimock 基线上同样失败，属于 oar 测试自身的问题。
- 响应保真度：块粒度、Anthropic 的 usage 元数据、`model` 别名、`encrypted_content`（缺口 3）；这些不影响多数客户端，但会影响逐块断言。
- Anthropic 缓存读取与 `input_tokens` 耦合、`input_tokens` 出现在 `message_start` 还是 `message_delta` 不可控（缺口 2、4）。
- Rust regex 不支持环视（缺口 8，三个后端的全量 vendor 套件都不需要它）。
- 覆盖：trajectory 与策略只在 openai-completions 上被真实消费方测过；CONNECT 代理转发 RST 的行为没验证（缺口 9、10）。
- 所有实验都对着本地二进制，没有一个消费方在 CI 里持续跑；分支都只在本地。

## xsai

- 克隆：`$RESEARCH_DIR/xsai`，分支 `fakellm`，提交 `e9febde`（跑套件），补充提交 `83f149e`（自定义端点规则，见第三轮一节末尾）。
- 运行：

```bash
eval "$(fnm env)"; fnm use 26
cd $RESEARCH_DIR/xsai && scripts/e2e-fakellm.sh
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

- 克隆：`$RESEARCH_DIR/pi-mono`，分支 `fakellm`，提交 `839c592`。
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

- 克隆：`$RESEARCH_DIR/claude-agent-sdk-python`，分支 `fakellm`，提交 `6bd191f`。
- 新增 `e2e-tests/conftest_fakellm.py`（fixture 和按测试名索引的 `RULES`），`conftest.py` 只在设置 `FAKELLM_BIN` 时导入它。session fixture 起 fakellm，并在 `os.environ` 里设置 CLI 所需的 base URL、API key、临时 `CLAUDE_CONFIG_DIR`，以及遥测、更新、版本检查开关；每个用例前 reset 并 PUT 该用例的规则加一个兜底规则；没有 `RULES` 条目的测试被跳过。`FAKELLM_LOG` 可在会话结束时导出完整请求日志。
- 运行：

```bash
cd $RESEARCH_DIR/claude-agent-sdk-python
FAKELLM_BIN=$RESEARCH_DIR/bin/fakellm \
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

- 克隆：`$RESEARCH_DIR/opencode`，分支 `fakellm`，提交 `c200d6b`（`bun.lock` 原本就有改动，未提交）。
- 新增 `packages/core/test/lib/fakellm.ts`（helper）、`packages/core/test/fakellm-cli.e2e.test.ts`（2 个：纯文本、bash 工具往返）；`session-runner-recorded.test.ts` 在设置 `FAKELLM_BIN` 时改用 `FetchHttpClient.layer` 和 fakellm 的 `url + /v1`，原断言全保留，另加一条日志里只命中 `main` 规则的断言。
- 运行（在 `packages/core`）：

```bash
FAKELLM_BIN=$RESEARCH_DIR/bin/fakellm bun run test test/session-runner-recorded.test.ts
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

- 克隆：`$RESEARCH_DIR/raft-source`，分支 `fakellm`，提交 `678808f`。
- 新增 `packages/daemon/src/testing/fakellm.ts`（`startFakeLlm()`，返回 `{url, openAiBaseUrl, setRules, requests(query), reset, close}`，`requests()` 带 `full=true`，每条有 `.raw`、`.path`、`.rule`、`.protocol`、`.headers`）；改写了三个测试文件里手写的假服务（`startFakeAnthropicServer` 等被删除）。
- 运行：

```bash
cd $RESEARCH_DIR/raft-source/packages/daemon
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

- 克隆：`$RESEARCH_DIR/oar`，分支 `fakellm`，提交 `a86b115`。
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

- 克隆：`$RESEARCH_DIR/paseo`，分支 `fakellm`，提交 `50ee5ef`（提交时用 `-c core.hooksPath=/dev/null` 跳过 hooks）。
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

- 克隆：`$RESEARCH_DIR/ai`，分支 `fakellm`，提交 `23d6d62`（提交 hook 对两个文件跑了 ultracite）。
- 新增：`packages/openai/src/fakellm-conformance.test.ts`（481 行，3 个测试）、`packages/anthropic/src/fakellm-conformance.test.ts`（424 行，4 个测试）；两个文件之前都不存在，共享 helper 复制在各自文件里。
- 运行（在 `packages/<pkg>`；先构建一次依赖 `pnpm --filter @ai-sdk/provider --filter @ai-sdk/provider-utils --filter @ai-sdk/test-server build`）：

```bash
eval "$(fnm env)"; fnm use 24
FAKELLM_BIN=$RESEARCH_DIR/bin/fakellm npx vitest run --config vitest.node.config.js src/fakellm-conformance.test.ts --silent=false --reporter=verbose
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

## 第三轮（M3 二进制）

M3 带来 trajectory、JS 策略脚本（`PUT /_fakellm/policies`）、路径前缀容忍、忽略 `<system-reminder>` 的 `last_user`（原文用 `last_user_raw`）、`stream.args_chunk_bytes`、embeddings。二进制 `$RESEARCH_DIR/bin/fakellm`，Node 24（xsai 用 26.8.1）。所有提交仅在本地。

### oar（`fee3096`，只改 `sea-trial/harness/fakellm.ts`）

- 做法偏离原计划：不能把 `Function.prototype.toString` 的源码塞进策略，因为谓词和回复函数闭包了可变的测试状态（`active`、`slow`、`providerCalls`、`calls`）和 `currentTurnSays`。改为每个动态夹具（非 no_tools 谓词，或函数式回复）注册成策略 `fx<N>`，策略源码是一个很小的 node 模块，把 IR 请求 POST 到测试进程里的 HTTP 回调服务（`127.0.0.1:0`）；回调构造 aimock 形状的视图，原样运行谓词和回复函数，用已有的 `translateReply` 转成 ReplySpec 返回。
- fakellm 的策略没有“放行/未命中后落到下一条规则”的语义，所以回调自己按顺序用 JS 移植的静态匹配器遍历后面的夹具，都不匹配就返回 404 spec。启动参数 `--policy-timeout-ms 60000`，策略运行时用 `process.execPath`。`reset()` 同时清夹具和策略。
- 慢参数映射：工具调用回复的 `chunkSize` 映射为 `stream.args_chunk_bytes`，`latency` 映射为 `tokens_per_sec = 1000/latency`（fakellm 每个 token 发一个参数块；实测 10 token/s 下 19 块用 1.9 秒）。claude-partials 的 slow-arguments 现通过。
- 命令（Node 24，三个后端并行各跑一次）：`CI=1 OAR_MOCK=fakellm OAR_TEST=<claude-aimock|codex-aimock|opencode-aimock> npx vitest run sea-trial/vendor --reporter=json --outputFile=$TMPDIR/oarp/res/<backend>.json`

| 后端 | 第二轮 | 第三轮 |
|---|---|---|
| claude-aimock | 49 / 19 | 66 / 2（改映射后只重跑了 claude-partials、tool-input-stream，全量应为 67 / 1） |
| codex-aimock | 41 / 11 | 50 / 2 |
| opencode-aimock vendor | 6 / 4 | 10 / 0 |

剩余失败（共 3 个，算上 claude 修好的那个前为 5 个）：
1. claude-partials "native background child completed blocks retain attribution"：已定位。`onMessage(/CHILD_PARTIAL/)` 在 Anthropic 上映射为 `any_message`，父级后续请求里带着 Task tool_use 参数（含 `CHILD_PARTIAL`），命中子规则而永远返回不了 `PARENT_OUTPUT`。aimock 只看最后一条 user 消息。M3 的 `last_user`（忽略 reminder）可能能修，但还没有换上重试；适配器此前因 Claude 会追加 reminder 和 tool_result 轮才用 `any_message`。
2. codex failure-classes "unknown model"：分类结果多一个字段，疑为错误体形状差异，未深究。
3. codex service-tier "resume preserves the saved tier"：继承的会话报 `default` 而不是 `priority`，不涉及谓词或函数式回复，未查是否与 mock 有关。

注意事项：
- `journal.getAll` 用阻塞的 `execFileSync`，若在策略请求进行中被调用会让回调死锁；未观察到。
- 每个动态夹具多一个 node 子进程，注册时约 100ms。
- 回落路径忽略“不含 X”的否定正则，与静态路径一致。
- 文本回复的 `latency` 仍映射为 `ttft_ms`，aimock 的 `latency` 是每块延迟，文本时序只是近似。
- 计时类失败没有单独归类；除已修好的慢参数外，剩余失败都不是计时问题。

### xsai（`e9febde`，未改脚本，运行遗留未跟踪的 `.vitest/`）

- 命令：`scripts/e2e-fakellm.sh`（加 `--reporter=verbose` 看逐条）；脚本在 11434 启动二进制并按 PID 退出时清理。规则来自 `scripts/e2e-fakellm.rules.yaml`。
- 总计 12 / 14（第一轮 10 / 14）：model 3/3、embed 2/2、text-chat 2/2、text-messages 2/2、text-responses 2/2、decide 1/3。embed 两个用例（单输入、批量）现在通过，embeddings 验证完成。
- decide 唯一失败是两个调用 `systemone` 的用例，均为 `HTTP 404 Invalid URL (POST /v1/systemone)`；Chat Completions 的 decide 通过。
- `/v1/systemone` 的格式现已查清（`packages/decide/src/model-systemone.ts`）：
  - 请求：`model`；`state`（字符串或 JSON）；可选 `images`；`questions` 为 id 到 `{criteria, instructions, type}` 的映射，`type` 为 `choice`、`score`、`noul`（SDK 对布尔问题发 `noul`）。choice 的 `criteria` 是 `{value: 描述|null}`，score 的是等级字符串数组，noul 无。
  - 响应：`answers`（`{type:'choice', choice, confidence, probabilities}`、`{type:'score', score, confidence, probabilities:{'0','1','2'}}`、`{type:'noul', noul:p}`）和 `usage:{input_tokens, output_tokens}`。
  - 测试约束：`damaged.probability` 对 angry 输入大于 0.5、happy 输入小于 0.5（需要按 `state` 区分两个请求）；`route.choice` 为 `shipping` 且概率在 `['billing','shipping']` 上和为 1；`anger.score` 大于 1 且不大于 2；`usage.input_tokens` 大于 0。
- 值得做，成本低：纯 JSON POST。但路由器对非标准路径一律 404，策略脚本也到不了；需要“自定义端点规则”（匹配任意 `POST <path>`，返回静态或模板化 JSON，可带策略）。按 `damaged` 匹配需要 body 级匹配器（`last_user` 对没有 messages 的请求无效）。
- 脚本用 `sleep 1` 而不是轮询就绪；fakellm 日志在 `$TMPDIR/xsai-fakellm.log`。
- 补充（`83f149e`，只改 `scripts/e2e-fakellm.rules.yaml`，加 16 行）：用 `match: { path: /v1/systemone, body: ... }` 加 `reply.body` 写了 `systemone-angry` 和 `systemone-happy` 两条规则（按 `state` 里的 `shattered screen` / `quick delivery` 区分），两个 decide 用例通过，合计 14 / 14，6 个测试文件全过。用当前工作区构建的二进制重跑确认过。规则写法见 [docs/scripts.md](scripts.md#自定义端点)。

### raft（`0adb751`，在 `678808f` 之上，只改 `packages/daemon/src/drivers/pi.test.ts`）

- 三个原本丢掉前缀的 Anthropic 用例（契约捕获 helper、工具往返、resume）恢复使用 `${fake.url}/anthropic`，路径断言改为 `startsWith("/anthropic/v1/messages")`（请求日志记录完整前缀路径），“fakellm 没有 /anthropic 前缀”的注释删除。路径前缀缺口已解决。
- 新增一个用例：Pi provider 经 CONNECT 代理访问真实 fakellm 子进程，规则 `reply.stream.reset_after_chunks: 1`（`last_user: "stream failure proof"`），断言 pi-ai 报 `stopReason: "error"`、`errorMessage: "terminated"`，并断言 CONNECT 目标 `direct-unreachable.invalid:80`。
- 命令（`packages/daemon`，Node 24）：`npx vitest run src/agentProcessManager.builtin.e2e.test.ts src/drivers/kimi-sdk.test.ts src/drivers/pi.test.ts`。157 通过，加 reset 用例后 158 通过；没有跑完整 daemon 套件。
- 发现：裸 `net` 客户端对 M3 二进制得到 `ECONNRESET`，没有干净的 `end`，即二进制发真正的 RST（与 `docs/m3-spec.md` 的 linger 0 一致），`docs/scripts.md`、README、本文旧版写的“FIN 不是 RST”曾经过时，现已更正。经 Node CONNECT 代理时 undici 仍报 `terminated`，没抓到代理是转发 RST 还是转成 FIN；该用例在 pi-ai 层区分不了 FIN 与 RST。
- 第一次插入新用例落进了上一个用例体里，过滤运行时被跳过，提交前已修正括号位置。

### pi-mono（`9cc6a5a`，Node 24.20.0）

- 命令：`cd packages/ai && npx vitest --run test/fakellm.e2e.test.ts`；`cd packages/agent && npx vitest --run test/fakellm.e2e.test.ts`。ai 9 到 10，agent 1 到 4，全部通过。
- reset 用例（ai）：`reset_after_chunks: 2`、`chunk_tokens: 1`、`maxRetries: 0`；pi-ai 报 `stopReason: "error"`、`errorMessage: "terminated"`，内容为部分文本 `"one two"`，fakellm 只记录 1 个请求（无重试）。比旧的截断用例（接受 error/stop/length）严格。
- trajectory 用例（agent）：三步，`tool_map` 把 shell 映射到 `run_shell`（`cmd: "$command"`），第 2 步 `expect contains: "Cargo"`，第 3 步 `contains: "version = 7"`；agent 真的执行了 `ls` 和 `cat Cargo.toml`；`GET /_fakellm/trajectories` 为 `hits: 3, last_step: 2`。
- 分歧用例（agent，额外增加）：`expect` 写错时，最后一条 assistant 消息 `stopReason: "error"`，`errorMessage` 含 "diverged"；严格分歧返回 400，pi-ai 不重试，agent 循环正常结束不抛异常。
- 策略用例（agent）：node 运行时的 async 策略，按 `request.messages.at(-1)` 与 `state.stage` 选下一个工具；最终文本含工具输出（"Policy saw: version = 7"）；策略列表 `hits: 3, state.stage: "done"`。
- 新增通用 helper `put(path, body, method?)`（`packages/ai/test/helpers/fakellm.ts`），非 2xx 即抛错。
- `package-lock.json` 在开始前就已被修改（删 26 行），没有提交。vitest 会吞 `console.log`，用 `--disableConsoleIntercept` 查看。

## 收尾一轮（consumers）

用 `cargo build --release` 得到的当前工作区二进制（含 `match.after`），Node 24（fnm）。所有提交只在本地。

### oar（`883154b`，只改 `sea-trial/harness/fakellm.ts`）

- 动态夹具改用策略 `decline`：每个夹具一个策略 `fx<N>`，fakellm 规则先做静态匹配，回调里只跑该夹具自己的谓词和回复函数，谓词为假就返回 `{decline: true}`，由 fakellm 落到后面的规则。删掉了 JS 版静态匹配器 `staticMatches` 和回调里自己遍历夹具、返回 404 的代码。
- Anthropic 的 `userMessage` 映射为 `last_user`，但只换成 `last_user` 不够：Claude Code 在真实提示词之后还追加自己的 user 轮（`# Environment ...` 约 21000 字符，子智能体请求则追加 `The following deferred tools are now available ...`），`last_user` 取到的是这条追加轮，导致 claude 掉到 61 / 7。做法是三条规则：`last_user` 精确匹配；其后一条 `any_message` 规则挂到策略 `fx<N>t` 上，由回调按“自上一条 assistant / tool 消息之后的第一条非空 user 文本（去掉 reminder）”重新判断，不是则 `decline`；最后是 Chat Completions 的 `any_message`。父级后续请求里 Task 参数带 `CHILD_PARTIAL` 不再命中子规则，"native background child" 通过。
- 文本回复默认 `chunk_tokens = 5`（aimock 默认每块 20 字符；fakellm 默认每个 token 一块，`PARENT_OUTPUT` 会被切成 `PARENT`、`_`、`OUTPUT`，而该用例要求一个完整的 `PARENT_OUTPUT` delta）。
- 结果（`CI=1`，三个后端的全量 vendor）：

| 后端 | 之前 | 现在 |
|---|---|---|
| claude-aimock | 66 / 2 | 68 / 0 |
| codex-aimock | 50 / 2 | 50 / 2 |
| opencode-aimock | 10 / 0 | 10 / 0 |

  三个后端并行跑时 claude 偶发 SessionStart hook、凭据刷新这类超时用例失败（一次 64 / 4，换了一批用例），单独跑稳定 68 / 0；判断是机器负载，不是 fakellm。
- codex 剩下的 2 个失败在 aimock 基线（不设 `OAR_MOCK`）上完全一样，与 fakellm 无关：
  - `failure-classes` "unknown model"：fakellm 与 aimock 两边的差异都是 `+ "status": 404`。测试期望 `{failure: 'model_unavailable'}`，oar 的分类结果多了 `status: 404`（Anthropic 那一格期望里写了 `status: 404`，OpenAI 这一格漏写）。错误体翻译没有问题，是测试期望过期。
  - `service-tier` "resume preserves the saved tier"：`expected 'default' to be 'priority'`，aimock 下同样出现，是 oar 对 codex 0.162.1 恢复会话时读取 service tier 的行为，不涉及请求内容。

### codex（`831fe79`，`$RESEARCH_DIR/codex/fakellm-e2e`）

- 独立 crate（自带 `[workspace]`，不编译 codex-rs 工作区），dev 依赖 `fakellm = { path = ... }` 与 tokio。`tests/cli_stream.rs` 把 `core/tests/suite/cli_stream.rs::responses_mode_stream_cli` 的 wiremock 场景搬到 Rust SDK：`Server::start(Config { scripts, .. })` 进程内启动，临时 `CODEX_HOME/config.toml` 指向 `server.base_url()`（`wire_api = "responses"`，重试 0），运行已安装的 `codex exec`，断言退出成功、stdout 恰好一行 `FAKELLM_OK`、`server.requests()` 里恰好一条 `/v1/responses`（协议、模型 `gpt-5.5`、流式、状态 200），规则 `hits == 1`。
- `cargo test`：1 通过，首次冷编译约一分钟，之后 1 秒内。

### paseo（`40fdc00`）

- `daemon-e2e/codex-auto-review.local.e2e.test.ts`：`guardian-1` 加 `match: {after: "main-1"}`，`main-2` 加 `match: {after: "guardian-1"}`，顺序成为匹配条件，不再靠规则顺序与 `times: 1`。该文件 2 / 2 通过。

### sdk/js 作为官方 helper

以后新的消费方不需要再抄 `startFakellm` 这类手写 harness（paseo 的 `test-utils/fakellm.ts`、oar 的适配器都是这样的一次性代码）：`sdk/js` 就是官方 helper，vitest 项目直接用它的 [globalSetup](../sdk/js/README.md#通用-helper-vitest-globalsetup)，一次启动、读取地址、结束时清理。

## 替换原有测试

`reply.raw`、`match.path` 指向标准路由、`match.nth` / `match.method` / `match.header` 实现之后，不再需要把期望改写成 fakellm 的剧本：原有测试里的 wiremock / msw 夹具（状态码、`content-type`、录制的 SSE 正文）可以逐字节原样回放。本节是这一轮的结果；所有提交只在本地，没有推送。

### codex（`e79b7d8`，`codex-rs/core/tests/common/fakellm_backend.rs`）

- 做法：`core_test_support` 新增依赖 `fakellm = { path = ..., default-features = false }` 和后端模块。`CODEX_TEST_FAKELLM=1` 时 `responses::start_mock_server()` 与新的 `responses::TestServer::start()` 启动进程内的 `fakellm::Server`（严格模式）；`mount_sse_once` / `mount_sse_sequence` 注册自定义端点规则（`match.path` 为 `*/responses`、方法 POST、`reply.raw` 的 `text/event-stream` 加原始 SSE 文本、每个正文 `times: 1`），`start_mock_server` 另挂一条默认的 `GET */models`。`ResponseMock` 在这个模式下读 fakellm 的请求日志（完整正文和头），转换成 `wiremock::Request`，原有的正文不变式检查照常跑。
- API 变动：新增 `TestServer`（解引用到 wiremock 的 `MockServer`，有 `uri()` 和 `verify()`）和 `MockBackend` trait，`test_codex.rs` 的 builder 与两个挂载函数接受 `&impl MockBackend`；`code_mode_elicitation`、`hooks_executor`、`review`、`scenarios` 四个文件把类型从 `MockServer` 换成 `TestServer`；`cli_stream.rs` 的 `mount_sse_*` 用例改用 `TestServer::start()`，两个 personal-access-token 用例因为挂自定义 wiremock `Mock` 仍用 wiremock。
- 结果（codex-core 的 `all` 测试二进制，要设 `RUST_MIN_STACK=8388608`，否则整个二进制栈溢出）：

| 运行 | 不设变量 | `CODEX_TEST_FAKELLM=1` |
|---|---|---|
| `cli_stream` | 9 / 0 | 9 / 0 |
| 完整 `all` 二进制 | 2456 通过、420 失败、8 忽略 | 未跑完（见下） |
| 只用 `mount_sse_once` / `mount_sse_sequence` 的 85 个模块 | 282 通过、1 失败、3 忽略 | 259 通过、24 失败、3 忽略 |

  完整运行里的 420 个失败不设变量也失败，是环境问题。fakellm 模式没跑完整：挂 wiremock `Mock`、`*_match` 或 `mount_response_*` 的测试会挂起，因为这些挂载落在客户端根本不连的 wiremock 上；85 个模块是用 grep 避开这些 helper 选出来的，选择是近似的，仍有几个会挂的混在里面。没有让这些 wiremock 专用 helper 在 fakellm 模式下 panic。
- 24 个失败的原因：15 个是请求日志解不开 codex 在 ChatGPT 认证下发的 zstd 请求正文（记录里是有损的 `raw_text`，`body_json()` 报 "failed to decode zstd request body"，集中在 `search_tool`、`hooks_mcp`），属于 fakellm 缺口 11；5 个是 `unified_exec_process_events` 的 "deadline has elapsed"，4 个其他，这 9 个没有调查。
- 限制：wiremock 的 `Match` 闭包没有对应物；要支持 `mount_sse_once_match` 之类得翻译成 `body` / `header` / `nth` 条件。`fakellm-e2e/target/*` 已被 git 跟踪、工作区里显示为删除，提交只含 codex-rs 的测试路径和 `Cargo.lock`。
- 构建代价：codex-core 测试二进制约 40 分钟（主要是依赖和 rusty_v8 拉取），另有 `codex` 二进制 8 分钟；磁盘最低剩 17 GB。

### ai-sdk（`4465312`，`packages/test-server/src/fakellm-test-server.ts`）

- 做法：`create-test-server.ts` 只在 `AI_SDK_TEST_SERVER=fakellm` 时委托给垫片，默认的 msw 路径不变。`start()` 在空闲端口启动 `FAKELLM_BIN`，`stop()` 按 PID 结束。每个路由键翻成原样回放规则（`match.path` 是 URL 路径，`:param` 换成 `*`）；`json-value`、`stream-chunks`、`error`、`empty` 映射到 `reply.raw`；数组和函数每次调用一条 `nth` 规则（函数展开 12 次）；给 `server.urls[x].response` 赋值会通过 `curl` 同步推送规则。`server.calls` 从 `full=true` 的请求日志读（multipart 用 `raw_text` 加 `Response.formData()` 重建）。
- 请求重定向：包装 `globalThis.fetch`，把每个路由的源站改写到 fakellm，因此显式传 `baseURL` 的测试也覆盖到；`AI_SDK_FAKELLM_ENV=1` 才额外设 `OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL`（会改变 provider 测试看到的 URL）。tests 引用的是 `test-server/dist`，改垫片后要在 `packages/test-server` 里 `npx tsdown`。
- 结果：

| 包 | 范围 | msw | fakellm |
|---|---|---|---|
| openai | `src/chat`、`src/responses` | 995 / 995 | 994 / 995 |
| anthropic | `src` 全部 | 792 / 792（含 4 个没有 `FAKELLM_BIN` 就跳过的对照占位） | 792 / 792 |

  只有 `openai-responses-language-model.test.ts` 与 msw 不同（389 / 390）；失败的是用 `controlled-stream` 的一个用例，垫片不支持（`binary` 也不支持，但这些文件没用到）。
- 第一次全量运行 19 个失败，垫片修复后剩 1 个：响应头和快照（fakellm 加了 `x-request-id`、`request-id`、`openai-processing-ms`、`date`、`transfer-encoding`，垫片从响应里剔除；请求侧也隐藏 `host`、`content-length`、`accept: */*` 等传输头）、`content-type`（头里的值变成 `raw.content_type`，`error` 用 `text/plain`）、multipart 正文重建。
- fakellm 一侧没有阻塞项：`raw` 回放处理了所有夹具；hyper 一定会加 `date` 和 `transfer-encoding`，不能关；非 JSON 正文以有损 UTF-8 文本保存，对这里的文本 multipart 够用。
- 依赖 msw 内部行为的地方都由垫片处理：`requestCredentials` 写死 `same-origin`、头集合相等的断言依赖 msw 的 `Request` / `Response` 不带传输头、中途赋值 `.response` 的测试依赖全局的 `callNumber`（垫片的 `nth` 按 URL 计，靠已见的调用数补偿；目前没有用例因此失败）。
- 没有跑 openai chat 与 responses 目录、anthropic 包之外的测试；默认 msw 路径只重跑了 `anthropic-language-model.test.ts`（404 通过）。

### claude-agent-sdk-typescript（`1f77bed`，新项目 `e2e-fakellm/`）

- 这个仓库没有 e2e，新建了一个私有的 ESM 项目（依赖 `@anthropic-ai/claude-agent-sdk` 和 `zod`）。`test/helpers.mjs` 从 `sdk/js/dist` 引入 `start`，每个测试文件启动一个 fakellm（`target/debug/fakellm`，`FAKELLM_BIN` 可换），`pathToClaudeCodeExecutable` 指向已安装的 `claude`（2.1.314，`CLAUDE_CLI` 可换），`CLAUDE_CONFIG_DIR` 用临时目录，设 `CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK=1`。每个用例前 reset，再装该用例的规则加一条兜底。
- 6 个用例，`node --test --test-concurrency=1 test/*.test.mjs`（Node 24）6 / 6：纯文本结果（日志里有自定义 system prompt 和非空 tools）；结构化输出（`StructuredOutput` 工具调用规则，`structured_output` 等于脚本的 JSON）；SDK MCP 工具往返（ToolSearch、工具调用、最终文本按 turn 匹配，`echo` 处理函数收到 `{text:'hi'}`，工具结果出现在下一条请求里）；`includePartialMessages` 打开时有 thinking 块、`thinking_delta` 和 `text_delta`；默认不打开时没有 stream 事件；模型名上的 404 错误规则（`is_error` 为真，日志里是该规则的 404）。
- 发现：日志里的 system prompt 是 `request.messages` 里 `role: system` 的消息，不是顶层 `system` 字段；日志里的 `rule` 是规则名字符串；`path` 带查询串（`/v1/messages?beta=true`），要用 `startsWith`；CLI 会把 404 改写成 "There's an issue with the selected model…"，所以用例只断言 `is_error` 和日志里的 404，SDK 遇到 CLI 的错误结果会抛异常，helper 里接住。

### oar-codex 诊断（`18f262b`）

- 之前剩下的两个 codex 失败都是测试期望随 codex 版本过期，既不是适配器问题也不是 oar 的 bug，在不设 `OAR_MOCK` 的 aimock 基线上同样失败。
- `failure-classes` "unknown model"：测试期望 `{failure: 'model_unavailable'}` 不带 status，对应 codex 0.165.0（provider 404 报成命名的 `codexErrorInfo: "modelNotFound"`）；已装的 0.162.1 报 `{"httpConnectionFailed":{"httpStatusCode":404}}`，oar 的分类（`packages/oar/src/runtimes/codex/failure.ts`）有意保留 status。修法：Cell 类型加可选 `alsoStatus`，这一格设 404，期望不变。
- `service-tier` "resume preserves the saved tier"：codex 0.164.0 起省略 tier 的恢复沿用线程保存的 tier，之前取当前配置。新增 `codexKeepsSavedTier()` 读 `codex --version`，断言按版本分支。
- 修复只改 `sea-trial/vendor/failure-classes.vendor.test.ts` 和 `service-tier.vendor.test.ts`；不设 `OAR_MOCK` 时这两个文件 14 / 0。`OAR_MOCK=fakellm`（`CI=1`，Node 24，逐个跑）：claude 68 / 0，codex 52 / 0（原 50 / 2），opencode 10 / 0。
- 旧二进制：默认的 `$RESEARCH_DIR/bin/fakellm` 已过期，不认适配器现在发的 `{decline:true}`（"unknown field `decline`"），用它跑 codex 会多 3 个无关失败；要么刷新它，要么设 `FAKELLM_BIN=<仓库>/target/debug/fakellm`（上面的数字都是后者）。

## fakellm 缺口

三轮和一次补充之后重新排序：先按需要它的仓库数，同数时按受影响的用例数；第三轮已解决的缺口移到表后。

| # | 缺口 | 涉及仓库 | 精确的失败断言或行为 |
|---|---|---|---|
| 1 | ~~策略放行语义未被 oar 使用；paseo 序列匹配缺失~~ 已解决 | oar、paseo | oar 适配器改用 `{decline: true}`（`883154b`），paseo 的 guardian / 主请求顺序改用 `match.after`（`40fdc00`），两边的用例都通过 |
| 2 | Anthropic 缓存读取与 `input_tokens` 耦合，`count_tokens` 不一致（paseo、ai-sdk） | paseo、ai-sdk | 规则的 `usage.input_tokens` 对 Anthropic 要包含缓存读取（paseo 写 `20236+turn+cache`，ai-sdk 写 input + cache read + cache write），响应里被扣减；`count_tokens` 不扣，会返回被抬高的值。TODO「文档与一致性」已有此条 |
| 3 | 与真实 API 的响应保真差异（ai-sdk） | ai-sdk | anthropic usage 缺 `cache_creation`、`service_tier`、`inference_geo`；`model` 是请求别名而非带日期的 id；`message_start` 的 `output_tokens` 为 1（夹具 10 和 2）；`message_delta` 缺 `context_management`，`contextManagement` 为 `null` 而非 `{appliedEdits: []}`；`tool-no-args` 多一个 `{}` 的 `tool-input-delta`；responses 的 `encrypted_content` 在 `output_item.added` 与 `.done` 上相同；每个 delta 一个分词 token，块数多于真实 API；responses `tool-input-delta` 夹具 13 块、fakellm 2 块（`args_chunk_bytes` 现可调，但未对照重测） |
| 4 | Anthropic 的 `input_tokens` 位置不可控（paseo） | paseo | 不能选择 `message_start` 还是 `message_delta`；paseo 的 Claude `start`/`delta` 两个用例变成同一 wire 形状 |
| 5 | ~~自定义端点规则：`POST /v1/systemone`（xsai）~~ 已解决 | xsai | 已实现 `match.path` / `match.body` 加 `reply.body` / `status` / `headers`（[docs/scripts.md](scripts.md#自定义端点)）；xsai 的 `83f149e` 让两个 decide 用例通过，14 / 14 |
| 6 | ~~Anthropic 适配只能用 `any_message`，误命中历史消息~~ 已解决 | oar | `last_user` 加“当前轮提示词”策略回退，claude 68 / 0（见收尾一轮；Claude Code 在提示词后追加的 user 轮使纯 `last_user` 不够用） |
| 7 | ~~oar 两个未诊断的 codex 失败~~ 与 fakellm 无关 | oar | 在 aimock 基线上同样失败；unknown model 的差异是多出的 `status: 404`（测试期望过期），service-tier 与请求内容无关 |
| 8 | 正则不支持环视（oar） | oar | Rust regex 无 lookahead；opencode 的“不含 X”已能靠规则顺序表达，适配器遇到其他环视仍抛错；回落路径也忽略否定正则。低优先级 |
| 9 | 文档过时：RST 行为（raft）。文档已更正，仅剩代理验证 | raft | 二进制已发 RST（裸 `net` 客户端得到 `ECONNRESET`），README、`docs/scripts.md` 已改成 RST；未验证 CONNECT 代理会转发 RST 还是转成 FIN。经代理和 pi-ai 看到的都是 `terminated`，区分不了 |
| 10 | trajectory/策略的覆盖缺口（pi-mono） | pi-mono | agent 测试只用 openai-completions，Responses 与 Anthropic 的 trajectory/策略没测；bun 与 tjs 运行时没试；`args_chunk_bytes`（仅 oar 间接覆盖）、`last_user_raw`、路径前缀容忍（仅 raft 覆盖）没有专门测试 |
| 11 | 请求日志不解码 `content-encoding: zstd` 的请求正文（codex） | codex | codex 在 ChatGPT 认证下发 zstd 压缩的请求正文；记录里只有有损的 `raw_text`，`body_json()` 报 "failed to decode zstd request body"，核心套件 85 个模块里 15 个失败（`search_tool`、`hooks_mcp`）。未实现 |
| 12 | 替换原有 mock 时的剩余差异（codex、ai-sdk） | codex、ai-sdk | wiremock 的 `Match` 闭包没有对应物（要翻成 `body` / `header` / `nth`）；hyper 一定会加 `date` 和 `transfer-encoding`，不能关；ai-sdk 垫片不支持 `controlled-stream` 和 `binary`（垫片侧，不是引擎缺口） |

替换原有测试这一轮没有新增已解决的缺口；原样回放（`reply.raw`、`match.nth` / `method` / `header`）本身就是为这一轮做的。

补充一轮已解决：

- 自定义端点规则（xsai）：见缺口 5，14 / 14。
- 策略的“放行”语义：`decline` 已实现，oar 适配器已改用（缺口 1）。

第三轮已解决：

- 工具参数流式分块与延迟（oar）：`stream.args_chunk_bytes` 加 `tokens_per_sec`，slow-arguments 用例通过（10 token/s 下 19 块用 1.9 秒）。
- 可配置路径前缀（raft）：`/anthropic/v1/messages` 现可用，raft 三个用例已恢复前缀并通过。
- `POST /v1/embeddings`（xsai）：embed 两个用例通过，已验证。
- `reset_after_chunks` 的 RST（pi-mono、raft）：二进制现发真正的 RST；文档已更正，剩余只是 CONNECT 代理转发 RST 的验证（缺口 9）。
- 任意谓词与函数式回复（oar）：可由策略脚本实现，适配器已改用 decline（缺口 1）。
- Anthropic 的 system 角色消息与 `<system-reminder>` 污染 `last_user`：M3 已让 `last_user` 忽略 reminder（`last_user_raw` 取原文），oar 已重试，见缺口 6 与收尾一轮。

第二轮已解决：

- `reply.usage` 覆盖：第一轮的 codex resumed-usage、`reasoning-tokens` 失败，oar 适配器透传后重跑通过。

不算缺口但值得写进文档的使用坑：

- `match.protocol` 的取值是 `openai.chat`、`openai.responses`、`anthropic.messages`，写 `openai` 或 `anthropic` 得到 400。
- 请求日志的 `headers` 是 `[name, value]` 对的列表；`GET /_fakellm/requests` 返回 `{"requests":[...]}`。
- `has_tool_result: false` 在后续 user 轮也会匹配，续聊场景用 `turn`。
- Claude Code 会用 ToolSearch 延迟加载 MCP 工具，首个请求的 `tools` 里看不到它们。
- 对 `claude` CLI 没观察到标题、`count_tokens`、`/v1/models` 旁路请求；opencode 有一个无工具的标题请求。

## 经验

- 第三轮补充：期望被改写成函数式的测试，不要靠嵌入函数源码——闭包状态会丢，用回调服务把真实函数留在测试进程里更稳；策略脚本现在有 `decline` 放行语义，适配器只需在回调里判断自己的谓词（缺口 1）。
- 第三轮补充：别信文档里的 wire 行为，用裸 `net` 客户端对二进制探一下（RST 就是这样发现已实现的）。
- 第二轮补充：paseo 的 16 个测试迁移后净减 392 行（+144/−536）手写假服务代码；日志按原始请求正文断言（例如 `function_call_output`/`call1`）比手写假服务更直接。
- 第二轮补充：夹具对照说明汇总断言（文本、工具、finish reason、usage）全部一致，差异集中在块粒度和 provider 元数据，不影响多数客户端，但会影响逐块断言的测试。
- 六个仓库里，Chat Completions、Responses、Anthropic Messages 三种协议在同一套规则上都能跑通；最常用的匹配键是 `has_tool_result`、`turn`、`system`、`no_tools`、`model`。
- 真实 harness 的第一类坑是「谁在请求里塞了什么」：Claude Code 的系统块、opencode 的标题请求。写规则前先用 `GET /_fakellm/requests?full=true` 看实际请求。
- 第二类坑在测试进程管理：不要用 `pkill -f`，子进程不要继承 stderr，退出时按 PID 清理。
