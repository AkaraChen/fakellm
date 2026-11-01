# fakellm

一个专门给测试用的 LLM API 服务：全内存、不跑模型，启动只要几毫秒。
只复刻协议（请求和响应的格式、SSE 分帧、错误体、状态码），回复来自你写的剧本，或者一个确定性的默认回复。
和 fakes3 之于 S3 是同一个思路。目前实现了三种协议：OpenAI Chat Completions、OpenAI Responses 和 Anthropic Messages（加模型列表、`count_tokens` 和 Embeddings），三者共用同一套剧本、请求日志和虚拟时钟，还能扮演 agent 循环里的模型（[fake agent](#fake-agent)），用 Rust 编写。

## 为什么

调研了九个真实的 agent 和 SDK 项目（见 [docs/research.md](docs/research.md)），没有一家用现成的假服务：
paseo 手写了约 5 个 `http.createServer` 假模型，raft 十几个测试文件各自手写假 provider，opencode 自己做了一个进程内的 Chat SSE 假后端。每家都在重复造这个轮子，而且都踩了同样几个坑：

- **启动慢**：用真实或本地推理服务做测试，启动和首包延迟以秒计。fakellm 是一个二进制或一个 Rust 库，启动几毫秒，也可以在进程内直接起。
- **时间不好控**：延迟用墙钟，abort / steer 的窗口就 flaky；retry 测试要真等 12 秒。fakellm 有服务端虚拟时钟，流式节奏、`created`、请求日志时间戳都跟着它走，冻结后一个 chunk 都不会发出，`advance` 一下立刻放行。
- **调用过程不好断言**：日志丢字段（想断言 `reasoning_effort` 却发现 mock 没记）。fakellm 记录每个请求的原始 JSON、解析后的结构、请求头和服务端的回复，通过控制面查询。
- **剧本无限重复匹配、旁路请求抢剧本**：agent 循环里同一条规则会一直命中（有人在 30 秒里被调用了 1143 次 tool），生成标题的旁路请求会吃掉本该给主流程的回复。剧本支持 `times`、`has_tool_result`、`last_tool_result`、`system`、`no_tools` 这类有状态条件。

## 协议与端点

| 协议 | 端点 | 说明 |
|---|---|---|
| OpenAI Chat Completions（`openai.chat`） | `POST /v1/chat/completions` | 同步和 SSE，工具调用，`n > 1`，`stream_options.include_usage` |
| OpenAI Responses（`openai.responses`） | `POST /v1/responses` | 同步和 SSE，工具调用，推理项，`previous_response_id`；codex 和 ai-sdk 的 `openai('...')` 默认走这个 |
| | `GET /v1/responses/{id}`、`DELETE /v1/responses/{id}` | 读取、删除保存的响应对象，见 [Responses 的状态](#responses-的状态与-previous_response_id) |
| Anthropic Messages（`anthropic.messages`） | `POST /v1/messages` | 同步和 SSE，`tool_use`、`thinking`（含 `signature`），`anthropic-beta` 头只记录不校验 |
| | `POST /v1/messages/count_tokens` | 返回 `{"input_tokens": n}`，不消耗剧本 |
| OpenAI Embeddings（`openai.embeddings`） | `POST /v1/embeddings` | 确定性向量，`float` / `base64`，见 [Embeddings](#embeddings)；剧本里只有带 `error` 的规则对它生效 |
| 模型列表 | `GET /v1/models`、`GET /v1/models/{id}` | 请求带 `anthropic-version` 头时返回 Anthropic 的格式（`display_name`、`created_at`、`has_more`），否则是 OpenAI 的 |

所有路径也可以不带 `/v1` 前缀，也可以带任意前缀：`--path-prefix /anthropic` 配置的前缀会先被剥掉，此外前面多出的若干段只要后面恰好是上表里的某个路径（如 `/anthropic/v1/messages`、`/gateway/v2/proxy/v1/chat/completions`）就会被路由，请求日志里保留完整路径。其他路径是 404 `Invalid URL (METHOD path)`，带 `anthropic-version` 头的请求得到 Anthropic 格式的 404。
认证：配了 `--api-key` 后，`Authorization: Bearer` 和 `x-api-key` 任一个对即可，所有路由通用；失败时 Chat / Responses 返回 OpenAI 的 `invalid_api_key`，Anthropic 路由返回 `authentication_error`（“invalid x-api-key”）。
三种协议的请求先被解析成同一个内部结构，所以[剧本](#剧本)里同一条规则可以回答它们，想区分时用 `protocol` 条件。
Anthropic 请求里 Claude Code 常常先发一条只有 `<system-reminder>` 块的 user 消息，所以剧本的 `last_user` 匹配的是去掉这些块之后仍有文本的最近一条 user 消息；要看原文用 `last_user_raw`（见 [docs/scripts.md](docs/scripts.md)）。

## 快速开始

```bash
cargo build --release
./target/release/fakellm --port 0      # 0 表示随机选一个空闲端口，实际地址会打印到 stdout
# http://127.0.0.1:41873
```

启动完成后 stdout 打印的第一行就是 endpoint，可以当作就绪信号。OpenAI 客户端的 base URL 要加 `/v1`；API key 随便填（除非用了 `--api-key`）。

openai Node SDK：

```js
import OpenAI from 'openai';

const client = new OpenAI({ baseURL: 'http://127.0.0.1:41873/v1', apiKey: 'test' });
const r = await client.chat.completions.create({
  model: 'gpt-4o',
  messages: [{ role: 'user', content: 'ping' }],
});
console.log(r.choices[0].message.content);
```

Vercel AI SDK。`openai('gpt-4o')` 默认走 Responses API，`openai.chat(...)` 走 Chat Completions，两者都支持：

```js
import { generateText } from 'ai';
import { createOpenAI } from '@ai-sdk/openai';

const openai = createOpenAI({ baseURL: 'http://127.0.0.1:41873/v1', apiKey: 'test' });
const { text } = await generateText({ model: openai.chat('gpt-4o'), prompt: 'ping' });
console.log(text);
```

Responses API 和 Anthropic SDK。注意 Anthropic SDK 的 `baseURL` **不带** `/v1`（它自己会加），ai-sdk 的 `createAnthropic` 则要带 `/v1`：

```js
import OpenAI from 'openai';
import Anthropic from '@anthropic-ai/sdk';

const openai = new OpenAI({ baseURL: 'http://127.0.0.1:41873/v1', apiKey: 'test' });
const r = await openai.responses.create({ model: 'gpt-5.5', input: 'ping' });
console.log(r.output_text);

const anthropic = new Anthropic({ baseURL: 'http://127.0.0.1:41873', apiKey: 'test' });
const m = await anthropic.messages.create({
  model: 'claude-opus-5-5',
  max_tokens: 64,
  messages: [{ role: 'user', content: 'ping' }],
});
console.log(m.content[0].text);
```

```js
import { generateText } from 'ai';
import { createAnthropic } from '@ai-sdk/anthropic';

const anthropic = createAnthropic({ baseURL: 'http://127.0.0.1:41873/v1', apiKey: 'test' });
const { text } = await generateText({ model: anthropic('claude-opus-5-5'), prompt: 'ping' });
```

Anthropic 的请求必须带 `max_tokens`（和真实 API 一样，缺了是 400 “max_tokens: Field required”）。

没有剧本时，每个请求得到一段确定性的 lorem 文本（同一个请求永远得到同一个回复）。注册一条规则：

```bash
curl -X PUT http://127.0.0.1:41873/_fakellm/scripts \
  -d '[{"match": {"last_user": "ping"}, "reply": {"text": "pong"}}]'
# {"added":1,"count":1}
```

上面的例子这时都会打印 `pong`。`sdk/node/` 里有用官方 SDK 跑的完整测试：`openai`、`@anthropic-ai/sdk`、Vercel AI SDK 的 `@ai-sdk/openai`（Chat 和 Responses）和 `@ai-sdk/anthropic`，共六个套件（`cargo test` 会在装了 node 时自动运行它们，先在该目录 `npm ci`；没装 `node_modules` 会打印 skip 并跳过）。

### 接线真实的 agent：`fakellm wire`

`fakellm wire <目标>` 打印让真实客户端连上 fakellm 的配置（环境变量、配置文件、代码片段），目标有 `claude`、`codex`、`opencode`、`pi`、`openai-sdk`、`ai-sdk`、`anthropic-sdk`：

```bash
eval "$(fakellm wire claude --endpoint http://127.0.0.1:41873)"    # 导出 ANTHROPIC_BASE_URL 和 ANTHROPIC_API_KEY
fakellm wire codex --endpoint http://127.0.0.1:41873 --format toml > "$CODEX_HOME/config.toml"
fakellm wire opencode --endpoint http://127.0.0.1:41873 --format json     # files["opencode.json"] 是配置文件
```

`--format` 取 `shell`（默认，有环境变量时）、`env`、`json`、`toml`，`--endpoint` 缺省读 `FAKELLM_ENDPOINT`。用真实二进制验证的结果（`e2e/*.sh`，手工运行，不在 `cargo test` 里；配方和注意事项见 [docs/wire.md](docs/wire.md)）：

| 客户端 | 结果 | 要点 |
|---|---|---|
| codex（0.162.1） | 通过：纯文本，和一轮 `exec_command` 工具调用 | 不读 `OPENAI_BASE_URL`，必须写自定义 provider（`wire codex` 生成）；`env_key` 可选（输出里注释掉了，启用后对应的环境变量必须非空）；故障只能用流内 `response.failed` |
| Claude Code（2.1.314） | 通过：`claude -p` 得到 `FAKELLM_OK` | 需要 `ANTHROPIC_API_KEY`；它在 `messages` 里放 `role: system` 的条目，fakellm 为此放宽了校验 |
| opencode（1.18.29） | 通过：标题旁路请求加主请求 | 要写一条 `no_tools` + `system` 的规则接住标题生成；隔离测试设 `OPENCODE_DISABLE_MODELS_FETCH=1`；第一次运行曾卡死一次，没能复现 |
| pi | **没有用真实的 pi 验证** | 只生成配置，格式按约定写，没有 e2e 脚本 |

这些 e2e 只验证“能完成一轮对话”，没有用真实 harness 测过 429 重试、断流、挂起这些故障。

## 剧本

剧本是一个有序的规则列表。每个请求从上到下找**第一条**条件全部满足、且 `times` 没用完的规则，用它的 `reply` 回答。没有规则命中时走[默认兜底](#默认兜底与---strict)。
通过 `PUT /_fakellm/scripts`、`fakellm ctl scripts put FILE`、`--scripts FILE` 或 Rust 的 `Config::scripts` 注册，格式是 JSON 或 YAML：一个规则数组，或者 `{"rules": [...]}`。

| 字段 | 含义 |
|---|---|
| `name` | 可选。规则名，出现在请求日志里，可以用 `rule=` 过滤 |
| `match` | 条件，缺省或 `{}` 匹配所有请求。所有写出的字段都要满足 |
| `reply` | 回复：`text` / `tool_calls` / `error` / `hang` 四选一（`text` 和 `tool_calls` 可以同时有） |
| `times` | 这条规则最多回答几次，用完后跳过，缺省不限 |

`match` 常用字段（完整列表和语义见 [docs/scripts.md](docs/scripts.md)）：

| 字段 | 含义 |
|---|---|
| `model` | 模型名 glob，`gpt-*`、`*-mini` |
| `last_user` | 正则，匹配最近一条 user 消息 |
| `system` | 正则，匹配 system / developer 消息 |
| `has_tool` / `no_tools` | 请求里提供了某个工具 / 没有提供任何工具 |
| `has_tool_result` | 对话以 tool 结果结尾（`true`）或不是（`false`） |
| `last_tool_result` | 结尾的 tool 结果里有某个函数的结果 |
| `last_tool_call_id` | 结尾的 tool 结果里有一条回答的是这个 id 的工具调用（`tool_call_id` / `call_id` / `tool_use_id`） |
| `stream` / `turn` | 只匹配流式 / 非流式；已经有几条 assistant 消息 |
| `protocol` | 只匹配某个协议：`openai.chat`、`openai.responses`、`anthropic.messages` |
| `probability` | 0 到 1，其他条件都满足后按这个概率命中（确定性的，同样的请求序列每次得到同样的结果） |
| `every_nth` | 其他条件都满足的请求里，每第 N 个命中（`3` 命中第 3、6、9 个） |

正则可以写成 `"pattern"`、`"/pattern/"` 或 `"/pattern/i"`（忽略大小写）。

`reply` 常用字段（完整说明见 [docs/scripts.md](docs/scripts.md#reply)）：

| 字段 | 含义 |
|---|---|
| `text` / `tool_calls` | 回复文本 / 工具调用（`arguments` 可以故意写成不完整的 JSON） |
| `usage` | 覆盖 tokenizer 的 token 计数：`prompt_tokens` / `input_tokens`、`completion_tokens` / `output_tokens`、`cached_tokens`、`reasoning_tokens`、`cache_creation_tokens`，只覆盖写了的字段；`count_tokens` 也会用它（见 [docs/scripts.md](docs/scripts.md#usage)） |
| `reasoning` / `reasoning_opaque` | 推理文本，和随它发的 `signature`（Anthropic）/ `encrypted_content`（Responses） |
| `error` | 错误：`status` 必填，`type` / `code` / `message` / `param` / `retry_after` 可覆盖；`in_stream: true` 把错误放进事件流；`headers` 加额外响应头（`retry-after-ms`、`x-should-retry`） |
| `hang` / `hang_ms` | 收到请求后不回答（连响应头都不发）/ 挂这么久（服务端时钟）再回答 |
| `reset` | 同步响应发出响应头和一半 body 后中断连接；流式请求对应 `stream.reset_after_chunks`（见[故障注入](#故障注入)） |
| `stream` | 节奏：`ttft_ms`、`tokens_per_sec`、`chunk_tokens`、`cut_after_chunks`（干净结束流）、`reset_after_chunks`（流中途连接中断，无结尾 chunk） |
| `delay_ms` | 同步响应或错误发出前的等待 |
| `finish_reason` | 覆盖结束原因（`stop`、`length`、`tool_calls`、`content_filter`） |

YAML：

```yaml
- name: greet
  match: { model: "gpt-*", last_user: "^hello" }
  reply: { text: "Hi there." }
- reply: { text: "兜底回复" }
```

JSON：

```json
[
  {"name": "greet", "match": {"last_user": "^hello"}, "reply": {"text": "Hi there."}},
  {"reply": {"error": {"status": 503}}}
]
```

有状态的 tool-call 两步：第一轮请求没有 tool 结果，回答一次工具调用；客户端执行工具后带着结果再来，回答最终文本。

```yaml
- name: step-1
  match: { has_tool: read_file, has_tool_result: false }
  reply:
    tool_calls:
      - name: read_file
        arguments: { path: src/main.rs }
- name: step-2
  match: { last_tool_result: read_file }
  reply: { text: "main.rs 只有一个 main 函数。" }
```

要点：

- **`times`**：`times: 1` 的规则回答一次后就被跳过，之后的请求落到后面的规则。`GET /_fakellm/scripts` 里每条规则有 `hits` 和 `exhausted`。
- **旁路请求**：agent 常在主流程之外发请求（生成会话标题、做摘要）。用 `system` 匹配它们的 system prompt、`no_tools: true` 区分它们没有带工具，写在主规则**前面**，主流程的规则就不会被它们消耗。完整例子在 [docs/scripts.md](docs/scripts.md)。
- **`tool_result` 的判定**对三种协议一致：看最后一条 assistant 消息之后有没有工具结果，所以 Claude Code 那种“`tool_result` 块后面还跟一段文本”的用户消息，`has_tool_result` 仍然命中。
- 注册规则时整体校验：正则不合法、`reply` 为空、`text` 和 `error` 同时出现、`probability` 不在 0 到 1、`every_nth` 为 0、`protocol` 写错、出现未知字段都是 400，不会改动已有的规则。
- 所有 `*_ms`、`tokens_per_sec` 等延迟字段读的是服务端时钟，见下一节。

## 默认兜底与 `--strict`

没有规则命中时，默认回一段 lorem 文本，长度由 `--fallback-tokens` 决定（默认 32 token），内容是请求的哈希播种的，所以相同请求得到相同回复、不同请求不同。
兜底规则：

- 遵守 `max_tokens` / `max_completion_tokens`（截断，`finish_reason` 为 `length`；Anthropic 是 `stop_reason: max_tokens`，Responses 是 `max_output_tokens`，状态 `incomplete`，真实 API 要求它至少为 16，更小是 400）。
- `response_format`（Responses 里是 `text.format`）是 `json_object` 或 `json_schema` 时返回 `{"message": "<lorem>"}`。
- `tool_choice` 强制调用工具（`required` 或指定名字，Anthropic 的 `any` 和 `tool`）时返回一个参数为 `{}` 的工具调用；其他情况**永远不会**主动调用工具，所以 agent 循环到兜底就会结束，而不是空转。

`--strict`（或 `FAKELLM_STRICT=true`，环境变量也认 `1/0`、`yes/no`、`on/off`）把兜底换成错误：没有规则命中的请求得到 400：

```json
{"error": {"message": "fakellm: no script rule matched this request (model \"gpt-4o\") and --strict is on", "type": "fakellm_error", "param": null, "code": "fakellm_no_matching_rule"}}
```

用 400 而不是 5xx，是为了让 SDK 不重试、测试立刻失败。Anthropic 路由的 strict 错误是同样的状态码，错误体是 Anthropic 的格式（`type: fakellm_error`，没有 `code`）。

## 虚拟时钟与流式节奏

服务端有一个自己的时钟（每个 `Server` 一个，同一个进程里的多个服务互不影响）。`created`、请求日志的时间戳、首 token 延迟、token 间隔、`retry-after` 都读它。控制面可以冻结、跳转、推进它：

```bash
fakellm ctl clock freeze                       # 冻结在当前时间；也可以 freeze 2030-01-02T03:04:05Z
fakellm ctl clock advance 500ms                # 推进，单位 ns us ms s m h d w，可以组合：1h30m
```

节奏由规则的 `reply.stream` 和 `delay_ms` 决定（命令行的 `--ttft`、`--tokens-per-sec` 是默认值）：

```yaml
- reply:
    text: "one two three"
    stream: { ttft_ms: 500, tokens_per_sec: 10 }
```

时钟冻结时，响应头立刻发出，但**一个 chunk 都不会发**；`advance 500ms` 后放行首个 chunk（role chunk 和第一个 token），之后每个 token 再等 100ms。一次性 `advance 10s` 则剩下的全部放行。测试因此不需要真的 sleep，也没有竞态：

```js
await ctl.post('/clock/freeze');
const stream = await client.chat.completions.create({ model: 'gpt-4o', messages, stream: true });
const first = stream[Symbol.asyncIterator]().next();   // 此时挂起
await ctl.post('/clock/advance', { by: '500ms' });     // 现在 first 完成
```

非流式响应在 `ttft + token 间隔 × (token 数 - 1)` 之后才返回，模拟真实延迟，同样跟着时钟走。

**边界**：虚拟时钟只管服务端。**管不住客户端自己的 backoff 定时器**：ai-sdk 重试时的 2s / 4s、openai SDK 的退避都是客户端进程里的真实定时器，`advance` 对它们无效。测重试逻辑时要么关掉重试（`maxRetries: 0`），要么在客户端侧用假定时器。`retry-after` 响应头的值是对的，只是等待发生在客户端。

## 请求日志

每个 API 请求（包括被拒绝的：认证失败、JSON 不合法）都会记入一个有上限的内存环形缓冲（`--request-log-capacity`，默认 10000，满了丢最旧的；`--no-request-log` 关闭）。控制面请求本身不记录。三种协议的请求记在同一个日志里，用 `protocol` 区分（`ctl requests list --protocol anthropic.messages`）。每条记录有原始 JSON body（`full=true` 时）、解析后的结构化请求、回复、状态码、来源（哪条规则、兜底、strict 还是被拒绝）：

```bash
fakellm ctl requests list --has-tool read_file --source script
fakellm ctl requests list --count --status 4xx
fakellm ctl --json requests list --limit 1 --full
```

字段和过滤条件见 [docs/control-plane.md](docs/control-plane.md#请求日志)。日志里保存完整文本，不截断 system prompt。`authorization` 头的值默认也原样保存，方便断言客户端发的 key；要导出日志给别人时用 `--redact-auth`。被 `hang` 挂住、客户端放弃的请求以 `status: 0` 记入。

## 错误与状态码

错误体跟着请求的协议走。Chat 和 Responses 与 OpenAI 一致：`{"error": {"message", "type", "param", "code"}}`，四个键总是都在（没有值时为 `null`）；Anthropic Messages 是 `{"type": "error", "error": {"type", "message"}}`，只有这两个键，规则里写的 `code` 和 `param` 在这里被丢掉。
规则里只写 `status` 就得到下表的默认值，`type` / `code` / `message` / `param` 都可以覆盖，`retry_after`（秒）写入 `retry-after` 头，`headers` 可以加任意响应头。

OpenAI 系（Chat、Responses）的默认值：

| status | `type` | `code` | message |
|---|---|---|---|
| 400 | `invalid_request_error` | null | Invalid request. |
| 401 | `invalid_request_error` | `invalid_api_key` | Incorrect API key provided. ... |
| 403 | `invalid_request_error` | null | You do not have access to this resource. |
| 404 | `invalid_request_error` | `model_not_found` | The model `unknown` does not exist ...（写死的是 `unknown`，不是请求里的模型名） |
| 408 | `timeout` | null | Request timed out. |
| 409 | `server_error` | null | Conflict. |
| 413 | `invalid_request_error` | `request_too_large` | Request too large. |
| 429 | `requests` | `rate_limit_exceeded` | Rate limit reached for requests. Please try again later. |
| 500 | `server_error` | null | The server had an error while processing your request. Sorry about that! |
| 502 | `server_error` | null | Bad gateway. |
| 503 | `server_error` | null | The engine is currently overloaded, please try again later. |
| 529 | `overloaded_error` | null | Overloaded（没有标准状态短语） |
| 其他 >= 500 | `server_error` | null | The server had an error while processing your request. |
| 其他 | `invalid_request_error` | null | Request failed with status N. |

Anthropic Messages 的默认值（只有 `type` 和 `message`）：

| status | `type` | message |
|---|---|---|
| 400 | `invalid_request_error` | Invalid request. |
| 401 | `authentication_error` | invalid x-api-key |
| 403 | `permission_error` | Your API key does not have permission to use the specified resource. |
| 404 | `not_found_error` | Not found. |
| 408 / 504 | `timeout_error` | Request timed out. |
| 409 | `api_error` | Conflict. |
| 413 | `request_too_large` | Request exceeds the maximum allowed number of bytes. |
| 429 | `rate_limit_error` | This request would exceed your organization's rate limit. |
| 529 | `overloaded_error` | Overloaded |
| 其他 >= 500 | `api_error` | Internal server error |
| 其他 | `invalid_request_error` | Request failed. |

配额耗尽、上下文溢出这类客户端会按 `code` 或文案分类的错误，在规则里自己写：

```yaml
- match: { last_user: "quota" }
  reply:
    error: { status: 429, type: insufficient_quota, code: insufficient_quota, message: "You exceeded your current quota, please check your plan and billing details." }
- match: { last_user: "huge" }
  reply:
    error:
      status: 400
      code: context_length_exceeded
      param: messages
      message: "This model's maximum context length is 128000 tokens. However, your messages resulted in 200000 tokens. Please reduce the length of the messages."
```

服务自己产生的错误（规则无法覆盖的）：

| 情况 | 状态码 | `code` |
|---|---|---|
| 配了 `--api-key` 但 key 不对（`Authorization: Bearer` 或 `x-api-key`） | 401 | `invalid_api_key` |
| body 不是合法 JSON | 400 | null（文案与 OpenAI 相同；Anthropic 路由是 “The request body is not valid JSON.”） |
| 缺 `messages` | 400 | `missing_required_parameter` |
| 缺 `model` | 400 | null，`you must provide a model parameter` |
| 字段类型不对 | 400 | `invalid_type` |
| `max_tokens` / `n` 为 0 | 400 | `integer_below_min_value` |
| body 超过 4 MiB | 413 | null |
| 不支持的路径 | 404 | null，`Invalid URL (POST /v1/images/generations)` |
| `GET /v1/models/{id}` 不在 `--models` 里 | 404 | `model_not_found` |
| strict 模式没有规则命中 | 400 | `fakellm_no_matching_rule` |
| Responses 缺 `input`（又没有 `previous_response_id`） | 400 | `missing_required_parameter` |
| Responses 的 `max_output_tokens` 小于 16 | 400 | `integer_below_min_value` |
| `previous_response_id` 找不到 | 404 | `previous_response_not_found` |
| Anthropic 缺 `max_tokens` / `model` / `messages`、角色不对 | 400 | `invalid_request_error`，文案是 Anthropic 的 `field: problem` 风格，如 “max_tokens: Field required” |
| Anthropic `thinking.budget_tokens` 小于 1024 或不小于 `max_tokens` | 400 | 同上，与真实 API 一样 |

Anthropic 路由上，上面的错误都用 Anthropic 的错误体，`code` 和 `param` 不出现。

每个响应都带 `x-request-id` 和 `openai-processing-ms`；Anthropic 路由还带值相同的 `request-id`。`x-request-id` 由请求内容和服务端的一个计数器派生：内容相同的两次请求（比如重试）得到不同的值，`reset` 会把计数器清零。

## 故障注入

客户端要处理的各类失败，都用剧本里的 `error`、`stream` 和 `hang` 写出来，三种协议各有自己的错误体（见上一节）。`match` 里加 `protocol` 可以给不同协议写不同的错误，`times` / `every_nth` / `probability` 决定失败几次、多久一次。完整的类别表、所有例子和每个字段的语义见 [docs/scripts.md](docs/scripts.md#故障注入每一类怎么写)。

| 类别 | Chat / Responses | Anthropic |
|---|---|---|
| `auth` | `error: { status: 401 }` | `error: { status: 401 }` |
| `quota` | `status: 429, type: insufficient_quota, code: insufficient_quota` | `status: 400, type: invalid_request_error`，文案 “...reached your specified workspace API usage limits.” |
| `rate_limited` | `status: 429, retry_after: 2` | 同左 |
| `billing` | `status: 400, code: billing_hard_limit_reached` | `status: 402, type: billing_error` |
| `model_unavailable` | `status: 404`（默认 `model_not_found`） | `status: 404`（默认 `not_found_error`） |
| `input_too_large` | `status: 400, code: context_length_exceeded, param: messages` | `status: 400`，文案 “prompt is too long: N tokens > M maximum”；请求体太大用 `status: 413` |
| `overloaded` | `status: 503`（或 529） | `status: 529` |
| `provider` | `status: 500`（或 502、504） | `status: 500` |
| 流中途干净结束 | `stream: { cut_after_chunks: N }` | 同左 |
| 流中途连接中断 | `stream: { reset_after_chunks: N }`；同步响应 `reply: { reset: true }` | 同左 |
| 挂起 | `hang: true`，或 `hang_ms: N` | 同左 |
| 流内错误 | `error: { ..., in_stream: true }` | 同左 |

这些类别的名字来自 oar 这类 agent 运行时的错误分类；oar 的分类代码不在本仓库里，上表是按各家真实 API 的状态码、`type`、`code` 和文案写的，没有对照 oar 验证过。如果你的分类器看别的字段，改 `type` / `code` / `message` 即可。

```yaml
- match: { last_user: "quota" }
  reply: { error: { status: 429, type: insufficient_quota, code: insufficient_quota, message: "You exceeded your current quota." } }
- match: { last_user: "flaky" }                   # 每第 3 个请求返回 500，要求 SDK 不要重试
  reply: { error: { status: 500, headers: { x-should-retry: "false" } } }
  # 在 match 里再加 every_nth: 3 即可
- match: { last_user: "cut" }                     # 发出两个 content chunk 后流消失
  reply: { text: "one two three four", stream: { cut_after_chunks: 2 } }
- match: { last_user: "rst" }                     # 发出两个 content chunk 后连接中断，没有结尾 chunk
  reply: { text: "one two three four", stream: { reset_after_chunks: 2 } }
- match: { last_user: "hang" }                    # 连响应头都不发，直到客户端超时
  reply: { hang: true }
```

- **重试头**：`error.headers` 可以加 `retry-after-ms`、`x-should-retry`（官方 SDK 用它覆盖自己的重试判断），`retry_after` 只写 `retry-after`。
- **流中途断开有三种**：`cut_after_chunks` 是干净地结束 chunked body（没有 finish 事件，没有 `[DONE]`；客户端看到流提前结束）；`in_stream: true` 的错误是 200 的流里出现一个错误事件（Chat 是 `data: {"error": ...}` 加 `[DONE]`，Anthropic 是 `event: error`，Responses 是 `response.failed`）。`stream.reset_after_chunks`（同步响应用 `reply.reset: true`）在同一位置直接关闭连接，不发结尾的 `0\r\n\r\n`：连接以 FIN 关闭而不是 RST，客户端看到 `premature close` / `terminated` / `unexpected EOF`，通常不是 `ECONNRESET`。
- **挂起**：`hang` 的请求一被挂住就以 `status: 0`、`hung: true` 记入请求日志，测试不用等客户端放弃就能看到它；`hang_ms` 读服务端时钟，冻结时钟时一直挂着，`advance` 后放行；`reset` 会唤醒所有挂住的请求（有 `hang_ms` 的按规则其余部分回答，只写 `hang: true` 的回 503）。详见[控制面](#控制面)。
- **codex 的限制**：codex 对 HTTP 400 的 body 不分类（一律是不重试的 `InvalidRequest`），要让它认出 `context_length_exceeded`、`insufficient_quota`、`rate_limit_exceeded`、`server_is_overloaded`，错误必须是流内的 `response.failed`，也就是规则里写 `in_stream: true`。写法见 [docs/scripts.md](docs/scripts.md#示例五codex-的上下文溢出)。codex 的 `wire` 配置同时把它的重试关掉了，并把空闲超时降到 5 秒，所以挂起超过 5 秒的流会被它当作超时。

## Embeddings

`POST /v1/embeddings`（也接受 `/embeddings` 和任意前缀）：`input` 可以是字符串、字符串数组、一个 token 数组或 token 数组的数组；`dimensions` 默认 1536，`encoding_format` 支持 `float` 和 `base64`。
向量由模型名和输入内容的哈希生成并做 L2 归一化，所以同一输入（同一模型）每次都得到同一个向量，不同输入得到不同向量，同一批里长度一致。`usage.prompt_tokens` 等于 `total_tokens`。
剧本里只有带 `error` 的规则对这个端点生效。

## Fake agent

在 agent 循环里 fakellm 扮演模型，被测的 agent（codex、Claude Code、opencode 或你自己的循环）自己执行工具、把结果发回来，所以测的是 agent 框架：工具分发、结果回传、重试、中断。

```
agent 框架 ──messages + tools──▶ fakellm（扮演模型）
     ▲                               │
     │                     按轨迹或策略返回 tool_call
     │                               ▼
  执行工具 ◀─────────────────── tool_call
     │
     └──tool result──▶ fakellm 检查结果，然后走下一步
```

- **轨迹（trajectory）**：一次运行写成回合列表（JSON、YAML 或 JSONL），按请求里的历史选步骤，所以重试和并行对话互不干扰；每步可以用 `expect` 检查客户端送回来的工具结果，偏离时 `strict` 报 400 并给出差异、`lenient` 照常继续、`stop` 直接收尾；`tool_map` 把数据集里的工具名和参数（`bash {command}`）改写成你的 agent 的写法（`run_shell {cmd}`）。
- **策略脚本（policy）**：一个 JS 模块 `decide(request, context)` 看请求返回回复，状态由 fakellm 保存；运行时是 txiki.js（`tjs`）、Node 或 Bun。**注册策略就是在本机执行代码**，不要把控制面暴露出去。

```bash
fakellm --trajectory fix-bug.yaml --policy reader.mjs --scripts rules.yaml
```

```yaml
# rules.yaml
- { name: side, match: { no_tools: true }, reply: { text: ok } }   # 标题之类的旁路请求
- { name: agent, reply: { trajectory: fix-bug } }                 # 或 policy: reader
```

端点 `/_fakellm/trajectories` 和 `/_fakellm/policies`（见[控制面](docs/control-plane.md)）；`sdk/js`、`bindings/node`、Go helper 都有对应的 `trajectories` / `policies` 客户端，`fakellm ctl` 暂时没有。格式、步骤选择、偏离处理、工具映射、策略协议、运行时、超时和三个完整例子（包括 SWE-agent 风格的轨迹）见 [docs/agent.md](docs/agent.md)；`e2e/codex-trajectory.sh`、`claude-trajectory.sh`、`opencode-policy.sh` 用真实的 codex、Claude Code、opencode 跑通了它。

## Responses 的状态与 `previous_response_id`

`POST /v1/responses` 默认把每个响应保存在内存里（请求带 `store: false` 则不存），供三件事用：

- `previous_response_id`：新请求的对话是“被引用响应的完整对话 + 新 `input`”（`instructions` 不继承，和真实 API 一样要重发）。请求日志和剧本条件（`turn`、`has_tool_result`）看到的是展开后的整个对话。找不到时返回 404，`code: previous_response_not_found`。
- `GET /v1/responses/{id}` 读回响应对象，`DELETE /v1/responses/{id}` 删除，未知 id 是 404。
- 保存的是快照：删除一个响应，不影响已经从它派生出的响应。

规则：最多保存 1000 个，满了丢最旧的；被 `cut_after_chunks` 截断的流也照常保存；`ctl reset` 默认连同它们一起清（`--keep-responses` 保留），控制面 `GET` / `DELETE /_fakellm/responses` 查看和清空（见 [docs/control-plane.md](docs/control-plane.md#responses-对象)）。
codex 走 HTTP，不发 `previous_response_id`（`store` 恒为 `false`，每次重发完整历史），所以对 codex 这条路径用不上；`openai` SDK 和 ai-sdk 的 Responses 会用。

```js
const a = await openai.responses.create({ model: 'gpt-5.5', input: 'ping' });
const b = await openai.responses.create({ model: 'gpt-5.5', input: 'and again', previous_response_id: a.id });
```

## Rust 库模式

`fakellm` 同时是一个库，测试里直接在进程内启动，不需要子进程。没有发布到 crates.io，用 git 依赖：

```toml
[dev-dependencies]
fakellm = { git = "https://github.com/AkaraChen/fakellm" }
```

异步测试：

```rust
let server = fakellm::Server::start(fakellm::Config::default()).await?;
let base_url = server.base_url(); // "http://127.0.0.1:<随机端口>/v1"，endpoint() 是不带 /v1 的
// ... 把 OpenAI 客户端指向 base_url ...
server.shutdown().await;
```

同步测试用 `Server::start_blocking`：服务跑在自己的后台 Tokio runtime 上（最多 4 个 worker 线程），句柄 drop 时自动停止。不要在 async 上下文里调用它。

```rust
let server = fakellm::Server::start_blocking(fakellm::Config::default())?;
let rules = fakellm::Rule::parse_many(
    r#"[{"match": {"last_user": "ping"}, "reply": {"text": "pong"}}]"#,
    fakellm::ScriptFormat::Json,
)?;
server.scripts().replace(rules)?;
// ... 发请求 ...
let (rows, total) = server.requests(&fakellm::RequestFilter::default());
assert_eq!(rows[0].model(), Some("gpt-4o"));
assert_eq!(server.scripts().list()[0].hits, 1);
server.clock().freeze(None)?;
server.reset();
```

| 方法 | 作用 |
|---|---|
| `addr()` / `endpoint()` / `base_url()` | 实际绑定的地址 / `http://host:port` / 加了 `/v1` |
| `scripts()` | `replace` / `append` / `clear` / `list`（带 `hits` 和 `exhausted`） |
| `requests(&RequestFilter)` | 日志记录（旧到新）和过滤后（`limit` 之前）的总数 |
| `clock()` | `freeze` / `unfreeze` / `set` / `advance` / `reset` / `now` |
| `control()` | 控制面的全部操作，包括 `reset_with(ResetOptions)` |
| `response_ids()` / `clear_responses()` | 保存的 Responses 对象的 id 列表 / 清空它们 |
| `reset()` | 清空剧本、日志、时钟、已存的 response |
| `shutdown()` | 停止接受连接（async），`BlockingServer::shutdown` 是同步版本 |

`Config` 的字段和下面的命令行参数一一对应（包括 `redact_auth` 和 `delivery` 里的 `chunk_tokens`、`args_chunk_bytes`）；`Config::default()` 监听随机端口、接受任何 key、非 strict、无延迟。多个 `Server` 之间状态完全独立。`Server::wait()` 会一直等到进程退出（二进制就是这么用的）。

## Docker 镜像

**镜像还没有发布**：`.github/workflows/release.yml` 已经写好（推送 `v*` tag 时构建 `linux/amd64` 和 `linux/arm64`、推送到 `ghcr.io/akarachen/fakellm` 并把静态二进制挂到 GitHub release），但还没有推过 tag，所以下面的 `ghcr.io` 地址现在拉不到。发布之前自己构建：

```bash
docker build -t fakellm .
docker run --rm -p 4010:4010 fakellm
# http://0.0.0.0:4010
```

- 镜像基于 Alpine，里面只有一个静态链接的 musl 二进制，以非 root 用户（uid 10001）运行；默认监听 `0.0.0.0:4010`（`FAKELLM_HOST` / `FAKELLM_PORT`），配置项见[配置](#配置)，环境变量和命令行参数都可以，如 `docker run fakellm --strict`。启动完成后 stdout 第一行是 `http://0.0.0.0:4010`，可以作为就绪信号。
- 带 `HEALTHCHECK`（`GET /_fakellm`），只跟随 `FAKELLM_PORT`；用 `--port` 参数改端口，或只跑 `wire` / `ctl` 这类一次性命令时加 `docker run --no-healthcheck`。
- 镜像里没有 JS 运行时，[策略脚本](docs/agent.md#策略脚本)跑不起来（轨迹不受影响）；需要策略时基于它做派生镜像 `apk add nodejs`。
- 控制面没有认证，容器里监听 `0.0.0.0`：只发布到本机（`-p 127.0.0.1:4010:4010`），不要暴露到公网。
- 发布后的 tag 是 `latest`、完整版本（`0.1.0`）、`主版本.次版本`（`0.1`）和提交短 sha；手动触发 release 工作流时 `publish` 默认是 `false`，只构建和冒烟测试。`e2e/docker.sh` 是本地的镜像冒烟脚本（构建、随机端口启动、请求一次 chat completion）。

### 在 Testcontainers 里使用

不需要专门的模块：通用容器加等待策略就够了。暴露 4010 端口，等 stdout 里出现 `http://`，用映射出来的主机和端口访问。OpenAI 客户端的 base URL 加 `/v1`，Anthropic SDK 不加；每个测试开始前调 `POST /_fakellm/reset`。下面的镜像名在发布后才可用；发布前先 `docker build -t fakellm .` 并把镜像名换成 `fakellm`。

Node.js / TypeScript（[testcontainers-node](https://github.com/testcontainers/testcontainers-node)）：

```ts
import { GenericContainer, Wait } from "testcontainers";

const container = await new GenericContainer("ghcr.io/akarachen/fakellm:latest")
  .withExposedPorts(4010)
  .withWaitStrategy(Wait.forLogMessage("http://"))
  .start();
const endpoint = `http://${container.getHost()}:${container.getMappedPort(4010)}`;
await fetch(`${endpoint}/_fakellm/scripts`, { method: "PUT", body: JSON.stringify([{ reply: { text: "pong" } }]) });
// new OpenAI({ baseURL: `${endpoint}/v1`, apiKey: "test" })
await container.stop();
```

Python（[testcontainers-python](https://github.com/testcontainers/testcontainers-python)）：

```python
from testcontainers.core.container import DockerContainer
from testcontainers.core.waiting_utils import wait_for_logs

with DockerContainer("ghcr.io/akarachen/fakellm:latest").with_exposed_ports(4010) as c:
    wait_for_logs(c, "http://")
    endpoint = f"http://{c.get_container_host_ip()}:{c.get_exposed_port(4010)}"
    # OpenAI(base_url=endpoint + "/v1", api_key="test")
```

Go（[testcontainers-go](https://golang.testcontainers.org/)）：

```go
c, err := testcontainers.GenericContainer(ctx, testcontainers.GenericContainerRequest{
    ContainerRequest: testcontainers.ContainerRequest{
        Image:        "ghcr.io/akarachen/fakellm:latest",
        ExposedPorts: []string{"4010/tcp"},
        WaitingFor:   wait.ForLog("http://"),
    },
    Started: true,
})
host, _ := c.Host(ctx)
port, _ := c.MappedPort(ctx, "4010")
endpoint := fmt.Sprintf("http://%s:%s", host, port.Port())
```

Java（[Testcontainers for Java](https://java.testcontainers.org/)）：

```java
GenericContainer<?> llm = new GenericContainer<>("ghcr.io/akarachen/fakellm:latest")
    .withExposedPorts(4010)
    .waitingFor(Wait.forLogMessage(".*http://.*\\n", 1));
llm.start();
String endpoint = "http://" + llm.getHost() + ":" + llm.getMappedPort(4010);
```

这些容器片段只照 fakes3 的写法改了端口和镜像名，没有对着真实的 testcontainers 运行过（镜像尚未发布，也没有在本机构建）。

## JS / TS 客户端（sdk/js）

[`sdk/js`](sdk/js/README.md) 是纯 TypeScript 的客户端：启动 `fakellm` 二进制、从 stdout 读到地址，再通过 `fetch` 调用[控制面](docs/control-plane.md)。Node、Bun、vitest、node:test 都能用。**尚未发布到 npm**，目前从 git 路径安装，并且需要一个 `fakellm` 二进制（`cargo build` 得到 `target/debug/fakellm`，或设 `FAKELLM_BIN`，或放在 `PATH` 上）：

```bash
npm install --save-dev "github:akarachen/fakellm#path:sdk/js"
```

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

| 名字 | 说明 |
|---|---|
| `start(options?)` | 启动二进制，返回 `FakeLLM`（选项 `bin`、`host`、`port`、`apiKey`、`models`、`strict`、`scripts`、`trajectories`、`policies`、`policyRuntime`、`pathPrefix`、`ttft`、`tokensPerSec`、`args`、`env`、`readyTimeoutMs`） |
| `connect(url)` | 连接已经在运行的服务，不管理进程 |
| `llm.url` / `baseURL` / `anthropicBaseURL` | `http://host:port` / 加了 `/v1`（OpenAI SDK） / 同 `url`（Anthropic SDK 自己加 `/v1`） |
| `llm.scripts` | `put`（替换）、`add`、`list`（带 `hits`、`exhausted`）、`clear` |
| `llm.trajectories` / `llm.policies` | 各有 `put`（替换）、`add`、`list`、`clear`，见 [Fake agent](#fake-agent) |
| `llm.requests` | `list`、`count`、`clear`、`config`、`dropped`；过滤字段与请求日志的查询参数同名 |
| `llm.clock` | `now`、`state`、`freeze`、`unfreeze`、`set`、`advance`（`'500ms'`、`'1h30m'` 或毫秒数）、`reset` |
| `llm.responses` | `list`、`clear` |
| `llm.reset(opts?)` | 清空剧本、日志、时钟、Responses；`{ clock: false }` 等保留其中一项 |
| `llm.wire(target)` | 等同 `fakellm wire`，返回 `{ env, files, snippet }`；CLI 的 `--file NAME` 取 `files[NAME]` |
| `text()`、`toolCall()`、`error()` | 构造规则的小工具 |
| `llm.stop()` | 结束子进程并等它退出，可重复调用，也支持 `await using` |

控制面会拒绝带 `Origin` 头的非 GET 请求，所以不要在浏览器页面里用这个包。完整说明见 [sdk/js/README.md](sdk/js/README.md)。

## Node.js 原生绑定（bindings/node）

[`bindings/node`](bindings/node/) 是一个 [napi-rs](https://napi.rs) 插件，服务器直接跑在 Node / Bun 进程里：不起子进程、不需要二进制，也不用协商端口。包名 `@fakellm/native`，**尚未发布到 npm**（`node.yml` 在推送 `v*` tag 时发布，其余情况只做 `npm publish --dry-run`）。控制面的操作是同步方法，不走 HTTP（`policies.put` / `add` 要启动并加载策略子进程，会阻塞 JS 线程约 100 ms）：

```js
import { createRequire } from 'node:module';
const { start } = createRequire(import.meta.url)('@fakellm/native');

const server = await start({ scripts: [{ match: { last_user: 'ping' }, reply: { text: 'pong' } }] });
// new OpenAI({ baseURL: server.baseUrl })  new Anthropic({ baseURL: server.anthropicBaseUrl })
server.scripts.list();                  // 同步，带 hits
server.requests.count({ protocol: 'openai.chat' });
server.clock.freeze();
server.clock.advance('500ms');          // 数字按毫秒，字符串如 '1h30m'
server.reset();
await server.stop();                    // 关闭监听和所有未结束的连接（包括挂起的请求）
```

`start(options?)` 的选项：`host`、`port`、`apiKey`、`models`、`strict`、`scripts`（规则数组，或 JSON / YAML 字符串）、`trajectories`、`policies`（数组，或 JSON / YAML / JSONL 字符串）、`policyRuntime`、`pathPrefix`、`ttft`、`tokensPerSec`、`fallbackTokens`、`chunkTokens`、`argsChunkBytes`、`requestLog`、`requestLogCapacity`、`redactAuth`。服务器对象的属性有 `endpoint`、`baseUrl`、`anthropicBaseUrl`、`host`、`port`，以及 `scripts`、`trajectories`、`policies`、`requests`、`responses`、`clock`、`reset()`、`status()`、`wire(target, format?)`、`stop()`。类型声明由 `napi build` 生成（`index.d.ts`）。没有调用 `stop()` 的服务器对象被垃圾回收时也会停止，所以在用到它的地址期间要保留引用。

- **Bun**：同一个 `.node` 在 Bun 下可以加载，`bun test __test__/openai.test.mjs` 在本机通过（20 通过，1 跳过：那一条需要 `node --expose-gc`）；CI 的 `napi-bun` 任务会跑它。Bun 的 napi 实现和 Node 不是同一个，除这个测试覆盖的内容外没有别的保证。
- **平台**：构建目标是 Linux 的 x64 / arm64，glibc 和 musl；macOS 和 Windows 没有构建，也没有验证过。
- 自己编译：`cd bindings/node && npm ci && npm run build`（需要 Rust 工具链），`npm test` 用官方 `openai` 和 `@anthropic-ai/sdk` 跑一遍。

## Go helper（go/）

[`go/`](go/README.md) 在 Go 测试里启动 `fakellm` 二进制并包装控制面，用法像 testcontainers 但不需要 Docker；只用标准库，需要 Go 1.24。二进制按 `Options.Bin`、`FAKELLM_BIN`、`PATH` 的顺序查找；`fakellm.Download` 从 GitHub release 下载，但**release 现在还不存在**（第一次推 `v*` tag 之后才有），在此之前自己 `cargo build`。

```go
import fakellm "github.com/AkaraChen/fakellm/go"

func TestAgent(t *testing.T) {
	ctx := context.Background()
	s, err := fakellm.Start(ctx, fakellm.Options{
		Scripts: `
- match: { last_user: "ping" }
  reply: { text: "pong" }
`,
	})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { s.Stop() })

	// OpenAI 兼容客户端用 s.BaseURL()（带 /v1），Anthropic 客户端用 s.URL()。
	reqs, _ := s.Requests().List(ctx, fakellm.Filter{Rule: "agent-step-1"})
	_ = reqs
	s.Clock().Advance(ctx, 500*time.Millisecond)
	s.Reset(ctx, fakellm.ResetOptions{})
}
```

`Scripts()`、`Trajectories()`、`Policies()`、`Requests()`、`Clock()`、`Responses()`、`Wire` / `WireJSON`、`Status`、`Reset` 与 `fakellm ctl` 一一对应，详见 [go/README.md](go/README.md)。

## 配置

命令行参数，大多数有同名环境变量：

| 参数 | 环境变量 | 默认值 | 说明 |
|---|---|---|---|
| `--host` | `FAKELLM_HOST` | `127.0.0.1` | 监听地址 |
| `--port` | `FAKELLM_PORT` | `4010` | 监听端口，`0` 为随机；实际地址打印到 stdout |
| `--api-key` | `FAKELLM_API_KEY` | 无 | 设置后必须匹配（`Authorization: Bearer` 或 `x-api-key`），否则 401（Anthropic 路由用 Anthropic 的错误体）；不设则任意 key 都行。只管 API，不管控制面 |
| `--models` | `FAKELLM_MODELS` | 14 个常见名字（gpt-4o、gpt-5、o3、gpt-5.5、claude-sonnet-4-5、claude-opus-4-1、claude-haiku-4-5 等） | `GET /v1/models` 返回的列表，逗号分隔。不影响生成接口，那里任何模型名都接受 |
| `--strict` | `FAKELLM_STRICT` | 关 | 没有规则命中时返回 400 而不是默认回复。环境变量认 `true/false`、`1/0`、`yes/no`、`on/off` |
| `--scripts` | `FAKELLM_SCRIPTS` | 无 | 启动时加载的规则文件；`.json` / `.yaml` / `.yml` 按扩展名，其他按内容判断。规则不合法则启动失败 |
| `--fallback-tokens` | `FAKELLM_FALLBACK_TOKENS` | `32` | 默认回复的长度（token） |
| `--ttft` | `FAKELLM_TTFT` | `0` | 默认首 token 延迟，如 `300ms`、`1.5s`（单位 ns us ms s m h d w，可以带小数）。规则可覆盖 |
| `--tokens-per-sec` | `FAKELLM_TOKENS_PER_SEC` | `0` | 默认流式速度，`0` 表示 chunk 之间不等待。规则可覆盖 |
| `--chunk-tokens` | `FAKELLM_CHUNK_TOKENS` | `1` | 每个流式文本 chunk 含几个 token（至少 1），三种协议通用。规则的 `stream.chunk_tokens` 可覆盖 |
| `--args-chunk-bytes` | `FAKELLM_ARGS_CHUNK_BYTES` | `16` | 流式工具调用参数每个分片的字节数（至少 1），三种协议通用 |
| `--trajectory` | `FAKELLM_TRAJECTORIES` | 无 | 启动时加载的轨迹文件，可重复；格式按扩展名（`.json`、`.jsonl` / `.ndjson`、`.yaml` / `.yml`），其他按内容判断。不合法则启动失败 |
| `--policy` | `FAKELLM_POLICIES` | 无 | 启动时加载的策略脚本（ES 模块，导出默认函数 `decide`），可重复；策略名是文件名去掉扩展名；加载失败则启动失败 |
| `--policy-runtime` | `FAKELLM_POLICY_RUNTIME` | 自动 | 策略脚本的运行时：`tjs`、`node`、`bun` 或可执行文件路径；默认按 `tjs`、`node`、`bun` 的顺序在 `PATH` 里找 |
| `--policy-timeout-ms` | `FAKELLM_POLICY_TIMEOUT_MS` | `5000` | 一个策略做一次决定的时限（真实时间，不跟虚拟时钟）；超时杀掉进程并返回 500 |
| `--path-prefix` | `FAKELLM_PATH_PREFIX` | 无 | API 同时挂在这个前缀下，如 `/anthropic` |
| `--no-request-log` | `FAKELLM_NO_REQUEST_LOG` | 关 | 不记录请求 |
| `--request-log-capacity` | `FAKELLM_REQUEST_LOG_CAPACITY` | `10000` | 日志保留的请求数，满了丢最旧的 |
| `--redact-auth` | `FAKELLM_REDACT_AUTH` | 关 | 请求日志里把 `authorization`、`proxy-authorization`、`x-api-key` 的值记成 `<redacted>`；不设则原样保存 |

Docker 镜像把 `FAKELLM_HOST` 设为 `0.0.0.0`、`FAKELLM_PORT` 设为 `4010`（见 [Docker 镜像](#docker-镜像)）。布尔参数可以只写 `--strict` 不带值。日志级别用 `RUST_LOG`（默认 `warn`）。`fakellm ctl` 和 `fakellm wire` 的 `--endpoint` 读 `FAKELLM_ENDPOINT`（默认 `http://127.0.0.1:4010`）。`--scripts` 文件读不出来时报错信息里有文件名。

## 控制面

API 端口上 `/_fakellm/*` 是测试控制面：重置状态、注册剧本、轨迹和策略脚本、移动时钟、查询请求日志、查看和清空保存的 Responses 对象。命令行 `fakellm ctl` 和 Rust 的 `Server` 方法做同样的事。
**控制面没有认证**，只监听 `127.0.0.1` 是唯一的防护（Docker 镜像里监听 `0.0.0.0`，别把端口发布到公网）。其中 `/_fakellm/policies` 会运行代码（见 [Fake agent](#fake-agent)）。路由、请求和响应示例见 [docs/control-plane.md](docs/control-plane.md)，规则格式见 [docs/scripts.md](docs/scripts.md)。

```bash
fakellm --port 4010 &
fakellm ctl scripts put agent.yaml
fakellm ctl clock advance 30s
fakellm ctl requests list --rule step-1
fakellm ctl reset                  # 每个测试开始前
fakellm ctl requests list --status 0   # 此刻被 hang 挂住的请求（JSON 里 hung: true）
```

被挂住的请求一被挂住就进日志（`status: 0`，`hung: true`），放行或客户端放弃后原地更新；`reset` 会唤醒它们，所以测试之间调 `reset` 不会留下悬挂的连接。

## 与真实 OpenAI 接口的已知差异

- **tokenizer 是近似的**：`usage` 里的 token 数由一个确定性的内置切分器算出（带前导空格的单词算一个 token，长单词每 4 到 6 个字符切一次，每个汉字和每个标点各一个 token，不是 BPE），前后自洽但和任何真实模型都不一致。流式 chunk 按这个切分器的 token 边界切，所以文本可能在单词中间断开（`Exer` `citat` `ion`），拼起来是对的。
- 不发 `obfuscation` 字段（真实 API 用来填充的随机串，没有语义）。`system_fingerprint` 和 `service_tier: "default"` 会发。
- `model` 原样回显请求里的值，不解析别名（`gpt-4o` 不会变成 `gpt-4o-2024-08-06`）。任何模型名都接受，不校验；只有 `GET /v1/models/{id}` 会因为不在列表里返回 404。
- `n > 1` 时各 choice 的 chunk 是依次发的（choice 0 发完再发 choice 1），不像真实 API 那样交错。每个 choice 有自己的 role 和 finish chunk，格式是对的；多数客户端只读 `choices[0]`。
- `created` 来自服务端时钟，冻结时间时是固定值；completion id 和 `x-request-id` 都由请求内容和一个服务端计数器派生，所以相同请求（比如重试）的 id 也不同。
- `GET /v1/models` 的 `created` 是按名字派生的固定值，`owned_by` 总是 `openai`。
- 剧本里的 `max_tokens` 只截断 `text`；`reasoning` 和工具调用参数按原样发出。`reasoning` 作为 `reasoning_content` 发送。
- 工具调用的 `id` 不写就由请求派生；`arguments` 可以是对象（紧凑序列化）或字符串（原样发送，可以故意写成不完整的 JSON）。
- 流式工具调用的参数每 16 字节一个分片，第一片带 `id`、`type`、`name` 和 `arguments: ""`。
- `cut_after_chunks` 的断流是**干净地结束 chunked body**（没有 finish chunk，没有 `[DONE]`）；客户端看到的是流提前结束。`reset_after_chunks` / `reset` 中断连接，但是以 FIN 关闭而不是 TCP RST，所以多数客户端报 `terminated` / `premature close` / `unexpected EOF`，而不是 `ECONNRESET`。
- 不支持：`logprobs`（请求里带了会被接受并记入日志，但响应里永远是 `null`）、`prediction`、音频输入输出、图片输入只会被解析并记录。
- `/v1/chat/completions` 也可以不带 `/v1` 前缀访问（`/chat/completions`）。
- 其他入口见[协议与端点](#协议与端点)，同样可以不带 `/v1`。

### Anthropic Messages 与真实 API 的差异

- `anthropic-version` 不校验，`anthropic-beta` 只记录进请求日志（`full=true` 的 `headers`），不影响行为。
- `model` 原样回显，不解析别名；任何模型名都接受。`GET /v1/models` 的 `created_at` 是按名字派生的固定值。
- 响应体里没有顶层的 `request_id`，请求 id 只在 `request-id` 响应头里（与 `x-request-id` 同值）。
- `stop_sequence` 总是 `null`，不会因为 `stop_sequences` 命中而停；`usage` 由内置的近似 tokenizer 算出（`input_tokens` 是扣掉缓存读写之后的值，缓存字段取自规则，默认 0）。
- `thinking`：规则里写 `reasoning`（和可选的 `reasoning_opaque` 作为 `signature`）才会有 `thinking` 块，请求里开了 `thinking` 但规则没写 `reasoning` 时不会凭空生成。请求侧按真实 API 校验：`budget_tokens` 至少 1024 且小于 `max_tokens`。
- `tool_use.input`：同步响应里参数不是合法 JSON 时是 `{}`，流式响应则原样发出 `partial_json` 片段（用来测截断的 JSON）。
- 流里的 `ping` 事件会发；`message_start` 里的 `usage.output_tokens` 是 1，和真实 API 一样。流内错误是 `message_start`、`ping`、`event: error`，没有 `message_stop`。
- 请求里的 `role: system` 消息被接受并当作 system（真实 API 会拒绝），因为 Claude Code 会这么发。
- 不支持：服务端工具（`web_search` 等）的执行，`/v1/messages/batches`，Files API，`cache_control` 只解析不生效；`count_tokens` 用的是近似 tokenizer。

### Responses 与真实 API 的差异

- 只实现 HTTP（同步和 SSE），没有 WebSocket。不支持 `background`、`/cancel`、`/input_items`。
- `reasoning` 项的 `encrypted_content` 只在请求的 `include` 里有 `reasoning.encrypted_content` 时才发；值取自规则的 `reasoning_opaque`，没写则是请求派生的确定值，不是真的加密内容。
- 未知的 input 项类型（`web_search_call`、`local_shell_call`、`compaction` 等）不会让请求失败，会被接受并记入日志；`tools` 里除 `function` / `custom` 之外的类型（`namespace`、`web_search` 等）不当作可调用的工具。
- 请求声明为 `custom`（自由格式，如 codex 的 `apply_patch`）的工具，剧本里的调用会以 `custom_tool_call` 返回，`input` 取参数字符串，或参数对象里的 `input` 字段；其余工具是 `function_call`。
- `finish_reason: length` 得到 `status: incomplete`（`incomplete_details.reason: max_output_tokens`），`content_filter` 得到 `incomplete` + `content_filter`；`failed` 只出现在 `in_stream` 错误里。
- 响应对象的 `usage`、`output` 项 id（`msg_`、`fc_`、`rs_`）由请求派生，`created_at` 来自服务端时钟。
- 保存的对象最多 1000 个，见 [Responses 的状态](#responses-的状态与-previous_response_id)。

## 路线图

| 阶段 | 内容 | 状态 |
|---|---|---|
| M0 | Chat Completions（同步 + SSE，含工具调用）、`/v1/models`、剧本、默认兜底、虚拟时钟、请求日志、控制面和 `ctl`、Rust 库 | 已完成 |
| M1 | Anthropic Messages 和 Responses API（复用同一套剧本） | 已完成 |
| M1 | 故障注入（`probability`、`every_nth`、`hang`、`error.headers`、`error.in_stream`、`cut_after_chunks`、`reset_after_chunks`、按协议的错误体） | 已完成 |
| M1 | `fakellm wire` helper 和 codex / claude / opencode 的冒烟 e2e（`e2e/*.sh`，手工运行） | 已完成 |
| M2 | napi 绑定（Node 和 Bun，`bindings/node`）、JS / TS 客户端（`sdk/js`）、Go helper（`go/`） | 已完成 |
| M2 | Docker 镜像和 GHCR 发布（`Dockerfile`、`release.yml`） | 已完成 |
| M2 | e2e 进 CI、补 pi 的 e2e | 未做（`e2e/codex-faults.sh` 已覆盖 codex 的故障场景，手工运行） |
| M3 | fake agent：轨迹回放、工具映射、txiki.js / Node / Bun 策略脚本，见 [docs/agent.md](docs/agent.md) | 已完成 |
| M3 | `POST /v1/embeddings`，见 [Embeddings](#embeddings) | 已完成 |
| M3 | 可配置的路径前缀、`last_user` 忽略 `<system-reminder>`、`last_user_raw`、`stream.args_chunk_bytes` | 已完成 |
| M4 | cassette 录制回放、Hugging Face 数据集导入 | 未做 |
| M4 | 按 `json_schema` 生成结构化输出 | 未做 |

M3 已完成，下一步是 M4。完整的设计思路见 [docs/design.md](docs/design.md)，具体的待办见 [TODO.md](TODO.md)。

## 不做的事

- 不做真正的推理，不保证回复有任何“智能”。
- 不保证 tokenizer 和任何模型一致，只保证结果确定、前后自洽。
- 不做多模态生成（图片、音频输出）；输入里的图片只会被解析并记入日志。
- 不做内容审核、安全策略这类模型行为；`content_filter` 只能在剧本里通过 `finish_reason` 手动指定。
- 不持久化：重启即清空。
