# 把真实客户端接到 fakellm：`fakellm wire`

`fakellm wire <目标>` 打印让某个客户端连上正在运行的 fakellm 所需的全部配置：环境变量、配置文件、代码片段。它不连接服务，只是把 endpoint 填进模板，所以可以在服务启动前后任意时间运行。

```bash
fakellm --port 0 &                       # 第一行 stdout 就是 endpoint，例如 http://127.0.0.1:41873
export FAKELLM_ENDPOINT=http://127.0.0.1:41873     # wire 的 --endpoint 默认读它，再默认 http://127.0.0.1:4010
fakellm wire claude
```

目标：`claude`、`codex`、`opencode`、`pi`、`openai-sdk`、`ai-sdk`、`anthropic-sdk`。未知目标和格式不对的 endpoint 往 stderr 写信息，退出码 2。
endpoint 末尾的 `/` 和 `/v1` 会被去掉，所以 `http://host:port` 和 `http://host:port/v1` 等价；必须以 `http://` 或 `https://` 开头。

## 输出格式

`--format` 取 `shell`、`env`、`json`、`toml`：

| 格式 | 内容 |
|---|---|
| `shell` | `export KEY='value'` 行（值用单引号，内部的引号会转义），然后是配置文件（每个文件前有一行 `# 文件名`），最后是说明片段。有环境变量的目标默认用它，可以直接 `eval "$(fakellm wire claude)"`（注释行会被 shell 忽略） |
| `env` | 只有 `KEY=value` 行，给 `env $(fakellm wire claude --format env) cmd` 或 `--env-file` 用 |
| `json` | 一个对象 `{"env": {...}, "files": {"文件名": "内容"}, "snippet": "..."}`，给脚本用。没有环境变量的目标默认用它 |
| `toml` | 第一个 `.toml` 文件的内容，只有 `codex` 有；其他目标报错 |

默认格式：有环境变量用 `shell`，否则有 `.toml` 文件用 `toml`，否则 `json`。`codex` 有环境变量，所以默认是 `shell`；想要纯 `config.toml` 要写 `--format toml`。

下面的输出都是对真实二进制运行得到的（endpoint 为 `http://127.0.0.1:41873`）。

## claude（Claude Code）

```
$ fakellm wire claude --endpoint http://127.0.0.1:41873
export ANTHROPIC_BASE_URL='http://127.0.0.1:41873'
export ANTHROPIC_API_KEY='fakellm'

# Claude Code prefers ANTHROPIC_AUTH_TOKEN (sent as a Bearer token) when it is set.
# fakellm accepts both headers; unset it, or set it to fakellm.
# Model names such as sonnet or glm-5.2[1m] are accepted as they are.
```

不需要配置文件。注意：

- **必须有 `ANTHROPIC_API_KEY`**。用一个空的 `CLAUDE_CONFIG_DIR` 时，只靠这两个变量就能跑，不需要登录，也不需要 `settings.json`。
- 如果环境里设了 `ANTHROPIC_AUTH_TOKEN`，Claude Code 会优先用它（以 `Authorization: Bearer` 发）。fakellm 两种头都接受，所以不会失败；但配了 `--api-key` 时，token 的值必须等于它。
- 想让 Claude Code 少发请求，加 `DISABLE_TELEMETRY=1 DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`。这样跑 `claude -p` 时，fakellm 的请求日志里只有一条 `POST /v1/messages?beta=true`（`stream: true`），没有 `count_tokens`、`/v1/models`、遥测或标题生成请求。
- Claude Code 会在 `messages` 数组里放一条 `{"role": "system", ...}`（环境和工作目录信息）。真实的 Anthropic API 只接受 `user` 和 `assistant`，fakellm 为了兼容它接受 `system` 角色并按 system 消息处理；这是故意放宽，不是 bug。
- 请求里有十几个工具（Bash、Read、Edit、……）。要让它走工具循环，写一条 `tool_calls` 规则调用 `Bash`（`arguments: {command, description}`），再用 `has_tool_result: true` 的规则回最终文本；已用真实 Claude Code 验证（`e2e/claude-faults.sh`），见下面“故障下的行为”。
- `claude -p` 会等 stdin，等不到数据时打印 “no stdin data received in 3s” 才继续；重定向 `</dev/null` 可以跳过这 3 秒。

### Claude Code 在故障下的行为

