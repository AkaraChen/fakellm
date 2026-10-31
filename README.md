# fakellm

一个专门给测试用的 LLM API 服务：全内存、不跑模型，启动只要几毫秒。
只复刻协议（请求和响应的格式、SSE 分帧、错误体、状态码），回复来自你写的剧本，或者一个确定性的默认回复。
和 fakes3 之于 S3 是同一个思路。目前实现的是 OpenAI Chat Completions 和 `/v1/models`，用 Rust 编写。

## 为什么

调研了九个真实的 agent 和 SDK 项目（见 [docs/research.md](docs/research.md)），没有一家用现成的假服务：
paseo 手写了约 5 个 `http.createServer` 假模型，raft 十几个测试文件各自手写假 provider，opencode 自己做了一个进程内的 Chat SSE 假后端。每家都在重复造这个轮子，而且都踩了同样几个坑：

- **启动慢**：用真实或本地推理服务做测试，启动和首包延迟以秒计。fakellm 是一个二进制或一个 Rust 库，启动几毫秒，也可以在进程内直接起。
- **时间不好控**：延迟用墙钟，abort / steer 的窗口就 flaky；retry 测试要真等 12 秒。fakellm 有服务端虚拟时钟，流式节奏、`created`、请求日志时间戳都跟着它走，冻结后一个 chunk 都不会发出，`advance` 一下立刻放行。
- **调用过程不好断言**：日志丢字段（想断言 `reasoning_effort` 却发现 mock 没记）。fakellm 记录每个请求的原始 JSON、解析后的结构、请求头和服务端的回复，通过控制面查询。
- **剧本无限重复匹配、旁路请求抢剧本**：agent 循环里同一条规则会一直命中（有人在 30 秒里被调用了 1143 次 tool），生成标题的旁路请求会吃掉本该给主流程的回复。剧本支持 `times`、`has_tool_result`、`last_tool_result`、`system`、`no_tools` 这类有状态条件。

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

