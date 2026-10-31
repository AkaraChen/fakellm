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
- 请求里有十几个工具（Bash、Read、Edit、……）。要让它走工具循环，用 `has_tool: Bash` 之类的条件写 `tool_calls` 规则；这一步没有被 e2e 覆盖，见下。
- `claude -p` 会等 stdin，等不到数据时打印 “no stdin data received in 3s” 才继续；重定向 `</dev/null` 可以跳过这 3 秒。

## codex（OpenAI Codex CLI）

```
$ fakellm wire codex --endpoint http://127.0.0.1:41873 --format toml
model = "gpt-5.5"
model_provider = "fakellm"

[model_providers.fakellm]
name = "fakellm"
base_url = "http://127.0.0.1:41873/v1"
env_key = "FAKELLM_API_KEY"
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
- **`env_key` 指向的环境变量必须设置且非空**，否则启动失败；值随便填（除非 fakellm 配了 `--api-key`，那就得一致）。不需要 `codex login`，也不需要 `CODEX_API_KEY` / `OPENAI_API_KEY`。
- `request_max_retries = 0`、`stream_max_retries = 0` 关掉 codex 的重试（默认 4 和 5 次），`stream_idle_timeout_ms = 5000` 把空闲超时从 5 分钟降到 5 秒。这意味着**挂住超过 5 秒的流会被 codex 当作超时错误**；测 `hang` 时心里要有数。
- **故障必须通过 `response.failed` 事件注入**，也就是错误规则要写 `in_stream: true`。codex 对 HTTP 400 的 body 不做分类（一律是不重试的 `InvalidRequest`），所以用 HTTP 400 写不出 `context_length_exceeded`；示例见 [scripts.md](scripts.md#示例五codex-的上下文溢出)。
- codex 以 `exec` 非交互运行时的实测情况（codex-cli 0.162.1）：
  - 提示词作为参数、stdin 没关时，codex 会卡在 “Reading additional input from stdin...”，要加 `</dev/null`。
  - 要让工具不经审批地执行，用 `--dangerously-bypass-approvals-and-sandbox`（只在临时目录里用）。
  - 发过去的工具是 `exec_command`、`write_stdin`、`request_user_input`、`apply_patch`、`view_image`、`get_goal`、`create_goal`、`update_goal`，**没有 `shell`**。剧本里调用没声明的工具（比如 `shell`）会得到 codex 的 `unsupported call: shell`。`apply_patch` 是 `custom` 类型的工具，剧本里的调用会按 `custom_tool_call` 返回。
  - 第一条消息是 `developer` 角色的系统提示（“You are Codex...”），`input` 里没有 `instructions` 字段。
  - codex 会 `GET /models?client_version=...`，fakellm 的回答不会让它失败。
  - 往 stderr 写一条无法在 `/tmp` 下创建 PATH 辅助别名的警告，无害。
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

放在项目根目录或 `~/.config/opencode/opencode.json`。不想写文件，把同样的 JSON（紧凑形式）放进 `OPENCODE_CONFIG_CONTENT`（`shell` 格式的第一行 `export` 就是它）。`wire opencode` 的 `shell` 输出会把 JSON 打印两遍（一次是文件内容，一次是说明片段），这是已知的小毛病，不影响使用。

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
- **已知问题**：e2e 脚本第一次运行时卡到了 120 秒超时，请求日志为空，之后（包括 `rm -rf` 清空后的重跑）都在约 3 秒内完成，没能复现，原因未查明，可能是首次运行的一次性初始化。e2e 脚本里的配置是通过 `OPENCODE_CONFIG` 指向的文件传入的，`OPENCODE_CONFIG_CONTENT` 这条路径没有被 e2e 覆盖。
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

**这一条没有用真实的 pi 验证过**：输出的格式是按 `docs/m1-spec.md` 的约定生成、由 `tests/wire.rs` 校验的，没有对照真实的 pi 跑过，也没有 `e2e/pi.sh`；`models.json` 的字段名和存放位置请以 pi 自己的文档为准。pi 没有环境变量，所以默认格式是 `json`，要取出文件内容：`fakellm wire pi --format json | jq -r '.files["models.json"]'`。

## SDK 片段：openai-sdk、ai-sdk、anthropic-sdk

这三个目标只打印代码片段，没有环境变量和文件。要点是 `baseURL` 带不带 `/v1`：

| 目标 | JS | Python | baseURL |
|---|---|---|---|
| `openai-sdk` | `new OpenAI({ baseURL, apiKey })` | `OpenAI(base_url=..., api_key=...)` | `http://host:port/v1` |
| `anthropic-sdk` | `new Anthropic({ baseURL, apiKey })` | `Anthropic(base_url=..., api_key=...)` | `http://host:port`，**不带** `/v1`（SDK 自己会加） |
| `ai-sdk` | `createOpenAI({ baseURL })` 和 `createAnthropic({ baseURL })` | 无 | 两个都是 `http://host:port/v1` |

`apiKey` 随便填。Python 片段只是照 SDK 的文档写出来的，仓库里的测试只覆盖 Node 的官方 SDK（见 [TODO](../TODO.md)）。

## e2e 脚本

`e2e/` 里有三个脚本，用真实的客户端二进制对着 fakellm 跑一遍。它们是**手工运行的冒烟测试**，不在 `cargo test` 和 CI 里（需要机器上装有对应的 CLI）：

| 脚本 | 客户端 | 验证了什么 |
|---|---|---|
| `e2e/codex.sh` | codex-cli 0.162.1 | 场景 1：纯文本，输出 `FAKELLM_OK`，请求日志有一条 `POST /v1/responses`（`openai.responses`、`stream: true`）；场景 2：`exec_command` 工具调用，codex 真的执行了命令，再收到 `TOOL_DONE`，第二个请求带着 `function_call` 和 `function_call_output` |
| `e2e/claude.sh` | Claude Code 2.1.314 | `claude -p "say hi"` 返回 `FAKELLM_OK`，`is_error: false`，一条 `POST /v1/messages?beta=true` |
| `e2e/opencode.sh` | opencode 1.18.29 | `opencode run` 输出 `FAKELLM_OK`，两个请求：标题旁路（规则 `title`）和主请求（规则 `main`） |

用法：

```bash
cargo build
./e2e/codex.sh            # 或 claude.sh、opencode.sh
FAKELLM_BIN=/path/to/fakellm ./e2e/codex.sh      # codex.sh 和 opencode.sh 可以换二进制
```

每个脚本都是：启动 `fakellm --port 0`，从第一行 stdout 读出 endpoint，用 `PUT /_fakellm/scripts` 注册规则，在 `/tmp/fakellm-e2e-<名字>` 里隔离配置目录（不碰用户的 `~/.codex`、`~/.claude`、opencode 的配置），带 120 秒超时运行客户端，检查输出，把请求日志存成 `requests.json`，最后用 `trap` 杀掉服务。通过打印 `PASS`（codex 脚本打印 `codex e2e ok`），失败时退出码非 0。`claude.sh` 总是用 `target/debug/fakellm`，不读 `FAKELLM_BIN`。

这些脚本验证的是“真实客户端能完成一轮对话”，不是故障注入：还没有用真实 harness 测过 429 重试、`response.failed`、断流、挂起。