`e2e/claude-faults.sh` 用真实 Claude Code 2.1.314 实测（`claude -p`，工具循环是 `Bash` 再最终文本，正常情况下 2 个 `/v1/messages` 请求）。每个故障规则前面都加了 `times: 1` 或不加，后面接工具循环规则：

| 故障规则 | 结果 | 请求数 | 说明 |
|---|---|---|---|
| `status: 429, retry_after: 1` | 退出码 0，答案正常到达 | 3 | 遵守 `retry-after`，多等约 1 秒后重试 |
| `status: 500` | 退出码 0，答案正常到达 | 3 | 自动重试 |
| `status: 529` | 退出码 0，答案正常到达 | 3 | 自动重试 |
| `status: 529, in_stream: true` | 退出码 0，答案正常到达 | 3 | 流内 `error` 事件也会重试（整个请求重发） |
| `status: 400`（不限次数） | 退出码 1，`result` 是 `API Error: 400 Invalid request.` | 6 | **400 也被重试**（1 次加 5 次重试），不是立即失败 |
| `status: 404`（不限次数） | 退出码 1，`result` 是 “There's an issue with the selected model (claude-opus-5-5). It may not exist or you may not have access to it...” | 2 | 当作模型不可用，重试 1 次 |
| `status: 401`（不限次数） | 超过 150 秒仍在重试（10 个以上请求），要靠外部超时杀掉 | 10+ | 脚本不跑这一项 |

`is_error` 为 true 时 `--output-format json` 的 `result` 是上面这些文案，退出码是 1。断言重试次数用 `ctl requests list` 或请求日志里 `path` 为 `/v1/messages` 的条数。

**`HEAD /api/hello`**：Claude Code 的二进制里有这个请求，是启动时的预连接（fire-and-forget，10 秒超时），但只在“第一方 provider、没设 `ANTHROPIC_BASE_URL`、没有代理、没关非必要流量”时才发。指向 fakellm（`ANTHROPIC_BASE_URL`）时 2.1.314 **不发**，不管有没有 `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`；`e2e/claude-faults.sh` 断言请求日志里没有它。别的客户端或桩服务如果真发了：没有规则时是 404（未知路径不进请求日志），写一条自定义端点规则即可回答并留下日志，`HEAD` 也命中：`{"match": {"path": "/api/hello"}, "reply": {"body": {}}}`。

## codex（OpenAI Codex CLI）

```
$ fakellm wire codex --endpoint http://127.0.0.1:41873 --format toml
model = "gpt-5.5"
model_provider = "fakellm"

[model_providers.fakellm]
name = "fakellm"
base_url = "http://127.0.0.1:41873/v1"
# env_key = "FAKELLM_API_KEY"   # 可选，见下
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 5000
```

把它存成 `$CODEX_HOME/config.toml`（`CODEX_HOME` 缺省是 `~/.codex`；测试时指向一个临时目录，免得动用户自己的配置）。`shell` 格式还会打印一条等价的命令行：

```bash
FAKELLM_API_KEY=fakellm codex -c model=gpt-5.5 -c model_provider=fakellm \
  -c 'model_providers.fakellm={ name = "fakellm", base_url = "http://127.0.0.1:41873/v1", env_key = "FAKELLM_API_KEY", wire_api = "responses", request_max_retries = 0, stream_max_retries = 0, stream_idle_timeout_ms = 5000 }'
```

注意：