Vercel AI SDK。必须用 `.chat(...)`：`openai('gpt-4o')` 默认走 Responses API，现在还没有实现（见[路线图](#路线图)）：

```js
import { generateText } from 'ai';
import { createOpenAI } from '@ai-sdk/openai';

const openai = createOpenAI({ baseURL: 'http://127.0.0.1:41873/v1', apiKey: 'test' });
const { text } = await generateText({ model: openai.chat('gpt-4o'), prompt: 'ping' });
console.log(text);
```

没有剧本时，每个请求得到一段确定性的 lorem 文本（同一个请求永远得到同一个回复）。注册一条规则：

```bash
curl -X PUT http://127.0.0.1:41873/_fakellm/scripts \
  -d '[{"match": {"last_user": "ping"}, "reply": {"text": "pong"}}]'
# {"added":1,"count":1}
```

上面两个例子这时都会打印 `pong`。`sdk/node/` 里有用官方 SDK 跑的完整测试（`cargo test` 会在装了 node 时自动运行它们，先在该目录 `npm ci`）。

## 剧本

剧本是一个有序的规则列表。每个请求从上到下找**第一条**条件全部满足、且 `times` 没用完的规则，用它的 `reply` 回答。没有规则命中时走[默认兜底](#默认兜底与---strict)。
通过 `PUT /_fakellm/scripts`、`fakellm ctl scripts put FILE`、`--scripts FILE` 或 Rust 的 `Config::scripts` 注册，格式是 JSON 或 YAML：一个规则数组，或者 `{"rules": [...]}`。

| 字段 | 含义 |
|---|---|
| `name` | 可选。规则名，出现在请求日志里，可以用 `rule=` 过滤 |
| `match` | 条件，缺省或 `{}` 匹配所有请求。所有写出的字段都要满足 |
| `reply` | 回复：`text` / `tool_calls` / `error` 三选一（`text` 和 `tool_calls` 可以同时有） |
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
| `stream` / `turn` | 只匹配流式 / 非流式；已经有几条 assistant 消息 |

正则可以写成 `"pattern"`、`"/pattern/"` 或 `"/pattern/i"`（忽略大小写）。

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
- 注册规则时整体校验：正则不合法、`reply` 为空、`text` 和 `error` 同时出现、出现未知字段都是 400，不会改动已有的规则。
- 所有 `*_ms`、`tokens_per_sec` 等延迟字段读的是服务端时钟，见下一节。

## 默认兜底与 `--strict`

没有规则命中时，默认回一段 lorem 文本，长度由 `--fallback-tokens` 决定（默认 32 token），内容是请求的哈希播种的，所以相同请求得到相同回复、不同请求不同。
兜底规则：

- 遵守 `max_tokens` / `max_completion_tokens`（截断，`finish_reason` 为 `length`）。
- `response_format` 是 `json_object` 或 `json_schema` 时返回 `{"message": "<lorem>"}`。
- `tool_choice` 强制调用工具（`required` 或指定名字）时返回一个参数为 `{}` 的工具调用；其他情况**永远不会**主动调用工具，所以 agent 循环到兜底就会结束，而不是空转。

`--strict`（或 `FAKELLM_STRICT=true`，这个环境变量只认 `true` / `false`）把兜底换成错误：没有规则命中的请求得到 400：

```json
{"error": {"message": "fakellm: no script rule matched this request (model \"gpt-4o\") and --strict is on", "type": "fakellm_error", "param": null, "code": "fakellm_no_matching_rule"}}
```

用 400 而不是 5xx，是为了让 SDK 不重试、测试立刻失败。

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

每个 API 请求（包括被拒绝的：认证失败、JSON 不合法）都会记入一个有上限的内存环形缓冲（`--request-log-capacity`，默认 10000，满了丢最旧的；`--no-request-log` 关闭）。控制面请求本身不记录。每条记录有原始 JSON body（`full=true` 时）、解析后的结构化请求、回复、状态码、来源（哪条规则、兜底、strict 还是被拒绝）：

```bash
fakellm ctl requests list --has-tool read_file --source script
fakellm ctl requests list --count --status 4xx
fakellm ctl --json requests list --limit 1 --full
```

字段和过滤条件见 [docs/control-plane.md](docs/control-plane.md#请求日志)。日志里保存完整文本，不截断 system prompt。`authorization` 头的值也原样保存，方便断言客户端发的 key。

## 错误与状态码

错误体与 OpenAI 一致：`{"error": {"message", "type", "param", "code"}}`，四个键总是都在（没有值时为 `null`）。规则里只写 `status` 就得到下表的默认值，`type` / `code` / `message` / `param` 都可以覆盖，`retry_after`（秒）写入 `retry-after` 头：

| status | `type` | `code` | message |
|---|---|---|---|
| 400 | `invalid_request_error` | null | Invalid request. |
| 401 | `invalid_request_error` | `invalid_api_key` | Incorrect API key provided. ... |
| 403 | `invalid_request_error` | null | You do not have access to this resource. |
| 404 | `invalid_request_error` | `model_not_found` | The model `unknown` does not exist ... |
| 408 | `timeout` | null | Request timed out. |
| 409 | `server_error` | null | Conflict. |
| 413 | `invalid_request_error` | `request_too_large` | Request too large.（目前响应状态码是 400，见 [TODO](TODO.md)） |
| 429 | `requests` | `rate_limit_exceeded` | Rate limit reached for requests. Please try again later. |
| 500 | `server_error` | null | The server had an error while processing your request. Sorry about that! |
| 502 | `server_error` | null | Bad gateway. |
| 503 | `server_error` | null | The engine is currently overloaded, please try again later. |
| 529 | `overloaded_error` | null | Overloaded（没有标准状态短语） |
| 其他 >= 500 | `server_error` | null | The server had an error while processing your request. |
| 其他 | `invalid_request_error` | null | Request failed with status N. |

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
| body 不是合法 JSON | 400 | null（文案与 OpenAI 相同） |
| 缺 `messages` | 400 | `missing_required_parameter` |
| 缺 `model` | 400 | null，`you must provide a model parameter` |
| 字段类型不对 | 400 | `invalid_type` |
| `max_tokens` / `n` 为 0 | 400 | `integer_below_min_value` |
| body 超过 4 MiB | 413 | null |
| 不支持的路径 | 404 | null，`Invalid URL (POST /v1/embeddings)` |
| `GET /v1/models/{id}` 不在 `--models` 里 | 404 | `model_not_found` |
| strict 模式没有规则命中 | 400 | `fakellm_no_matching_rule` |

每个响应都带 `x-request-id`（由请求内容派生，相同请求相同）和 `openai-processing-ms`。

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
| `reset()` | 清空剧本、日志、时钟 |
| `shutdown()` | 停止接受连接（async），`BlockingServer::shutdown` 是同步版本 |

`Config` 的字段和下面的命令行参数一一对应；`Config::default()` 监听随机端口、接受任何 key、非 strict、无延迟。多个 `Server` 之间状态完全独立。`Server::wait()` 会一直等到进程退出（二进制就是这么用的）。

## 配置

命令行参数，大多数有同名环境变量：

| 参数 | 环境变量 | 默认值 | 说明 |
|---|---|---|---|
| `--host` | `FAKELLM_HOST` | `127.0.0.1` | 监听地址 |
| `--port` | `FAKELLM_PORT` | `4010` | 监听端口，`0` 为随机；实际地址打印到 stdout |
| `--api-key` | `FAKELLM_API_KEY` | 无 | 设置后必须匹配（`Authorization: Bearer` 或 `x-api-key`），否则 401；不设则任意 key 都行。只管 API，不管控制面 |
| `--models` | `FAKELLM_MODELS` | 12 个常见名字 | `GET /v1/models` 返回的列表，逗号分隔。不影响 chat 接口，那里任何模型名都接受 |
| `--strict` | `FAKELLM_STRICT` | 关 | 没有规则命中时返回 400 而不是默认回复 |
| `--scripts` | `FAKELLM_SCRIPTS` | 无 | 启动时加载的规则文件；`.json` / `.yaml` / `.yml` 按扩展名，其他按内容判断。规则不合法则启动失败 |
| `--fallback-tokens` | 无 | `32` | 默认回复的长度（token） |
| `--ttft` | 无 | `0` | 默认首 token 延迟，如 `300ms`、`2s`（单位 ns us ms s m h d w，不支持小数）。规则可覆盖 |
| `--tokens-per-sec` | 无 | `0` | 默认流式速度，`0` 表示 chunk 之间不等待。规则可覆盖 |
| `--no-request-log` | 无 | 关 | 不记录请求 |
| `--request-log-capacity` | 无 | `10000` | 日志保留的请求数，满了丢最旧的 |

日志级别用 `RUST_LOG`（默认 `warn`）。`fakellm ctl` 的 `--endpoint` 读 `FAKELLM_ENDPOINT`（默认 `http://127.0.0.1:4010`）。

## 控制面

API 端口上 `/_fakellm/*` 是测试控制面：重置状态、注册剧本、移动时钟、查询请求日志。命令行 `fakellm ctl` 和 Rust 的 `Server` 方法做同样的事。
**控制面没有认证**，只监听 `127.0.0.1` 是唯一的防护。路由、请求和响应示例见 [docs/control-plane.md](docs/control-plane.md)，规则格式见 [docs/scripts.md](docs/scripts.md)。

```bash
fakellm --port 4010 &
fakellm ctl scripts put agent.yaml
fakellm ctl clock advance 30s
fakellm ctl requests list --rule step-1
fakellm ctl reset                  # 每个测试开始前
```

## 与真实 OpenAI 接口的已知差异

- **tokenizer 是近似的**：`usage` 里的 token 数由一个确定性的内置切分器算出（带前导空格的单词算一个 token，长单词每 4 到 6 个字符切一次，每个汉字和每个标点各一个 token，不是 BPE），前后自洽但和任何真实模型都不一致。流式 chunk 按这个切分器的 token 边界切，所以文本可能在单词中间断开（`Exer` `citat` `ion`），拼起来是对的。
- 不发 `obfuscation` 字段（真实 API 用来填充的随机串，没有语义）。`system_fingerprint` 和 `service_tier: "default"` 会发。
- `model` 原样回显请求里的值，不解析别名（`gpt-4o` 不会变成 `gpt-4o-2024-08-06`）。任何模型名都接受，不校验；只有 `GET /v1/models/{id}` 会因为不在列表里返回 404。
- `n > 1` 时各 choice 的 chunk 是依次发的（choice 0 发完再发 choice 1），不像真实 API 那样交错。每个 choice 有自己的 role 和 finish chunk，格式是对的；多数客户端只读 `choices[0]`。
- `created` 来自服务端时钟，冻结时间时是固定值；completion id 由请求内容和一个计数器派生，所以相同请求的 id 也不同。`x-request-id` 则只由请求内容派生。
- `GET /v1/models` 的 `created` 是按名字派生的固定值，`owned_by` 总是 `openai`。
- 剧本里的 `max_tokens` 只截断 `text`；`reasoning` 和工具调用参数按原样发出。`reasoning` 作为 `reasoning_content` 发送。
- 工具调用的 `id` 不写就由请求派生；`arguments` 可以是对象（紧凑序列化）或字符串（原样发送，可以故意写成不完整的 JSON）。
- 流式工具调用的参数每 16 字节一个分片，第一片带 `id`、`type`、`name` 和 `arguments: ""`。
- `cut_after_chunks` 的断流是**干净地结束 chunked body**（没有 finish chunk，没有 `[DONE]`），不是 TCP 重置；客户端看到的是流提前结束。
- 不支持：`logprobs`（请求里带了会被接受并记入日志，但响应里永远是 `null`）、`prediction`、音频输入输出、图片输入只会被解析并记录。
- `/v1/chat/completions` 也可以不带 `/v1` 前缀访问（`/chat/completions`）。

## 路线图

| 阶段 | 内容 | 状态 |
|---|---|---|
| M0 | Chat Completions（同步 + SSE，含工具调用）、`/v1/models`、剧本、默认兜底、虚拟时钟、请求日志、控制面和 `ctl`、Rust 库 | 本版本 |
| M1 | Anthropic Messages 和 Responses API（复用同一套剧本） | 未做 |
| M1 | 故障注入（按概率、延迟、流中途 reset 等，现在只有 `cut_after_chunks` 和剧本里的错误） | 未做 |
| M1 | `fakellm wire` helper | 未做 |
| M2 | napi / Bun 绑定 | 未做 |
| M2 | GHCR 镜像（Docker） | 未做 |
| M2 | Go helper、真实 harness 的 e2e | 未做 |
| M3 | fake agent：轨迹回放、工具映射、txiki.js 策略脚本 | 未做 |
| M4 | cassette 录制回放、Hugging Face 数据集导入 | 未做 |
| M4 | Embeddings、按 `json_schema` 生成结构化输出 | 未做 |

完整的设计思路见 [docs/design.md](docs/design.md)，具体的待办见 [TODO.md](TODO.md)。

## 不做的事

- 不做真正的推理，不保证回复有任何“智能”。
- 不保证 tokenizer 和任何模型一致，只保证结果确定、前后自洽。
- 不做多模态生成（图片、音频输出）；输入里的图片只会被解析并记入日志。
- 不做内容审核、安全策略这类模型行为；`content_filter` 只能在剧本里通过 `finish_reason` 手动指定。
- 不持久化：重启即清空。
