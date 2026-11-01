# 剧本规则格式

剧本是 fakellm 决定“回什么”的方式：一个有序的规则列表，每条规则是 `match`（什么样的请求）加 `reply`（怎么回答）。
同一套规则服务三种协议（Chat Completions、Responses、Anthropic Messages），请求先被解析成同一个内部结构再匹配，所以 `last_user`、`has_tool`、`has_tool_result` 这些条件在三种协议上含义相同；想只作用于某一种用 `protocol`。
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
| `last_user` | 正则 | 匹配**最近一条** user 消息的文本（后面即使跟着 assistant / tool 消息也算）；文本先去掉 `<system-reminder>` 块，去完是空的消息跳过。没有这样的 user 消息则不匹配 |
| `last_user_raw` | 正则 | 同上，但匹配最近一条 user 消息的原文，不做任何处理（Claude Code 的第一条 user 消息是 reminder 块时用它） |
| `system` | 正则 | 匹配所有 system 和 developer 消息拼起来的文本（用换行连接） |
| `any_message` | 正则 | 任何一条消息（任何角色）的文本匹配即可 |
| `body` | 正则 | 匹配请求的原始 JSON 正文（客户端发来的文本，字段顺序和空白都原样保留）。适合按非标准字段匹配，比如 `"damaged"\s*:\s*true`。任何规则都能用 |
| `has_tool` | 字符串 | `tools` 里提供了这个名字的工具 |
| `no_tools` | 布尔 | `true`：没有提供任何工具；`false`：至少提供了一个 |
| `has_tool_result` | 布尔 | `true`：最后一条 assistant 消息之后有 tool 结果（客户端刚执行完工具）；`false`：没有 |
| `last_tool_result` | 字符串 | 那一串 tool 结果里，有一条是这个函数的结果 |
| `tool_result_contains` | 正则 | 那一串 tool 结果中，有一条的文本匹配 |
| `last_tool_call_id` | 字符串 | 那一串 tool 结果里，有一条回答的是这个 id 的工具调用（`tool_call_id` / `call_id` / `tool_use_id`） |
| `stream` | 布尔 | `true` 只匹配流式请求，`false` 只匹配非流式 |
| `turn` | 整数 | 请求里已有多少条 assistant 消息；第一次请求是 `0` |
| `any` | 布尔 | 显式的“全部匹配”，只为可读，`{}` 等价 |
| `protocol` | 字符串 | 只匹配某个协议：`openai.chat`、`openai.responses`、`openai.embeddings`、`anthropic.messages`、`custom`（自定义端点，需要和 `path` / `reply.body` 一起写才有意义）。写别的值是 400 |
| `probability` | 数字 | 0 到 1。其他条件都满足后，以这个概率命中。不在范围内（包括 NaN）是 400 |
| `every_nth` | 整数 | 至少为 1。其他条件都满足的请求里，每第 N 个命中：`every_nth: 3` 命中第 3、6、9 个 |
| `after` | 规则名 | 序列匹配：名字为此值的规则至少回答过 `after_count` 次（缺省 1）之后，这条规则才可能命中。次数数的是整个服务里该规则的 `hits`，`reset` 或替换剧本后清零。整组规则里必须有这个名字，否则是 400（`names no rule`）；写自己的名字、几条规则互相引用成环（A after B、B after A，谁也无法开始）、`after_count` 没有 `after` 或为 0 也都是 400。`keep_hits=true` 替换时，计数按规则名沿用（没有名字的规则按同一位置沿用），所以调整顺序或插入规则不会让计数错位 |
| `after_count` | 整数 | 至少为 1，配合 `after`：被引用规则的最少命中次数 |
| `path` | glob | 自定义端点规则，见[自定义端点](#自定义端点)：匹配请求路径（去掉网关前缀之后），`*` 任意串、`?` 一个字符。必须和 `reply.body` 一起写 |

细节：

- **文本**：消息里 `text` 类型的 part 用换行连在一起；图片、音频等 part 不参与。字符串 content 就是它自己。
- **序列匹配（`after`）**：不靠规则顺序和 `times: 1` 区分“先到的”和“后到的”请求。例如 paseo 里 guardian 请求先于主请求：`[{name: guardian, match: {last_user: "guard"}, reply: {text: "ok"}, times: 1}, {name: main, match: {after: guardian, last_user: "work"}, reply: {text: "done"}}]`，guardian 没答过之前 `main` 不会命中，调换 codex 发请求的顺序也不会错位。门槛只看被引用规则的命中数，所以被引用的规则最好有 `name` 且不是 `times: 0` 之类永远不会命中的规则。
- **正则**：先用 Rust `regex`（快）；写了它不支持的语法（环视 `(?=…)` `(?!…)` `(?<=…)` `(?<!…)`、反向引用 `\1`）时自动改用 `fancy-regex`，所以 `"foo(?!bar)"` 可以直接写。回溯超过上限的匹配按不匹配处理。写法有三种：`"pattern"`（子串搜索，不要求整串匹配，要整串自己写 `^...$`）、`"/pattern/"`、`"/pattern/i"`（忽略大小写）。YAML 里含 `:`、`#`、`{` 的要加引号。不合法的正则在注册时报错并指出规则序号和字段。
- **`last_tool_result` 里的函数名**：通过 tool 消息的 `tool_call_id` 找到前面 assistant 消息里同 id 的调用，取它的函数名；找不到时才退而用 tool 消息自己的 `name`。所以客户端必须把 assistant 的 `tool_calls` 带回来（所有 SDK 都会）。名字是精确匹配。
- **tool 结果的判定**：看“最后一条 assistant 消息之后”的所有 tool 结果，不要求它们是整个对话的最后几条。Claude Code 会把 `tool_result` 块和一段附加文本放进同一个 user 消息，fakellm 把它拆成 tool 消息加 user 消息，`has_tool_result` / `last_tool_result` 仍然命中。Responses 里 `function_call_output` 和 `custom_tool_call_output` 也算，Anthropic 里 `tool_result` 块算，`is_error: true` 的也算（想区分就用 `tool_result_contains` 匹配错误文本）。
- **`has_tool_result: false`** 不仅匹配“第一次请求”，还匹配所有不以 tool 消息结尾的请求（例如用户追问）。想只匹配第一次请求，用 `turn: 0`。
- **`turn`** 数的是整个对话里的 assistant 消息数，不是工具调用次数。Responses 里连续的 `reasoning`、assistant `message` 和 `function_call` 项合并算一条 assistant 消息（一次模型输出）；带 `previous_response_id` 时，前面已存的对话也算进去。
- **`probability` 和 `every_nth` 的计数**：它们数的是“到达过这条规则的请求”，也就是其他所有条件都满足、规则也没用完 `times` 的请求（`GET /_fakellm/scripts` 里每条规则的 `reached`），**不是 `hits`**。`probability` 的随机数由请求内容和 `reached` 决定，所以同一个测试每次运行的结果都一样；`reached` 不同，同样内容的重试也会抽到不同的数（用 `hits` 做计数的话，一次没抽中，`hits` 不变，重试会永远抽到同一个数）。同时写 `probability` 和 `every_nth` 时两者都要满足，`reached` 每个请求只加一次。`times` 只数真正命中的请求。
- **`protocol` 的用法**：同一个提示词在三种协议下需要不同的错误体或不同的行为时，写多条规则、各带一个 `protocol`。Anthropic 的 system 提示在 `system` 字段，Chat 在 `system` / `developer` 消息，Responses 在 `instructions` 或 `developer` 消息，它们都会被 `system` 条件看到。

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

`text`、`tool_calls`、`error`、`hang` / `hang_ms`、`trajectory`、`policy` 至少要有一个；`error` 不能和 `text` / `tool_calls` 同时出现；`text` 和 `tool_calls` 可以同时出现（先发文本，再发工具调用）。空的 `tool_calls: []` 不算。违反的规则注册时 400。只写 `hang: true` 的规则是合法的。

| 字段 | 类型 | 说明 |
|---|---|---|
| `text` | 字符串 | 回复文本。受请求的 `max_tokens` / `max_completion_tokens` 约束：超出就按 tokenizer 的 token 边界截断，`finish_reason` 变成 `length` |
| `tool_calls` | 数组 | 见下 |
| `reasoning` | 字符串 | 推理文本（流式在正文之前）。Chat 里是 `reasoning_content`；Anthropic 里是 `thinking` 块；Responses 里是 `reasoning` 项的 `summary_text`。不受 `max_tokens` 约束，计入 `reasoning_tokens` |
| `reasoning_opaque` | 字符串 | 跟 `reasoning` 一起发的不透明串：Anthropic `thinking` 块的 `signature`，Responses `reasoning` 项的 `encrypted_content`（请求的 `include` 里有 `reasoning.encrypted_content` 时才发）。不写则由请求派生一个确定的值。Chat 里没有对应物，被忽略 |
| `hang` | 布尔 | `true`：收到请求后不回答，一直挂到客户端放弃。见[挂起](#挂起) |
| `hang_ms` | 整数 | 挂这么久（服务端时钟，毫秒）再按这条规则的其余部分回答。隐含 `hang`。不能和 `delay_ms` / `stream.ttft_ms` 同时写：前者连响应头都不发，后者先发响应头再延迟首字节，同时写注册时 400 |
| `usage` | 对象 | 覆盖 tokenizer 的计数，只覆盖写了的字段，其余照常计算。见下 |
| `error` | 对象 | 见下 |
| `finish_reason` | `stop` / `length` / `tool_calls` / `content_filter` | 覆盖自动推断的结束原因。默认：被 `max_tokens` 截断为 `length`，有工具调用为 `tool_calls`，否则 `stop` |
| `stream` | 对象 | 流式节奏，见下 |
| `delay_ms` | 整数 | 同步响应或错误响应发出前的等待，毫秒，服务端时钟。流式请求里等同 `stream.ttft_ms`（`stream.ttft_ms` 优先） |
| `trajectory` | 字符串 | 把请求交给这条已注册的[轨迹](agent.md#轨迹)，由它按请求里的历史选一步来回答。不能和 `text`、`tool_calls`、`reasoning`、`finish_reason`、`error`、`policy` 同时写；`stream`、`delay_ms`、`hang*`、`reset`、`usage` 是步骤的默认值。名字在请求到来时才查，找不到是 400 `fakellm_trajectory_not_found` |
| `body` | 任意 JSON | 仅自定义端点规则：原样返回的 JSON 正文。见[自定义端点](#自定义端点) |
| `status` | 整数 | 仅自定义端点规则：HTTP 状态码，默认 200，范围 100 到 599 |
| `headers` | 对象 | 仅自定义端点规则：额外的响应头（可以覆盖 `content-type`） |
| `policy` | 字符串 | 把请求交给这个已注册的[策略脚本](agent.md#策略脚本)，由 JS 模块决定回复。同样不能和内容、`error`、`trajectory` 同时写，规则上的 `stream`、`delay_ms`、`hang*`、`reset`、`usage` 是策略回复的默认值。找不到是 400 `fakellm_policy_not_found`。策略可以“放弃”：`decide` 返回 `null` / `undefined` / `{decline: true}` 时这条规则视为没匹配上（不计命中，策略的 `hits` 也不加），引擎继续往下找后面的规则，直到有规则回答或落到 cassette / 回退 |

没有被覆盖的 `usage` 字段由服务端用内置 tokenizer 算出：`prompt_tokens` 来自整个请求，`completion_tokens` 是文本、工具调用参数和推理文本的 token 之和。

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
| `in_stream` | 布尔 | 流式请求时，不返回 HTTP 错误，而是返回 200 的事件流，错误作为流里的事件发出。非流式请求忽略它（仍是 HTTP 错误）。见[协议里的错误形态](#协议里的错误形态) |
| `headers` | 对象 | 额外的响应头，值是字符串，如 `retry-after-ms: "250"`、`x-should-retry: "false"`。头名会转成小写，名字或值不合法的头被丢弃。流内错误（`in_stream`）时它们设在 200 的 SSE 响应头上（三个协议一致），Responses 的 `response.failed` 另外把它们放进 `response.error.headers` |

只写 `status` 时，`type` / `code` / `message` 取真实 API 对这个状态码的默认值，**默认值随请求的协议而变**（状态码表见 [README](../README.md#错误与状态码)）：Chat 和 Responses 用 OpenAI 的格式 `{"error": {"message", "type", "param", "code"}}`（四个键总在）；Anthropic 用 `{"type": "error", "error": {"type", "message"}}`，`code` 和 `param` 在这个协议里没有位置，会被丢掉。
错误在 `delay_ms` 之后才发出。默认情况下流式请求遇到错误规则也是一个普通的 HTTP 错误响应（不是 SSE），所以客户端能像真实 API 那样在流开始前重试；要模拟流已经开始之后才出的错，用 `in_stream: true`。

只写 `status: 404` 时，生成接口的默认文案回显请求里的模型名：Chat / Responses 是 “The model `x` does not exist ...”，Anthropic 是 `model: x`。

### Usage

`usage` 的字段同时接受 OpenAI 和 Anthropic 的写法：`prompt_tokens` / `input_tokens`、`completion_tokens` / `output_tokens`（含推理）、`cached_tokens`（`cache_read_tokens`）、`reasoning_tokens`、`cache_creation_tokens`（`cache_creation_input_tokens`）。写了的字段原样出现在响应的 usage 里（流式也是），没写的照常用 tokenizer 计算（Anthropic 协议下 `input_tokens` 同样原样输出：规则里写 `input_tokens: 20236` 又写 `cached_tokens: 5`，响应里是 `input_tokens: 20236`、`cache_read_input_tokens: 5`，请求日志里的 `prompt_tokens` 是 20241。真实 API 的 `input_tokens` 不含缓存读写、`count_tokens` 返回总数；没有写 `input_tokens` 的规则时，fakellm 按真实语义由 tokenizer 总数扣减缓存得出。规则里写的 `input_tokens` **永远不会被 `cached_tokens` 扣减**，`cache_read_input_tokens` / `cache_creation_input_tokens` 也原样输出；`count_tokens` 返回规则里写的值）；

Anthropic 流式里 `input_tokens` 出现在哪个事件由 `input_tokens_in` 控制：`start`（默认）放在 `message_start` 的 usage（连同两个缓存字段），`message_delta` 只带 `output_tokens`；`delta` 反过来，`message_start` 的 usage 只有 `output_tokens: 1`，`input_tokens` 和缓存字段在 `message_delta`。同步响应不受影响；`completion_tokens` 没写时，会用你给的 `reasoning_tokens` 重新累加。

`POST /v1/messages/count_tokens` 也会查剧本：第一条匹配且写了 `usage.input_tokens` 的规则，它的值就是返回的 `input_tokens`，并**算一次命中**（受 `times` 约束）；没有这样的规则时返回 tokenizer 的计数。没写 `input_tokens` 的规则在 count_tokens 里被整条跳过，不会被消耗。

```yaml
- match: { last_user: "context" }
  reply: { text: "ok", usage: { input_tokens: 20236, output_tokens: 64 } }
```

### Stream

`chunk_tokens` 和 `cut_after_chunks` 只对流式请求有意义。`ttft_ms` 和 `tokens_per_sec` 同步请求也读：整个响应在 `ttft + 间隔 × (token 数 - 1)` 之后一次返回，也走服务端时钟。没写的字段用命令行的默认值（`--ttft`、`--tokens-per-sec`）。

| 字段 | 类型 | 说明 |
|---|---|---|
| `ttft_ms` | 整数 | 首 token 延迟：从请求开始到发出第一个 chunk（role chunk），毫秒 |
| `tokens_per_sec` | 数字 | 流式速度，必须大于 0。chunk 之间等待 `chunk_tokens / tokens_per_sec` 秒；不写则 chunk 之间不等待 |
| `chunk_tokens` | 整数 | 每个 content chunk 含几个 token，默认 1（真实 API 的行为）；0 按 1 处理 |
| `args_chunk_bytes` | 整数 | 流式工具调用的参数每个分片的字节数，至少 1（0 是 400），覆盖 `--args-chunk-bytes`（默认 16），三个协议都生效。配合 `tokens_per_sec` 可以模拟缓慢的工具参数，如 `args_chunk_bytes: 5` 把 20 字节的参数分成 4 片 |
| `cut_after_chunks` | 整数 | 发完这么多个 content chunk 后结束 body，**不发 finish chunk 和 `[DONE]`**（Anthropic 没有 `message_delta` / `message_stop`，Responses 没有 `response.completed`），用来测客户端的流中断处理。结束是干净的 chunked 结尾，不是连接中断（要中断用 `reset_after_chunks`） |
| `reset_after_chunks` | 整数 | 和 `cut_after_chunks` 同一个位置，但连接被**异常中断**（没有结尾的 `0\r\n\r\n`）：连接设了 `SO_LINGER(0)`，关闭时发 TCP RST，客户端看到 `ECONNRESET` / `connection reset by peer`（curl 的退出码是 56），而不是干净的流结束。两者都写时取较小的那个 |

`reply.reset: true`：同步响应在响应头（200）和一半 body 发出后连接被中断；流式请求在第一个非头部帧处中断（等价于 `reset_after_chunks: 0`）。连接以 RST 中断（没有结尾 chunk），客户端在读 body 时报传输错误。限制：响应头已经发出，所以不可能表现成 HTTP 错误状态，也无法在发头之前断开（那种情况用 `hang` 加客户端超时）。

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

## 挂起

`hang: true` 让 fakellm 收到请求后什么都不回：**连响应头都不发**，客户端看到的是一个一直等不到响应的连接，直到它自己的超时或取消。`hang_ms: N` 是“挂 N 毫秒再回答”，期间同样不发响应头，到时间后按规则的 `text` / `tool_calls` / `error` 正常回答。这样可以测客户端的请求超时、abort、steer 窗口，而且不用真的等：

```yaml
- name: never
  match: { last_user: "^hang$" }
  reply: { hang: true }
- name: late
  match: { last_user: "^slow$" }
  reply: { hang_ms: 30000, text: "finally" }       # 冻结时钟时一直挂着，advance 30s 后才回答
```

- 请求一被挂住就写进请求日志：`status: 0`、`hung: true`，测试里不用等客户端放弃就能看到它。放行（时钟到点、`reset`）后这条记录被更新为真实的 `status` 和 `latency_us`、`hung: false`；客户端先放弃则 `status` 仍为 `0`、`hung: false`、`latency_us` 为挂了多久。
- `reset` 会唤醒所有挂住的请求：有 `hang_ms` 的按规则其余部分回答（`text` / `tool_calls` / `error`），只写 `hang: true` 的回 503（没有可回答的内容）。
- 等待读的是服务端时钟：冻结时钟时 `hang_ms` 不会结束，`ctl clock advance 30s` 立刻放行。`hang: true`（没有 `hang_ms`）永远不会自己结束，推进时钟也没用。
- 客户端放弃（超时、abort、断开）后，这个请求的记录保持 **`status: 0`**，表示“没有发出任何响应”，`reply.hang` 是 `{"until_ms": null}` 或毫秒数；如果等待期间日志被清空，这条记录会被丢弃。
  所以挂起的请求要等客户端放弃之后才在日志里出现，断言前先让客户端超时：`fakellm ctl requests list --status 0`。
- 不要把 `hang` 和 `delay_ms` / `stream.ttft_ms` 搞混：后者是“等一会儿再发响应头”（流的响应头在 `ttft` 之前就发了，非流式和错误响应在等待结束后才发），`hang` 是“根本不回”。
- 带 `hang_ms` 又带 `error` 的规则，是“挂一会儿再失败”，常用来模拟网关超时前的长时间无响应。

## 协议里的错误形态

同一个 `error` 规则，不同协议的客户端看到的东西不一样：

| | HTTP 错误（默认） | `in_stream: true` 且请求是流式 |
|---|---|---|
| Chat | 状态码 + `{"error": {message, type, param, code}}` | 200 的事件流：一条 `data: {"error": {...}}`，然后 `data: [DONE]` |
| Anthropic | 状态码 + `{"type": "error", "error": {type, message}}` | 200 的事件流：`message_start`、`ping`，然后 `event: error`，`data: {"type":"error","error":{type,message}}`；没有 `message_stop` |
| Responses | 状态码 + OpenAI 错误体 | 200 的事件流：`response.created`，然后 `response.failed`，错误在 `response.error` 里：`{code, message}`，设了 `retry_after` 或 `headers` 时还有 `headers` 对象 |

Responses 的 `response.error.code` 由规则决定：写了 `code` 就用它；没写时按状态码推：`insufficient_quota` 类型用 `insufficient_quota`，429 是 `rate_limit_exceeded`，503 和 529 是 `server_is_overloaded`，其他 5xx 是 `server_error`，都不是就用 `type`。
`in_stream` 在三种协议里都只对流式请求生效；对非流式请求，同一条规则退回成 HTTP 错误。

## 故障注入：每一类怎么写

下面的“类别”是 oar 这类 agent 运行时分类错误时用的名字。**oar 自己的分类代码不在本仓库里，这里没有对照它验证**；下表按各家真实 API 的状态码、`type`、`code` 和文案约定写，所有例子都对真实二进制跑过，看到的响应和下面写的一致。分类器如果按文案或别的字段判断，请调整 `message` / `code`。

| 类别 | Chat / Responses（HTTP） | Anthropic Messages（HTTP） |
|---|---|---|
| `auth` | `status: 401`（默认 `invalid_api_key`，“Incorrect API key provided...”） | `status: 401`（默认 `authentication_error`，“invalid x-api-key”） |
| `quota` | `status: 429, type: insufficient_quota, code: insufficient_quota`，文案 “You exceeded your current quota...” | `status: 400, type: invalid_request_error`，文案 “You have reached your specified workspace API usage limits.” |
| `rate_limited` | `status: 429`（默认 `type: requests`, `code: rate_limit_exceeded`），加 `retry_after` | `status: 429`（默认 `rate_limit_error`），加 `retry_after` |
| `billing` | `status: 400, code: billing_hard_limit_reached`，文案 “Billing hard limit has been reached.” | `status: 402, type: billing_error` |
| `model_unavailable` | `status: 404`（默认 `code: model_not_found`） | `status: 404`（默认 `not_found_error`），建议自己写 `message: "model: claude-x"` |
| `input_too_large` | `status: 400, code: context_length_exceeded, param: messages`，文案 “This model's maximum context length is ...” | `status: 400, type: invalid_request_error`，文案 “prompt is too long: 213462 tokens > 200000 maximum”；请求体太大用 `status: 413`（默认 `request_too_large`） |
| `overloaded` | `status: 503`（默认 `server_error`，“The engine is currently overloaded”）；也可以用 `529`（默认 `overloaded_error`） | `status: 529`（默认 `overloaded_error`） |
| `provider` | `status: 500`（默认 `server_error`）或 502 / 504 | `status: 500`（默认 `api_error`）或 502 / 504 |
| 流中途断开 | `stream: { cut_after_chunks: N }` | 同左 |
| 流中途连接中断（无结尾 chunk） | `stream: { reset_after_chunks: N }`；同步用 `reply: { reset: true }`（头已发出，见 Stream 一节） | 同左 |
| 流中途出错 | 加 `in_stream: true` | 同左 |
| 挂起 | `hang: true` 或 `hang_ms` | 同左 |

要点：

- **客户端重试**：官方 SDK 会重试 408、409、429 和 5xx，不会重试 400、401、403、404。`x-should-retry: "true"` / `"false"` 可以覆盖 SDK 的判断，`retry-after-ms` 和 `retry-after` 决定等多久；这两个头用 `error.headers` 加（`retry_after` 只写 `retry-after`）。
- **只想让它失败几次**：加 `times: N`，或者用 `every_nth` / `probability` 做间歇故障；再用 `ctl requests list --rule X --count` 断言重试次数。
- **混合**：同一个规则列表里按 `protocol` 区分不同协议的错误体（见下面的完整例子），一份剧本可以同时给三种客户端用。

### 官方 SDK 实测

下面是 `sdk/node/faults.test.mjs`用官方 SDK 逐类跑出来的结果，默认重试设置（openai / Anthropic 的 `maxRetries` 和 ai-sdk 的 `maxRetries` 都是 2，所以“3”表示发了 3 个请求）。格子里是 `错误类 状态码 / 请求数`；OpenAI 协议的规则用 openai 和 ai-sdk（`@ai-sdk/openai` 的 `.chat`），Anthropic 协议的规则用 `@anthropic-ai/sdk` 和 `@ai-sdk/anthropic`。ai-sdk 重试用尽后抛 `RetryError`（`lastError` 是最后一个 `APICallError`，`errors` 里有每次的错误），表里写成 `RetryError(状态码)`。

| 类别（规则） | `openai` | `@anthropic-ai/sdk` | ai-sdk + OpenAI | ai-sdk + Anthropic |
|---|---|---|---|---|
| `auth` 401 | `AuthenticationError` 401 / 1 | `AuthenticationError` 401 / 1 | `APICallError` 401 / 1 | `APICallError` 401 / 1 |
| `quota`（OpenAI 429 `insufficient_quota`；Anthropic 400） | `RateLimitError` 429 / **3**，`code` / `type` 是 `insufficient_quota` | `BadRequestError` 400 / 1 | `RetryError(429)` / **3** | `APICallError` 400 / 1 |
| `rate_limited` 429 | `RateLimitError` / 3 | `RateLimitError` / 3 | `RetryError(429)` / 3 | `RetryError(429)` / 3 |
| `rate_limited` + `x-should-retry: false` | `RateLimitError` / **1** | `RateLimitError` / **1** | `RetryError(429)` / **3**（不读这个头） | `RetryError(429)` / **3** |
| `billing`（OpenAI 400；Anthropic 402） | `BadRequestError` 400 / 1，`code: billing_hard_limit_reached` | `APIError` 402 / 1（402 没有专门的类） | `APICallError` 400 / 1 | `APICallError` 402 / 1 |
| `model_unavailable` 404 | `NotFoundError` / 1，`code: model_not_found` | `NotFoundError` / 1 | `APICallError` 404 / 1 | `APICallError` 404 / 1 |
| `input_too_large` 400 | `BadRequestError` / 1，`code: context_length_exceeded`、`param: messages` | `BadRequestError` / 1 | `APICallError` 400 / 1 | `APICallError` 400 / 1 |
| `overloaded`（OpenAI 503；Anthropic 529） | `InternalServerError` 503 / 3 | `InternalServerError` 529 / 3 | `RetryError(503)` / 3 | `RetryError(529)` / 3 |
| `provider` 500 | `InternalServerError` / 3 | `InternalServerError` / 3 | `RetryError(500)` / 3 | `RetryError(500)` / 3 |
| 流中途 `cut_after_chunks`（流干净结束，无结尾事件） | 迭代正常结束，只收到前几块，**没有报错** | `stream()` 的 `finalMessage()` 抛 `AnthropicError: stream ended without producing a Message`；直接迭代事件则正常结束 | `streamText` 正常结束，文本被截短，没有 `error` 部分 | 同左 |
| 流中途 RST `reset_after_chunks` / `reset` | `TypeError: terminated`，`cause.code` 是 `ECONNRESET`（Bun 直接是 `ECONNRESET` 错误），不重试 | `AnthropicError: terminated`（`finalMessage()`），不重试 | `streamText` 的错误（`Failed to process successful response` 或连接错误），不重试 | 同左 |
| 流内错误 `in_stream: true` | `APIError`，**没有 `status`**，有 `type`（如 `server_error`），不重试 | `APIError`，没有 `status`，`type: overloaded_error`，不重试 | `error` 部分是 `RetryError`，重试 3 次（流还没产出内容时 ai-sdk 把它当 HTTP 错误处理） | `error` 部分是 `AI_StreamProviderError`（`type`、`statusCode: 529`、`isRetryable: true`），**不重试**，1 次 |
| 挂起 `hang_ms` + 冻结时钟 | 请求一直不返回，`ctl clock advance` 后正常返回 | 同左 | 同左 | 同左 |

SDK 差异里值得记住的几点：

- **429 `insufficient_quota` 会被 openai 和 ai-sdk 重试**，它们只看状态码；想让调用方“立刻失败”，用 `x-should-retry: "false"`（openai / Anthropic SDK 认；ai-sdk 不认，它只按 408 / 409 / 429 / 5xx 判断，所以 ai-sdk 下只能用 `times: 1` 控制次数或设 `maxRetries: 0`）。
- `x-should-retry: "true"` 能让 openai / Anthropic SDK 重试 400，`"false"` 能让它们不重试 500；ai-sdk 两个方向都忽略。
- `retry-after-ms`（以及 `retry-after`，60 秒以内）决定两个官方 SDK 的等待；没有这个头时它们按 0.5 秒起步的指数退避等，测试里的重试用 `headers: { retry-after-ms: "5" }` 来加速。
- `@anthropic-ai/sdk` 的错误 `message` 是 `状态码 + 整个错误体 JSON`（如 `429 {"type":"error","error":{...}}`），不是只有文案；要看 `type` 用 `e.error.error.type`（HTTP 错误）或 `e.type`（流内 `error` 事件）。
- 不带 `in_stream` 的 HTTP 错误在流式请求上也是普通 HTTP 错误，各 SDK 会照常重试；`in_stream` 之后的错误不会被 openai / Anthropic SDK 重试。

### Responses（codex）的限制

codex 对 HTTP 错误体不做细分类：**400 的 body 一律是不重试的 `InvalidRequest`**，所以 `context_length_exceeded`、`insufficient_quota` 这类要被 codex 认出来的错误，**必须作为流内的 `response.failed` 注入**，即错误规则写 `in_stream: true`，并且请求是流式的（codex 总是流式）。codex 对 `response.error.code` 的反应（来自对 codex 源码的调研，见 [research.md](research.md)）：

| `response.error.code` | codex 的行为 |
|---|---|
| `context_length_exceeded` | 不重试，上下文溢出错误 |
| `insufficient_quota` | 不重试，额度错误 |
| `rate_limit_exceeded` / `slow_down` | 重试；等多久从 `error.headers` 或消息里的 “try again in Ns” 读 |
| `server_is_overloaded` | 只有带 retry-after 才重试 |
| `usage_not_included` | 套餐错误 |
| 其他 code | 一律重试 |

HTTP 层面（不在流里）：429 加 `{"error":{"type":"usage_limit_reached"}}` 是 usage limit（codex 会读 `x-codex-active-limit` 头，用 `error.headers` 加）；`insufficient_quota` 是额度错误；其他 429 是 RetryLimit；5xx 重试；401、404 也会被当成可重试的意外状态码。
`response.incomplete` 的 `content_filter` 原因会被 codex 重试，`max_output_tokens` 是硬错误；对应 `finish_reason: content_filter` 和 `finish_reason: length`。

上表现已用真实 codex 0.162.1 验证（`e2e/codex-faults.sh`）：`context_length_exceeded`、`insufficient_quota`、`rate_limit_exceeded`（带 `error.headers`，`stream_max_retries = 2` 时重试）、`server_is_overloaded`（无 `retry_after` 不重试，有则重试）、`response.incomplete` 的 `max_output_tokens`、`apply_patch`、挂起；每一项的输出和请求数见 [wire.md](wire.md#codexopenai-codex-cli)。`in_stream: true` 的 `context_length_exceeded` 打印 “Codex ran out of room in the model's context window...” 并退出码 1，不重试；同一条规则不加 `in_stream`（HTTP 400）时，codex 原样打印错误体 JSON，没有这句友好提示。`response.failed` 的形状有 `tests/responses.rs` 覆盖。

### RST 经过 CONNECT 代理

`reset_after_chunks` / `reset` 的 RST 是 fakellm 到它的直接对端的 TCP RST。中间隔着 HTTP CONNECT 代理时，客户端看到什么取决于代理怎么处理“上游连接被重置”，fakellm 无法控制。`e2e/connect-rst.sh`（`e2e/connect-proxy.mjs` 加一个裸 socket 客户端）实测：

| 路径 | 客户端看到 |
|---|---|
| 直连 fakellm | `ECONNRESET`，没有结尾 chunk |
| 经过 `socket.pipe()` 式的代理，上游出错时 `client.destroy()`（最常见的写法） | **干净的 EOF（FIN）**，没有结尾 chunk；客户端的 HTTP 层报“流提前结束”（pi-ai 等看到的 `terminated`），区分不了是 RST 还是正常关闭 |
| 同样的代理，但上游出错时 `client.resetAndDestroy()` | `ECONNRESET`，和直连一样 |

所以：想在代理后面测“连接被重置”，要么让代理转发 RST，要么接受客户端看到的是“body 没有结尾 chunk 的 EOF”；两种情况下 chunked 响应都是不完整的，客户端不会把它当成功。需要区分 RST 和 FIN 的测试请直连 fakellm。

## 自定义端点

有些 SDK 和 harness 除了 LLM 路由，还会调用非标准的 JSON 接口。写一条带 `match.path` 和 `reply.body` 的规则就能回答它：

- `match.path` 是路径 glob。设置之后这条规则适用于 API 下的任意路径，包括标准路由不认识的路径；它只看这类请求，不会被 `/v1/chat/completions` 等标准请求命中，反过来没有 `path` 的规则（包括 `any: true`）也不会命中自定义请求。
- `reply.body` 是原样返回的 JSON；`reply.status`（默认 200）和 `reply.headers` 可选。`path` 和 `body` 必须成对出现，且这类回复不能再写 `text`、`tool_calls`、`error`、`trajectory`、`policy`、`reasoning`；`status` / `headers` 没有 `path` 是 400。
- 用 `match.body`（正则，匹配原始请求正文）区分同一路径上的不同请求；`times`、`name`、`every_nth`、`probability` 照常生效，规则按顺序，第一条命中的回答。
- 没有任何规则命中的未知路径仍是 404 `Invalid URL (POST /path)`。
- 配置了 `--path-prefix` 时，匹配 `match.path` 之前先去掉这个前缀，所以 `/v1/systemone` 的规则同时回答 `/v1/systemone` 和 `/anthropic/v1/systemone`；其他前导路径段只在标准路由后缀前才会被去掉，自定义路径按字面匹配。请求日志保留完整路径。
- 自定义端点**不经过 `--api-key` 校验**（校验在标准路由里），错误的密钥也会得到规则的回复；`tests/custom_protocols.rs` 把这个现状固定了下来。
- 请求日志里 `protocol` 为 `"custom"`，`raw` 是原始正文，`rule` 和 `source` 是命中的规则，`reply.text` 是返回的 JSON 文本；`/_fakellm/requests?protocol=custom` 可以过滤。自定义请求不经过 cassette、上游和回退，不支持流式和 `delay_ms`。

示例：xsai 的 decide 包 `POST /v1/systemone`（请求 `{model, state, questions}`，响应 `{answers, usage}`，格式见 consumers.md），按 `state` 区分两个用例：

```yaml
- name: systemone-angry
  match: { path: /v1/systemone, body: "/angry/i" }
  reply:
    body:
      answers:
        damaged: { type: noul, noul: 0.9 }
        route: { type: choice, choice: shipping, confidence: 0.8, probabilities: { billing: 0.2, shipping: 0.8 } }
        anger: { type: score, score: 2, confidence: 0.7, probabilities: { "0": 0.1, "1": 0.2, "2": 0.7 } }
      usage: { input_tokens: 42, output_tokens: 8 }
- name: systemone-happy
  match: { path: /v1/systemone, body: "/happy/i" }
  reply:
    status: 200
    headers: { x-fake: "1" }
    body:
      answers:
        damaged: { type: noul, noul: 0.1 }
      usage: { input_tokens: 42, output_tokens: 8 }
```

## 与 cassette、上游的关系

剧本规则永远排在 [cassette](cassette.md) 和上游代理前面：一个请求先按注册顺序找规则，**没有规则命中**才在 cassette 里按含义查找，再不行才转发给 `--upstream`，最后是默认兜底（`--strict` 时 400）。后果：

- 规则可以盖住录制：给某个请求写一条 `match` + `reply`，就不会走回放。
- 想在回放上注入故障，写一条带 `probability` / `every_nth` / `times` 的错误规则，放行的那部分请求自然落到 cassette。`times: 1` 的 429 规则适合测重试：第一次 429，重试时规则已用完，请求落到 cassette 得到录制的回复。
- 规则命中的请求**不会**被录制，也不会到达上游，即使配置了 `--upstream`。
- 规则的 `reply.trajectory` / `reply.policy` 把请求交给[轨迹或策略](agent.md)，同样排在 cassette 之前；录好的会话想按步骤推进，用 `fakellm cassette to-trajectory` 转成轨迹。
- `/v1/embeddings` 只有带 `error` 的规则对它生效，cassette 不处理它。
- 请求日志的 `source` 区分谁回答了：`script`（规则、轨迹、策略）/ `cassette` / `upstream` / `fallback` / `strict`，`ctl requests list --source cassette` 可以过滤。
- 兜底的 `json_schema` 结构化输出见[README](../README.md#结构化输出)。
- `PUT /_fakellm/scripts` 只替换规则；`reset` 默认连 cassette 和录制一起清掉（`--keep-scripts` 保留规则，也保留 cassette）。

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

## 示例四：一份剧本覆盖所有故障类别

下面的文件三种协议通用，用提示词里的 `fault:<类别>` 触发。每条规则对应上面表格的一行；同一个类别在 Anthropic 和 OpenAI 系协议下的错误体不同，所以写成两条，带 `protocol` 的写在前面。

```yaml
# faults.yaml —— fakellm ctl scripts put faults.yaml
- name: auth
  match: { last_user: "fault:auth" }
  reply: { error: { status: 401 } }

- name: quota-anthropic
  match: { protocol: anthropic.messages, last_user: "fault:quota" }
  reply:
    error: { status: 400, type: invalid_request_error, message: "You have reached your specified workspace API usage limits." }
- name: quota
  match: { last_user: "fault:quota" }
  reply:
    error: { status: 429, type: insufficient_quota, code: insufficient_quota, message: "You exceeded your current quota, please check your plan and billing details." }

- name: rate-limited
  match: { last_user: "fault:rate" }
  reply: { error: { status: 429, retry_after: 2, headers: { retry-after-ms: "2000" } } }

- name: billing-anthropic
  match: { protocol: anthropic.messages, last_user: "fault:billing" }
  reply: { error: { status: 402, type: billing_error, message: "There was an issue with your billing or payment information." } }
- name: billing
  match: { last_user: "fault:billing" }
  reply: { error: { status: 400, type: invalid_request_error, code: billing_hard_limit_reached, message: "Billing hard limit has been reached." } }

- name: model-unavailable
  match: { last_user: "fault:model" }
  reply: { error: { status: 404 } }

- name: too-large-anthropic
  match: { protocol: anthropic.messages, last_user: "fault:large" }
  reply: { error: { status: 400, type: invalid_request_error, message: "prompt is too long: 213462 tokens > 200000 maximum" } }
- name: too-large
  match: { last_user: "fault:large" }
  reply:
    error:
      status: 400
      code: context_length_exceeded
      param: messages
      message: "This model's maximum context length is 128000 tokens. However, your messages resulted in 200000 tokens."

- name: overloaded
  match: { last_user: "fault:overloaded" }
  reply: { error: { status: 529 } }

- name: provider
  match: { last_user: "fault:provider" }
  reply: { error: { status: 500, headers: { x-should-retry: "true" } } }

- name: cut
  match: { last_user: "fault:cut" }
  reply: { text: "one two three four five six", stream: { cut_after_chunks: 2 } }

- name: hang
  match: { last_user: "fault:hang" }
  reply: { hang: true }
```

对 Chat 发 `fault:quota` 和对 Messages 发同样的提示词：

```
$ curl $E/v1/chat/completions -d '{"model":"m","messages":[{"role":"user","content":"fault:quota"}]}'
429 {"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}
$ curl $E/v1/messages -d '{"model":"m","max_tokens":9,"messages":[{"role":"user","content":"fault:quota"}]}'
400 {"type":"error","error":{"type":"invalid_request_error","message":"You have reached your specified workspace API usage limits."}}
```

`fault:rate` 在 Anthropic 下的响应头：`retry-after: 2`、`retry-after-ms: 2000`。流中途断开（`fault:cut`）在三种协议下看到的事件是：Chat 两个 content chunk 之后结束；Anthropic 是 `message_start`、`content_block_start`、`ping`、两个 `content_block_delta` 之后结束；Responses 是 `response.created`、`response.in_progress`、`response.output_item.added`、`response.content_part.added` 和两个 `response.output_text.delta` 之后结束。

间歇故障：每第 3 个请求失败，其余成功；或者随机 30% 失败：

```yaml
- name: every-third
  match: { every_nth: 3 }
  reply: { error: { status: 500 } }
- name: thirty-percent
  match: { probability: 0.3 }
  reply: { error: { status: 529 } }
- reply: { text: "ok" }
```

`probability` 是确定性的：同一个请求序列每次运行得到同样的成败顺序，不需要种子参数。

## 示例五：codex 的上下文溢出

codex 只认流内的 `response.failed`。下面的规则让第一次请求溢出，之后的请求正常回答（codex 不重试 `context_length_exceeded`，所以它会向用户报告错误并停下，这正是要测的路径）：

```yaml
- name: overflow
  match: { protocol: openai.responses }
  times: 1
  reply:
    error:
      status: 400
      in_stream: true
      code: context_length_exceeded
      message: "Your input exceeds the context window of this model. Please adjust your input and try again."
- reply: { text: "FAKELLM_OK" }
```

流式请求得到的是（节选，`response` 对象有省略）：

```
HTTP/1.1 200 OK
content-type: text/event-stream; charset=utf-8

event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_...","object":"response","status":"in_progress",...,"output":[]}}

event: response.failed
data: {"type":"response.failed","sequence_number":1,"response":{"id":"resp_...","object":"response","status":"failed","error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model."},...,"output":[]}}
```

没有 `[DONE]`（Responses 的流不发它）。对非流式请求（`stream: false`）同一条规则退回成 HTTP 400 `{"error": {..., "code": "context_length_exceeded"}}`，这是官方 SDK 能识别的形态。

另外两个常用的 codex 故障：

```yaml
# 限流，读 error.headers 里的等待时间；codex 的重试被 wire 的配置关掉了，所以请求日志里只会看到一次
- match: { protocol: openai.responses, last_user: "limit" }
  reply:
    error:
      status: 429
      in_stream: true
      code: rate_limit_exceeded
      message: "Rate limit reached. Please try again in 2s."
      retry_after: 2
# 过载：只有带 retry-after 才会被 codex 重试
- match: { protocol: openai.responses, last_user: "busy" }
  reply: { error: { status: 503, in_stream: true, retry_after: 1 } }
```

`retry_after` 在 `response.failed` 里变成 `response.error.headers` 的 `retry-after`。

### 路径前缀与其他（M3）

- 路径前缀：`--path-prefix /anthropic`（`Config.path_prefix`）会先被剥掉；此外，任何前置若干段后恰好是 `/v1/messages`、`/v1/messages/count_tokens`、`/v1/chat/completions`、`/v1/responses[/id]`、`/v1/models[/id]`、`/v1/embeddings`（或不带 `/v1` 的写法）的路径都会路由到对应接口，例如 `/gateway/v2/proxy/v1/chat/completions`。请求日志保留完整路径；其余路径仍是 404。
- `fakellm ctl requests list` 对挂起（`hung`）的请求在状态列显示 `hung`。
