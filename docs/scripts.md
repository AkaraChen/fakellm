# 剧本规则格式

剧本是 fakellm 决定“回什么”的方式：一个有序的规则列表，每条规则是 `match`（什么样的请求）加 `reply`（怎么回答）。
没有规则命中的请求走[默认兜底](../README.md#默认兜底与---strict)，或者在 `--strict` 下得到 400。

注册方式：

- `fakellm --scripts FILE`：启动时加载，文件不合法则启动失败。
- `fakellm ctl scripts put|add FILE`、`PUT|POST /_fakellm/scripts`：运行时替换或追加（见[控制面](control-plane.md#剧本)）。
- Rust：`Config::scripts`、`server.scripts().replace(...)`。

文档是 JSON 或 YAML：一个规则数组，或者 `{"rules": [...]}`。所有对象都拒绝未知字段（拼错字段名是 400，而不是被默默忽略）。

## 匹配顺序与 `times`

1. 请求按注册顺序从上到下和每条规则比较。
2. 规则被跳过的情况：`times` 已经用完，或者 `match` 里任何一个写出的条件不满足。
3. 第一条没被跳过的规则回答这个请求，它的 `hits` 加 1。
4. 没有规则命中：兜底回复，或 strict 错误。

`times: N` 表示这条规则最多回答 N 个请求，之后（`exhausted: true`）被跳过，请求落到后面的规则。`times` 不写则不限。
`hits` 在规则注册（`PUT`）时从 0 开始，只有真正被选中的规则才计数；被前面的规则抢走的请求不计入后面规则。
`times: 0` 合法，等于永远不会命中。

匹配和消耗命中计数是在同一个锁里完成的，并发请求不会让一个 `times: 1` 的规则回答两次。

## Rule

| 字段 | 类型 | 说明 |
|---|---|---|
| `name` | 字符串 | 可选。出现在请求日志的 `rule` 字段，可用 `?rule=` 过滤 |
| `match` | 对象 | 缺省或 `{}` 匹配所有请求 |
| `reply` | 对象 | 必填 |
| `times` | 整数 | 缺省不限 |

## Match

所有写出的字段都必须满足（与）；没写的字段不参与判断。

| 字段 | 类型 | 语义 |
|---|---|---|
| `model` | glob | 对请求的 `model` 做 glob，`*` 任意串，`?` 一个字符，区分大小写。`gpt-*`、`*-mini` |
| `last_user` | 正则 | 匹配**最近一条** user 消息的文本（后面即使跟着 assistant / tool 消息也算）。没有 user 消息则不匹配 |
| `system` | 正则 | 匹配所有 system 和 developer 消息拼起来的文本（用换行连接） |
| `any_message` | 正则 | 任何一条消息（任何角色）的文本匹配即可 |
| `has_tool` | 字符串 | `tools` 里提供了这个名字的工具 |
| `no_tools` | 布尔 | `true`：没有提供任何工具；`false`：至少提供了一个 |
| `has_tool_result` | 布尔 | `true`：对话**以 tool 消息结尾**（客户端刚执行完工具）；`false`：不是 |
| `last_tool_result` | 字符串 | 结尾那一串连续的 tool 消息里，有一条是这个函数的结果 |
| `tool_result_contains` | 正则 | 结尾的 tool 消息中，有一条的文本匹配 |
| `stream` | 布尔 | `true` 只匹配流式请求，`false` 只匹配非流式 |
| `turn` | 整数 | 请求里已有多少条 assistant 消息；第一次请求是 `0` |
| `any` | 布尔 | 显式的“全部匹配”，只为可读，`{}` 等价 |

细节：

- **文本**：消息里 `text` 类型的 part 用换行连在一起；图片、音频等 part 不参与。字符串 content 就是它自己。
- **正则**：Rust `regex` 语法，不是 PCRE（没有环视和反向引用）。写法有三种：`"pattern"`（子串搜索，不要求整串匹配，要整串自己写 `^...$`）、`"/pattern/"`、`"/pattern/i"`（忽略大小写）。YAML 里含 `:`、`#`、`{` 的要加引号。不合法的正则在注册时报错并指出规则序号和字段。
- **`last_tool_result` 里的函数名**：通过 tool 消息的 `tool_call_id` 找到前面 assistant 消息里同 id 的调用，取它的函数名；找不到时才退而用 tool 消息自己的 `name`。所以客户端必须把 assistant 的 `tool_calls` 带回来（所有 SDK 都会）。名字是精确匹配。
- **`has_tool_result: false`** 不仅匹配“第一次请求”，还匹配所有不以 tool 消息结尾的请求（例如用户追问）。想只匹配第一次请求，用 `turn: 0`。
- **`turn`** 数的是整个对话里的 assistant 消息数，不是工具调用次数。

示例（每个都是独立的规则）：

```yaml
- match: { model: "gpt-4*" }                      # 模型名
  reply: { text: "gpt-4 family" }
- match: { last_user: "/^hello\\b/i" }            # 忽略大小写
  reply: { text: "Hi." }
- match: { system: "You are a code reviewer" }    # system prompt 子串
  reply: { text: "LGTM" }
- match: { any_message: "TODO\\(alice\\)" }
  reply: { text: "found alice's todo" }
- match: { has_tool: web_search, no_tools: false }
  reply: { tool_calls: [{ name: web_search, arguments: { q: "fakellm" } }] }
- match: { has_tool_result: true, tool_result_contains: "ENOENT" }   # 工具报错了
  reply: { text: "the file does not exist" }
- match: { stream: false, turn: 0 }
  reply: { text: "first sync request only" }
```

## Reply

`text`、`tool_calls`、`error` 至少要有一个；`error` 不能和 `text` / `tool_calls` 同时出现；`text` 和 `tool_calls` 可以同时出现（先发文本，再发工具调用）。空的 `tool_calls: []` 不算。违反的规则注册时 400。

| 字段 | 类型 | 说明 |
|---|---|---|
| `text` | 字符串 | 回复文本。受请求的 `max_tokens` / `max_completion_tokens` 约束：超出就按 tokenizer 的 token 边界截断，`finish_reason` 变成 `length` |
| `tool_calls` | 数组 | 见下 |
| `reasoning` | 字符串 | 推理文本，作为 `reasoning_content` 发送（流式在正文之前）。不受 `max_tokens` 约束，计入 `reasoning_tokens` |
| `error` | 对象 | 见下 |
| `finish_reason` | `stop` / `length` / `tool_calls` / `content_filter` | 覆盖自动推断的结束原因。默认：被 `max_tokens` 截断为 `length`，有工具调用为 `tool_calls`，否则 `stop` |
| `stream` | 对象 | 流式节奏，见下 |
| `delay_ms` | 整数 | 同步响应或错误响应发出前的等待，毫秒，服务端时钟。流式请求里等同 `stream.ttft_ms`（`stream.ttft_ms` 优先） |

`usage` 总是由服务端用内置 tokenizer 算出，不能在规则里写。`prompt_tokens` 来自整个请求，`completion_tokens` 是文本、工具调用参数和推理文本的 token 之和。

### ToolCall

| 字段 | 类型 | 说明 |
|---|---|---|
| `name` | 字符串 | 必填 |
| `arguments` | 对象或字符串 | 对象会被紧凑序列化成 JSON 字符串；字符串原样发送，**可以故意写成不合法或不完整的 JSON**（`"{\"a\":"`）来测客户端的容错；缺省是 `{}` |
| `id` | 字符串 | 缺省时由请求内容生成 `call_...`，同一请求同一位置的 id 相同 |

流式时：第一个分片是 `{index, id, type: "function", function: {name, arguments: ""}}`，之后 `arguments` 每 16 字节一个分片（`{index, function: {arguments: "..."}}`）。多个工具调用的 `index` 从 0 递增。

### Error

| 字段 | 类型 | 说明 |
|---|---|---|
| `status` | 整数 | 必填，100 到 599 |
| `type` | 字符串 | 覆盖错误体的 `type` |
| `code` | 字符串 | 覆盖 `code` |
| `message` | 字符串 | 覆盖 `message` |
| `param` | 字符串 | 覆盖 `param` |
| `retry_after` | 数字 | 秒，可以带小数；写入 `retry-after` 头，向上取整成整数秒（`0.5` 写成 `1`） |

只写 `status` 时，`type` / `code` / `message` 取真实 API 对这个状态码的默认值（状态码表见 [README](../README.md#错误与状态码)）。错误响应是 `{"error": {"message", "type", "param", "code"}}`，四个键总在。
错误在 `delay_ms` 之后才发出。流式请求遇到错误规则也是一个普通的 HTTP 错误响应（不是 SSE），所以客户端能像真实 API 那样在流开始前重试。

目前 `status: 413` 的响应状态码实际是 400（`type` 和 `code` 是对的），见 [TODO](../TODO.md)。

### Stream

`chunk_tokens` 和 `cut_after_chunks` 只对流式请求有意义。`ttft_ms` 和 `tokens_per_sec` 同步请求也读：整个响应在 `ttft + 间隔 × (token 数 - 1)` 之后一次返回，也走服务端时钟。没写的字段用命令行的默认值（`--ttft`、`--tokens-per-sec`）。

| 字段 | 类型 | 说明 |
|---|---|---|
| `ttft_ms` | 整数 | 首 token 延迟：从请求开始到发出第一个 chunk（role chunk），毫秒 |
| `tokens_per_sec` | 数字 | 流式速度，必须大于 0。chunk 之间等待 `chunk_tokens / tokens_per_sec` 秒；不写则 chunk 之间不等待 |
| `chunk_tokens` | 整数 | 每个 content chunk 含几个 token，默认 1（真实 API 的行为）；0 按 1 处理 |
| `cut_after_chunks` | 整数 | 发完这么多个 content chunk 后结束 body，**不发 finish chunk 和 `[DONE]`**，用来测客户端的流中断处理。结束是干净的 chunked 结尾，不是 TCP 重置 |

所有时间都读服务端时钟：冻结时钟时流停在对应位置，`advance` 后放行。详见[控制面的时钟一节](control-plane.md#时钟)。

```yaml
- reply:
    text: "one two three"
    stream: { ttft_ms: 500, tokens_per_sec: 10 }   # 500ms 后第一个 token，之后每 100ms 一个
- match: { last_user: "cut" }
  reply:
    text: "aaa bbb ccc ddd"
    stream: { cut_after_chunks: 2 }                # 收到两个 content chunk 后流消失
- match: { last_user: "slow" }
  reply: { text: "ok", delay_ms: 2000 }            # 同步响应 2 秒后才返回
```

## 示例一：最简单的文本

```yaml
- name: ping
  match: { last_user: "ping" }
  reply: { text: "pong" }
```

```bash
fakellm ctl scripts put ping.yaml
curl http://127.0.0.1:4010/v1/chat/completions -d '{"model":"gpt-4o","messages":[{"role":"user","content":"ping"}]}'
```

响应的 `choices[0].message.content` 是 `"pong"`，`finish_reason` 是 `stop`。流式请求在 role chunk 之后发一个 content chunk `pong`。

## 示例二：先 429 一次，再成功

```yaml
- name: rate-limited-once
  match: { last_user: "^flaky" }
  times: 1
  reply:
    error: { status: 429, retry_after: 2 }
- name: then-ok
  match: { last_user: "^flaky" }
  reply: { text: "ok now" }
```

第一个请求得到：

```
HTTP/1.1 429 Too Many Requests
retry-after: 2

{"error":{"message":"Rate limit reached for requests. Please try again later.","type":"requests","param":null,"code":"rate_limit_exceeded"}}
```

第二个请求起得到 `ok now`。`GET /_fakellm/scripts` 里能看到 `rate-limited-once` 是 `"hits":1,"exhausted":true`。
SDK 自己会重试 429（openai SDK、ai-sdk 默认重试 2 次，等待的是客户端的真实定时器，见[虚拟时钟的边界](../README.md#虚拟时钟与流式节奏)）；断言“重试发生过”就查请求日志：`fakellm ctl requests list --path /v1/chat/completions --count` 是 2。
要测“一直失败”，去掉 `times`。要测“限流的是一部分模型”，加 `model: "gpt-4*"`。

## 示例三：agent 循环，加上标题旁路请求

许多 agent 在主流程之外还会发一个“生成会话标题”的请求：带着一个固定的 system prompt，没有 `tools`。如果不单独处理，它会吃掉主流程的第一条规则。把旁路规则写在前面，并用 `system` 和 `no_tools` 把它框住：

```yaml
# 旁路：生成标题。没有工具，system prompt 里有这句话
- name: title
  match: { system: "/generate a concise.*title/i", no_tools: true }
  reply: { text: "Fix flaky test" }

# 主流程第一步：客户端提供了 read_file，还没有工具结果 -> 调用它
- name: read-file
  match: { has_tool: read_file, has_tool_result: false }
  reply:
    tool_calls:
      - name: read_file
        arguments: { path: src/main.rs }

# 第二步：read_file 的结果回来了 -> 给出最终回答
- name: summarize
  match: { last_tool_result: read_file }
  reply: { text: "main.rs 只有一个 main 函数。" }
```

请求序列和各自命中的规则：

| 请求 | 命中 |
|---|---|
| system = “Generate a concise, sentence-case title”，无 tools | `title`（`no_tools` 满足） |
| user: “read main”，tools = [read_file]，不以 tool 结尾 | `read-file`，返回 `finish_reason: "tool_calls"` |
| …… assistant 的 read_file 调用 + tool 结果 | `read-file` 因 `has_tool_result: false` 不满足被跳过，`summarize` 命中，`finish_reason: "stop"` |
| 第三次追问，不再有 tool 结尾 | 若仍带 tools 则又命中 `read-file`；要避免就加 `times: 1`，之后落到兜底（兜底从不主动调用工具，循环自然结束） |

用 `times: 1` 保护主流程的第一条规则，是防止循环无限重复的保险：

```yaml
- name: read-file
  match: { has_tool: read_file, has_tool_result: false }
  times: 1
  reply: { tool_calls: [{ name: read_file, arguments: { path: src/main.rs } }] }
```

事后断言整个过程：

```bash
fakellm ctl requests list --rule read-file --count     # 1
fakellm ctl requests list --rule title --count         # 1
fakellm ctl requests list --source fallback --count    # 0，没有请求落到兜底
```

用 Vercel AI SDK 跑这个两步流程的完整测试在 `sdk/node/ai-sdk.test.mjs`（“tool loop over two steps”）。
