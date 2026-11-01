# fakellm：一个给测试用的 LLM API 服务

> 状态：想法 / 草稿（2026-10-31）。思路沿用 [fakes3](https://github.com/AkaraChen/fakes3)，理念参考 [vercel-labs/emulate](https://github.com/vercel-labs/emulate)。

## 一句话

S3 是对象存储的事实标准，OpenAI 兼容格式是 LLM API 的事实标准。fakes3 不复刻 AWS 本身，只复刻 S3 协议；fakellm 同理，只复刻 LLM 的线上协议，不做推理。

它是一个全内存的服务，几毫秒就能启动。它的回复是确定的、可以编排的、可以回放的，测试代码还可以控制它的时间和故障。官方 SDK 连上来，应该分辨不出对面是不是真模型。

## 为什么需要

现在测试调用 LLM 的代码，大概有这几种办法，各有各的毛病：

| 办法 | 问题 |
|---|---|
| 真调模型 | 慢，花钱，结果不确定，CI 里还得放 key |
| AI SDK 的 `MockLanguageModel` 这类进程内 mock | 只能在 AI SDK 内部用，测不到 HTTP、SSE、自己写的 proxy 和重试逻辑；换个 SDK 或换门语言就没法用了 |
| msw / nock 拦截 HTTP | 每个测试都要手写 SSE 分片，很容易写得和真接口不一样；没有状态 |
| VCR 式录制回放（Polly.js、vcrpy） | 只能逐字节回放，prompt 改一个字录像就失效；没法注入故障，也没法控制时间 |
| 本地跑一个小模型（ollama 之类） | 启动慢，吃资源，输出不确定，tool call 也不稳定 |

我自己写测试时最常卡在三件事上，fakellm 要分别解决：

1. **启动慢、占资源**：fakellm 毫秒级启动，可以嵌进 Rust、Bun/Node、Go 的进程里，`port: 0` 起一个随机端口，测完就扔。
2. **时间不好控制**：429 加 `retry-after`、限流窗口、首 token 延迟、token 之间的间隔，全部跟着虚拟时钟走，推进一下就测完，不用真等。
3. **调用过程不好断言**：请求日志可以直接断言发出去的 system prompt、tools、messages 对不对，重试了几次，有没有带 `previous_response_id`。

## 协议范围

所有 provider 共用一套内部表示（IR）：provider 无关的消息、内容块、tool call、usage。每个协议做一个前端，负责把请求解析成 IR，再把 IR 渲染回这个协议的格式，同步响应和 SSE 都一样。这和 fakes3 基于 s3s 的分层是一个思路。

| 协议 | 端点 | 优先级 |
|---|---|---|
| OpenAI Chat Completions | `POST /v1/chat/completions`（同步 + SSE） | P0 |
| OpenAI Models | `GET /v1/models` | P0 |
| Anthropic Messages | `POST /v1/messages`（同步 + SSE 事件流） | P1 |
| OpenAI Responses | `POST /v1/responses`、`GET/DELETE /v1/responses/{id}`，`previous_response_id` 是真的有状态 | P1 |
| OpenAI Embeddings | `POST /v1/embeddings`，向量由内容哈希生成，是确定的 | P1 |
| Gemini `generateContent` | 以后再说 | P3 |

"像真的"指的是下面这些细节，bug 往往就出在这些地方：

- SSE 的分片方式：`role` 单独占第一个 chunk；`tool_calls[].function.arguments` 按片段下发，客户端要自己按 `index` 拼起来；`stream_options.include_usage` 时最后多发一个 usage chunk；结尾是 `data: [DONE]`。
- Anthropic 的事件顺序：`message_start`、`content_block_start/delta/stop`、`message_delta`（带 `stop_reason` 和 usage）、`message_stop`，中间还有 `ping`。
- `finish_reason` / `stop_reason`：`stop`、`length`（被 `max_tokens` 截断时真的截断）、`tool_calls`、`content_filter`。
- 错误体的格式和状态码要和真接口一致：OpenAI 的 `{"error":{"type","code","message"}}`、Anthropic 的 `overloaded_error`（529），以及 `context_length_exceeded`。
- usage 由一个确定的 tokenizer 计算（可以用 tiktoken-rs 的 o200k，也可以用近似算法），保证前后自洽：同一段输入，算出来的 token 数永远一样。

## 回复从哪来（按优先级从高到低）

### 1. 剧本（script）

测试里最常用的就是这一层。用控制面或者种子文件注册规则，按条件匹配，返回预先写好的内容：

```yaml
- match:
    model: gpt-*
    last_user: /天气/            # 正则
    has_tool: get_weather
  reply:
    tool_calls:
      - name: get_weather
        arguments: { city: "上海" }
- match:
    last_tool_result: get_weather
  reply:
    text: "上海今天晴，22 度。"
    stream: { ttft_ms: 300, tokens_per_sec: 40 }   # 按虚拟时钟
- match: { any: true }
  reply:
    error: { status: 429, retry_after: 2 }
    times: 1                                         # 只命中一次，之后交给后面的规则
```

### 2. 录像（cassette）

把 fakellm 设成 `--upstream` 代理模式（上游可以是 CLIProxyAPI，也可以是任意 OpenAI 兼容服务），把真实的往返录成 JSONL，存成 IR 格式而不是原始字节。回放时按"语义"匹配，不按字节匹配：可以选择忽略 `temperature`、system prompt 里的时间戳之类的字段。

这样录一次，以后 Chat Completions、Responses、Anthropic 三种协议都能回放，因为存的是 IR。

### 3. 数据集回放（Hugging Face）

真实的对话和 agent 轨迹有两个用途：

- **当作真实的语料分布**：用来测 UI 和流式渲染（长文本、代码块、markdown、CJK），以及截断、上下文长度这些边界情况。这时不要求按 prompt 精确匹配，从数据集里抽一条回复就行（用 seed 保证每次抽到的一样）。
- **当作 agent 轨迹**：见下一节的 fake agent。

数据集不打包进 fakellm 发布，提供一个导入命令，转成 cassette 格式：

```bash
fakellm import hf nebius/SWE-agent-trajectories --split train --limit 200 -o fixtures/swe.jsonl
```

候选数据集（2026-10-31 查过 HF API，许可证以数据集页面为准）：

| 数据集 | 内容 | 许可证 | 备注 |
|---|---|---|---|
| `allenai/WildChat-1M` | 真实用户对话 | ODC-BY | 不需要申请访问，适合做语料分布 |
| `anon8231489123/ShareGPT_Vicuna_unfiltered` | ShareGPT 对话 | 标的是 apache-2.0 | 数据来源有争议，只建议本地使用 |
| `lmsys/lmsys-chat-1m` | 真实对话 | 自定义协议，需申请访问 | 不能再分发，只能用户自己导入 |
| `NousResearch/hermes-function-calling-v1` | 函数调用多轮对话 | apache-2.0 | 适合测 tool call |
| `Salesforce/xlam-function-calling-60k` | 函数调用 | CC-BY-4.0，需申请访问 | 单轮为主 |
| `nebius/SWE-agent-trajectories` | SWE-agent 完整轨迹 | CC-BY-4.0 | fake agent 的主力素材 |
| `SWE-bench/SWE-smith-trajectories` | SWE-agent 轨迹 | MIT | 同上 |
| `SWE-Gym/OpenHands-Sampled-Trajectories` | OpenHands 轨迹 | 页面上没标许可证 | 用之前要确认许可证 |

### 4. 默认兜底

什么规则都没有命中时，返回一个确定的回复：按请求内容哈希生成一段 lorem 文本，长度可以配置；`response_format` 指定了 `json_schema` 时，按 schema 生成一个合法的值。这样不写任何规则，客户端也能跑通。另外提供一个严格模式开关 `--strict`：没有命中规则就直接报错，避免测试在不知情的情况下吃了兜底回复。

## Fake agent

这一块最有意思，也是 fakellm 和普通 mock 拉开差距的地方。

**关键点**：在一个 agent 循环里，fakellm 扮演的是模型这一方，工具由被测的 agent 框架自己去执行。所以 fake agent 测的就是你的 agent 框架：工具分发、结果回传、上下文管理、重试、中断、人工审批，这些都能测到。

```
agent 框架 ──messages + tools──▶ fakellm（扮演模型）
     ▲                               │
     │                     按轨迹或策略返回 tool_call
     │                               ▼
  执行工具 ◀─────────────────── tool_call
     │
     └──tool result──▶ fakellm 检查结果，然后走下一步
```

可以做成三种模式：

1. **轨迹回放（trajectory）**：按步骤推进一条录好的轨迹（来自 cassette 或者 HF 导入的数据）。每一步都检查客户端送回来的 tool result，发现偏离轨迹时按策略处理：
   - `strict`：直接报错，测试失败，并给出差异。
   - `lenient`：忽略 tool result 的内容，照原样继续往下走。
   - `stop`：直接给出最终回复，结束这次运行。
2. **工具名映射**：数据集里的工具（比如 SWE-agent 的 `bash`、`str_replace_editor`）和你的 agent 注册的工具多半对不上。用一份映射表把参数转换一下，例如 `bash {command}` 转成 `run_shell {cmd}`。轨迹里有、但客户端没注册的工具，按 `stop` 处理，或者报错。
3. **策略脚本（policy）**：用 JS 写一个小状态机，根据当前的 messages 和 tools 决定下一步，脚本由 [txiki.js](https://github.com/saghul/txiki.js)（QuickJS-ng + libuv）执行。例如："还没读过文件就调用 `read_file`；读过了就调用 `edit_file`；收到工具报错就重试一次，然后放弃"。适合测各种分支，不依赖录像。

可以顺带支持的 agent 场景：

- 并行 tool call（一个回复里带多个 `tool_calls`）。
- 模型发来格式错误的 tool 参数（JSON 不合法、缺字段），测框架的容错。
- 跑到一半上下文超长（`context_length_exceeded`），测压缩和截断逻辑。
- 流式输出到一半断开，测恢复和续写。
- Anthropic 的 `thinking` 块、OpenAI Responses 的 reasoning item，测框架是不是正确地把它们回传了。

## 控制面

沿用 fakes3 的 `/_fakes3/*` 设计，换成 `/_fakellm/*` 前缀，命令行是 `fakellm ctl`，Rust 和 napi 绑定里有对应的方法：

| 接口 | 作用 |
|---|---|
| `POST /_fakellm/reset` | 清空剧本、Responses 状态、请求日志、时钟 |
| `PUT /_fakellm/scripts` | 注册或替换规则 |
| `POST /_fakellm/cassettes` | 加载录像或轨迹 |
| `/_fakellm/clock` | 冻结、推进或设置虚拟时间；SSE 的 token 节奏、限流窗口、`retry-after` 都跟着它走 |
| `POST /_fakellm/faults` | 按模型、按第几次请求、按概率注入故障：429、500、529、超时、流中途断开、首 token 很慢 |
| `GET /_fakellm/requests` | 结构化的请求日志：解析好的 IR 加原始请求，可以按模型和工具过滤 |
| `POST /_fakellm/snapshots`、`.../restore` | 保存和恢复整个状态 |
| `GET /_fakellm/usage` | 累计的 token 用量，用来测计费和配额逻辑 |

## 分发

和 fakes3 一样：

- Rust 库：`fakellm::Server::start()`，以及 `BlockingServer`。
- Bun / Node：napi-rs 绑定。Bun 对 napi 的兼容性要单独在 CI 里测。
- Go：没有原生绑定，提供一个小 helper 包，负责下载和启动二进制、等待就绪信号、提供控制面客户端（和 testcontainers 的用法类似，但不需要 Docker）。
- Docker：`ghcr.io/akarachen/fakellm`，可以直接配合 testcontainers 的通用容器。
- 认证默认宽松，任何 `Authorization: Bearer xxx` 或者 `x-api-key` 都放行；`--api-key` 可以开启严格校验，用来测 401。

## 一致性测试

和 fakes3 的 matrix 一样，拿官方客户端当裁判：

- `openai`（Node + Python）、`@anthropic-ai/sdk`、Vercel AI SDK（`@ai-sdk/openai`、`@ai-sdk/anthropic`）、LangChain，Go 端可以用 `openai-go`。
- 录制对照：同一组请求分别打真实的上游（通过 CLIProxyAPI）和 fakellm，比较两边的 SSE 事件序列和响应结构。只比较结构，不比较内容。
- 基准测试和压测只在 CI 上跑，跟 fakes3 一样。

## 不做的事

- 不做真正的推理，不保证回复有任何"智能"。
- 不保证 tokenizer 和任何模型完全一致，只保证结果确定、前后自洽。
- 不做多模态生成（图片、音频输出）；输入里的图片只需要能被正确解析并记进日志。
- 不做内容审核、安全策略这类模型行为；`content_filter` 只能作为故障手动注入。

## 里程碑

| 阶段 | 内容 |
|---|---|
| M0 | Chat Completions（同步 + SSE）、`/v1/models`、剧本、默认兜底、请求日志、虚拟时钟、Rust 库和二进制 |
| M1 | tool call（含并行和流式分片）、Anthropic Messages、故障注入、napi 绑定（Bun 和 Node） |
| M2 | Responses API（有状态）、Embeddings、JS SDK、napi 绑定、Docker 镜像和发布流程 |
| M3 | fake agent（轨迹、工具映射）、策略脚本（JS）、embeddings 与路由整理 |
| M4 | cassette 录制和回放（`--upstream`）、`fakellm import hf`、结构化输出、快照、热重载 |
| 收尾 | 策略 `decline`、自定义端点规则、LICENSE、CHANGELOG |

上表是实际交付，每个阶段的细节见 [CHANGELOG.md](../CHANGELOG.md)；最初的计划曾把 cassette 放在 M2、import 放在 M3。

## 还没想清楚的问题

1. 策略脚本的集成方式：已解决，作为子进程，通过 JSONL 标准输入输出协议调用，不链接 C 库。txiki.js（`tjs`）、Node、Bun 三个运行时共用协议，各有一个内置 shim；`tjs` 在自编译的 v26.6.0 上验证通过，见 [agent.md](agent.md#策略脚本)。
2. cassette 的语义匹配规则怎么设计：哪些字段默认忽略，怎么归一化 system prompt 里会变的部分（时间、随机 ID）？已解决，见 [cassette.md](cassette.md)。
3. Responses API 的服务端状态：已解决。默认保存，`store: false` 不保存，`DELETE` 和控制面可清空，快照可存取；不做过期，也就不涉及虚拟时钟。见 [control-plane.md](control-plane.md) 和 [cassette.md](cassette.md)。
4. 和 fakes3 共用的部分（控制面框架、时钟、请求日志、ctl、napi 发布流程）是拆成一个公共 crate，还是两个仓库各自维护一份？已决定：各自维护（是否改动留给作者，见 [TODO.md](../TODO.md)）。两边的控制面路由、请求记录字段和 ctl 子命令各不相同（fakellm 有协议、规则、轨迹、cassette，fakes3 有桶和对象），真正相同的只有几十行时钟和日志骨架；抽成公共 crate 要让两个仓库同步发版，成本高于复制，所以不拆，改动时人工对照。
5. HF 上的轨迹格式：已解决，每种格式一个转换器，统一输出轨迹或 cassette（ShareGPT、WildChat、Hermes、SWE-agent、OpenHands、xlam），不另设中间格式。见 [import.md](import.md)。