- **codex 不读 `OPENAI_BASE_URL`**，内置 `openai` provider 的 `base_url` 也被忽略。只有自定义 provider 能改地址，这就是为什么必须用上面的配置。codex 现在只有 Responses（`wire_api = "chat"` 启动即报错），fakellm 的 `POST /v1/responses` 就是为它做的。
- **`env_key` 是可选的**，输出里把它注释掉了，没有它 codex 照常运行（已用真实 codex 验证，不发 `Authorization`）。一旦启用，它指向的环境变量必须设置且非空，否则 codex 启动即报 `Missing environment variable`；只有 fakellm 配了 `--api-key` 时才需要启用（值要一致）。下面的命令行形式自带 `FAKELLM_API_KEY=fakellm`，所以保留了 `env_key`。不需要 `codex login`，也不需要 `CODEX_API_KEY` / `OPENAI_API_KEY`。
- `request_max_retries = 0`、`stream_max_retries = 0` 关掉 codex 的重试（默认 4 和 5 次），`stream_idle_timeout_ms = 5000` 把空闲超时从 5 分钟降到 5 秒。这意味着**挂住超过 5 秒的流会被 codex 当作超时错误**；测 `hang` 时心里要有数。
- **故障必须通过 `response.failed` 事件注入**，也就是错误规则要写 `in_stream: true`。codex 对 HTTP 400 的 body 不做分类（一律是不重试的 `InvalidRequest`），所以用 HTTP 400 写不出 `context_length_exceeded`；示例见 [scripts.md](scripts.md#示例五codex-的上下文溢出)。
- codex 以 `exec` 非交互运行时的实测情况（codex-cli 0.162.1）：
  - 提示词作为参数、stdin 没关时，codex 会卡在 “Reading additional input from stdin...”，要加 `</dev/null`。
  - 要让工具不经审批地执行，用 `--dangerously-bypass-approvals-and-sandbox`（只在临时目录里用）。
  - 发过去的工具是 `exec_command`、`write_stdin`、`request_user_input`、`apply_patch`、`view_image`、`get_goal`、`create_goal`、`update_goal`，**没有 `shell`**。剧本里调用没声明的工具（比如 `shell`）会得到 codex 的 `unsupported call: shell`。`apply_patch` 是 `custom` 类型的工具，剧本里的调用会按 `custom_tool_call` 返回。
  - 第一条消息是 `developer` 角色的系统提示（“You are Codex...”），`input` 里没有 `instructions` 字段。
  - codex 会 `GET /models?client_version=...`，fakellm 的回答不会让它失败。
  - 往 stderr 写一条无法在 `/tmp` 下创建 PATH 辅助别名的警告，无害。
- **故障下的行为**（`e2e/codex-faults.sh`，codex-cli 0.162.1，除标明外都是 `stream_max_retries = 0`）：

  | 规则 | codex 的表现 | 请求数 |
  |---|---|---|
  | `in_stream` 的 `context_length_exceeded` | “Codex ran out of room in the model's context window...”，退出码 1 | 1 |
  | 同一错误用 HTTP 400 | 原样打印错误体 JSON | 1 |
  | `in_stream` 的 `insufficient_quota`（`status: 429`） | “Quota exceeded. Check your plan and billing details.”，退出码 1 | 1 |
  | `in_stream` 的 `rate_limit_exceeded` + `retry_after` + `headers` | “rate limit exceeded: Rate limit reached for requests...”，退出码 1 | 1 |
  | 同上但 `stream_max_retries = 2` 且 `times: 1` | 打印 “Reconnecting... 1/2”，重试，答案正常到达，退出码 0 | 2 |
  | `in_stream` 的 `status: 503`（`server_is_overloaded`），无 `retry_after` | “Selected model is at capacity. Please try a different model.”，不重试 | 1 |
  | 同上加 `retry_after: 1`，`stream_max_retries = 2` | 重试，答案正常到达 | 2 |
  | `finish_reason: length`（`response.incomplete`，`max_output_tokens`） | 先打印已发出的部分文本，再 “stream disconnected before completion: Incomplete response returned, reason: max_output_tokens”，退出码 1，不重试 | 1 |
  | `tool_calls` 调 `apply_patch`，`arguments` 是补丁字符串 | 作为 `custom_tool_call` 发出，codex **真的应用了补丁**（文件出现），再发 `custom_tool_call_output`，最终文本到达，退出码 0。补丁要完整（`*** Begin Patch` 到 `*** End Patch`，换行结尾） | 2 |
  | `stream.ttft_ms: 30000`（响应头先发出，之后沉默），时钟冻结 | 5 秒后 “stream disconnected before completion: idle timeout waiting for SSE”，退出码 1，不重试 | 1 |
  | 同上，2 秒后 `clock advance 30s` 放行 | codex 无感知，答案正常到达，退出码 0 | 1 |
  | `hang_ms: 30000`（响应头都不发），时钟冻结 | **codex 没有等响应头的超时**：`stream_idle_timeout_ms` 不起作用，一直等到被外部杀掉（`timeout` 的退出码 124），stderr 没有 idle timeout | 1 |
  | `stream.tokens_per_sec: 1` 且时钟冻结（流中途沉默） | 同 `ttft_ms`：5 秒后 idle timeout | 1 |

  所以“挂住超过 `stream_idle_timeout_ms`”要用 `stream.ttft_ms` 或 `tokens_per_sec` 这类“头已发出”的写法；想让 codex 卡在发请求阶段才用 `hang` / `hang_ms`，并自己给 codex 设外部超时。
- codex 走的是 HTTP SSE，不是 WebSocket，所以请求里没有 `previous_response_id`，`store` 恒为 `false`；对话历史每次都完整重发。

## opencode

```
$ fakellm wire opencode --endpoint http://127.0.0.1:41873 --format json
```

输出里 `files` 的 `opencode.json` 是：

```json
{
  "$schema": "https://opencode.ai/config.json",
  "model": "fakellm/fake-model",
  "provider": {
    "fakellm": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "fakellm",
      "options": { "baseURL": "http://127.0.0.1:41873/v1", "apiKey": "fakellm" },
      "models": { "fake-model": { "name": "fake-model", "tool_call": true } }
    }
  }
}
```

放在项目根目录或 `~/.config/opencode/opencode.json`。不想写文件，把同样的 JSON（紧凑形式）放进 `OPENCODE_CONFIG_CONTENT`（`shell` 格式的第一行 `export` 就是它）。`shell` 输出里 JSON 出现两次：紧凑形式的环境变量，和 `# opencode.json` 下的文件内容，二选一用即可。

`--file NAME` 只打印一个生成文件的原始内容，方便直接重定向：`fakellm wire pi --file models.json > ~/.pi/agent/models.json`、`fakellm wire codex --file config.toml`、`fakellm wire opencode --file opencode.json`。文件名不对时报错并列出可选项（退出码 2）；不能和 `--format` 同时用。

注意：

- opencode 走 `@ai-sdk/openai-compatible`，也就是 **Chat Completions**，不是 Responses。
- **标题生成是旁路请求**：开一个会话时，opencode 会在主请求之前约 0.3 秒先发一个生成标题的请求，system prompt 以 “You are a title generator...” 开头，**不带 tools**，`messages` 是 system、user、user，system 是 content parts 数组而不是字符串。不处理它，它会吃掉你的第一条规则。把 `no_tools: true` 加 `system: "(?i)title"` 的规则写在前面：

  ```yaml
  - name: title
    match: { no_tools: true, system: "(?i)title" }
    reply: { text: "Greeting" }
  - name: main
    match: { any: true }
    reply: { text: "FAKELLM_OK" }
  ```

- 主请求带 `tool_choice`，工具有 bash、edit、glob、grep、read、skill、task、todowrite、webfetch、write，system prompt 以 “You are opencode, an interactive CLI tool...” 开头，约 9.7 KB。
- opencode 默认会去拉模型列表（models.dev），隔离测试时设 `OPENCODE_DISABLE_MODELS_FETCH=1`，再加 `OPENCODE_DISABLE_AUTOUPDATE=1`，并把 `HOME`、`XDG_CONFIG_HOME`、`XDG_DATA_HOME`、`XDG_CACHE_HOME`、`XDG_STATE_HOME` 都指进临时目录，免得动用户自己的配置和数据。即使不加 `--pure`，opencode 也只向 fakellm 请求了 `/v1/chat/completions`。
- 非交互运行：`opencode run --model fakellm/fake-model "say hi" </dev/null`。
- **`OPENCODE_CONFIG_CONTENT`**：`e2e/opencode.sh` 的第二个场景只用 `wire opencode --format env` 打印的这个环境变量（不写文件），标题旁路和主请求都正常，输出 `FAKELLM_OK`。第一个场景用 `OPENCODE_CONFIG` 指向的文件。
- **首次运行卡死 120 秒**：曾在第一次跑 e2e 时出现（请求日志为空），没能重现。现在 e2e 脚本里有 5 次“全新 XDG 目录”的运行（每次 `HOME` 和四个 `XDG_*` 都是空目录，等于首次运行），全部在 4 到 7 秒内正常完成，没有卡死。能确认的只有：首次运行比有缓存时慢，另外**不设 `OPENCODE_DISABLE_MODELS_FETCH=1` 时，同样的全新目录运行要 12 到 24 秒**（多出来的时间是去拉 models.dev，请求日志里看不到，因为它不经过 fakellm）。所以那次 120 秒很可能是这个网络拉取在当时的网络环境下超时；这是推断，没有在受限网络里复现。隔离测试一定要设 `OPENCODE_DISABLE_MODELS_FETCH=1`。
- 另外，一份写着旧端口的 `opencode.json` 会让 opencode 报 “Cannot connect to API”；fakellm 用 `--port 0` 时每次端口都变，配置文件要每次重新生成。

## pi

```
$ fakellm wire pi --endpoint http://127.0.0.1:41873 --format json
```

`files` 里的 `models.json` 是：

```json
{
  "providers": {
    "fakellm": {
      "baseUrl": "http://127.0.0.1:41873/v1",
      "api": "openai-completions",
      "apiKey": "fakellm",
      "models": [{ "id": "fake-model" }]
    }
  }
}
```

存成 `~/.pi/agent/models.json`，然后选 `fakellm/fake-model`。

已用真实的 pi 验证（`e2e/pi.sh`，pi-mono 1.4.0 的 `packages/coding-agent`）：`wire pi` 输出的 `models.json` 原样被接受，`fakellm/fake-model` 能直接选中，只请求 `/v1/chat/completions`，不需要改任何字段。一轮纯文本得到 `FAKELLM_OK`；`bash` 工具循环（pi 执行命令，把结果发回来，再打印最终文本）也通过。

pi 没有全局安装，`e2e/pi.sh` 直接从 pi-mono 克隆里跑源码，不需要构建：

```bash
RESOLVER=$(node -p 'require("node:url").pathToFileURL(process.argv[1]).href' $PI_MONO/packages/coding-agent/src/experimental/source-resolver.ts)
PI_CODING_AGENT_DIR=/tmp/pi-agent node --import "$RESOLVER" $PI_MONO/packages/coding-agent/src/cli.ts -p --model fakellm/fake-model "say hi"
```

`PI_CODING_AGENT_DIR` 指向放 `models.json` 的目录（缺省是 `~/.pi/agent`）；`$PI_MONO` 缺省是 `$RESEARCH_DIR/pi-mono`，找不到克隆或 `node_modules` 时脚本打印 skip 并退出码 0。需要较新的 node（实测 v24，靠 TypeScript 类型剥离），首次启动较慢（几秒）。pi 没有环境变量，所以默认格式是 `json`，要取出文件内容：`fakellm wire pi --file models.json`。

## SDK 片段：openai-sdk、ai-sdk、anthropic-sdk

这三个目标只打印代码片段，没有环境变量和文件。要点是 `baseURL` 带不带 `/v1`：

| 目标 | JS | Python | baseURL |
|---|---|---|---|
| `openai-sdk` | `new OpenAI({ baseURL, apiKey })` | `OpenAI(base_url=..., api_key=...)` | `http://host:port/v1` |
| `anthropic-sdk` | `new Anthropic({ baseURL, apiKey })` | `Anthropic(base_url=..., api_key=...)` | `http://host:port`，**不带** `/v1`（SDK 自己会加） |
| `ai-sdk` | `createOpenAI({ baseURL })` 和 `createAnthropic({ baseURL })` | 无 | 两个都是 `http://host:port/v1` |

`apiKey` 随便填。Python 片段只是照 SDK 的文档写出来的，仓库里的测试只覆盖 Node 的官方 SDK（见 [TODO](../TODO.md)）。

## e2e 脚本

`e2e/` 里的脚本用真实的客户端二进制对着 fakellm 跑一遍。它们是**手工运行的冒烟测试**，不在 `cargo test` 和 CI 里（需要机器上装有对应的 CLI）：

| 脚本 | 客户端 | 验证了什么 |
|---|---|---|
| `e2e/codex.sh` | codex-cli 0.162.1 | 场景 1：纯文本，输出 `FAKELLM_OK`，请求日志有一条 `POST /v1/responses`（`openai.responses`、`stream: true`）；场景 2：`exec_command` 工具调用，codex 真的执行了命令，再收到 `TOOL_DONE`，第二个请求带着 `function_call` 和 `function_call_output` |
| `e2e/claude.sh` | Claude Code 2.1.314 | `claude -p "say hi"` 返回 `FAKELLM_OK`，`is_error: false`，一条 `POST /v1/messages?beta=true` |
| `e2e/opencode.sh` | opencode 1.18.29 | 配置文件和 `OPENCODE_CONFIG_CONTENT` 两种形式：`opencode run` 输出 `FAKELLM_OK`，两个请求：标题旁路（规则 `title`）和主请求（规则 `main`）；另外 5 次全新 XDG 目录的运行 |
| `e2e/claude-faults.sh` | Claude Code 2.1.314 | `Bash` 工具循环；429 / 500 / 529 / 流内 529 各一次后成功，400 / 404 的失败；`HEAD /api/hello` |
| `e2e/codex-faults.sh` | codex-cli 0.162.1 | `response.failed`（`context_length_exceeded`、`insufficient_quota`、`rate_limit_exceeded`）、`response.incomplete`、重试、`apply_patch`、挂起与 idle timeout，见上 |
| `e2e/pi.sh` | pi-mono 1.4.0 | `wire pi` 的 `models.json`，一轮文本和 `bash` 工具循环 |
| `e2e/connect-rst.sh` | 裸 socket + `e2e/connect-proxy.mjs` | CONNECT 代理后面的 `reset_after_chunks`，见 [scripts.md](scripts.md#故障注入每一类怎么写) |
| `e2e/claude-trajectory.sh`、`codex-trajectory.sh`、`opencode-policy.sh`、`docker.sh` | | 轨迹、策略脚本、Docker 镜像，见 [agent.md](agent.md) |

用法：

```bash
cargo build
./e2e/codex.sh            # 或 claude.sh、opencode.sh、pi.sh ...，任意工作目录都行
FAKELLM_BIN=/path/to/fakellm ./e2e/codex-faults.sh      # 除 claude.sh 外都读 FAKELLM_BIN
```

每个脚本都是：启动 `fakellm --port 0`，从第一行 stdout 读出 endpoint，用 `PUT /_fakellm/scripts` 注册规则，在 `/tmp/fakellm-e2e-<名字>` 里隔离配置目录（不碰用户的 `~/.codex`、`~/.claude`、opencode 的配置），带 120 秒超时运行客户端，检查输出，把请求日志存成 `requests.json`，最后用 `trap` 杀掉服务。通过打印 `PASS`（codex 脚本打印 `codex e2e ok`），失败时退出码非 0。`claude.sh` 先 `cargo build`，但同样读 `FAKELLM_BIN`。

`*-faults.sh` 做故障注入：每个场景断言退出码、输出里的关键文案和请求数（重试次数），实测结果就是上面各节的表；客户端升级后行为变了脚本会失败。

## 与真实 API 有意保留的差异

拿 ai-sdk 自带的真实抓包夹具对照（见 [consumers.md](consumers.md)）得到的响应保真差异，下面几项已经补齐（`tests/anthropic.rs` 的 `response_fidelity_fields`、`src/openai/responses_stream.rs` 的单元测试）：

- Anthropic `usage` 带 `cache_creation`（`ephemeral_5m_input_tokens` 等于缓存写入数，`ephemeral_1h_input_tokens` 恒为 0）、`service_tier: "standard"`、`inference_geo: "not_available"`，同步响应和 `message_start` 都有。
- `message_delta` 带 `context_management`：请求里有 `context_management` 时是 `{"applied_edits": []}`，否则是 `null`。
- 没有参数的工具调用（`arguments` 为 `{}`）在 Anthropic 流里只发一个空的 `input_json_delta`，不再多发 `{}`。
- Responses 的 `reasoning` 条目在 `output_item.added` 和 `output_item.done` 上的 `encrypted_content` 不同（`added` 上是生成值的前半段）；规则自己写了 `reasoning_opaque` 时两处保持一致。

下面几项是**有意不做**的，原因如下：

| 差异 | 不做的原因 |
|---|---|
| `model` 回显请求里的别名，而不是带日期的 id（`claude-sonnet-4-5-20250929`） | fakellm 没有模型目录的真相来源；回显请求值才能让客户端的“请求什么就得到什么”假设成立，要带日期的 id 可以直接把它当作请求的 `model` 或在规则里写 |
| `message_start` 的 `output_tokens` 是 1（夹具里是 10 和 2） | 真实值是上游当时的瞬时估计，没有可复现的来源；固定为 1 让流的前缀可预测，`message_delta` 里的最终值是准确的 |
| 每个 delta 一个分词 token，块数多于真实 API；Responses 工具参数分片也不同 | 真实 API 的分块由服务端批处理决定，不可复现；`stream.chunk_tokens` 和 `args_chunk_bytes` 可以调粗，需要对齐某个夹具时按它设置 |
| Responses 的 WebSocket 传输、`background`、`/cancel`、`/input_items` | 见 [control-plane.md](control-plane.md) 的“不做的 Responses 能力” |
