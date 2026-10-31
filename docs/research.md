# 真实消费方对 LLM API fake 的要求（调研摘要，2026-10-31）

调研对象：vercel/ai、openai/codex、anthropics/claude-agent-sdk、badlogic/pi-mono、anomalyco/opencode、
moeru-ai/xsai、getpaseo/paseo、botiverse/raft-source、botiverse/oar。本文只记对 fakellm 实现有约束力的结论；
实现代码必须遵守"硬约束"一节。

## 现状

- 没有人用现成的假服务。paseo 手写了约 5 个 `http.createServer` 假模型（Responses / Messages / Chat 各一套），
  raft 十几个测试文件各自手写假 provider，oar 用 `@copilotkit/aimock`（LLMock），opencode 自己做了
  `@opencode/simulation`（进程内 Chat SSE 假后端 + JSON-RPC 控制面）。
- agent 循环是第一消费场景：pi、opencode、oar、paseo、raft 全是 agent harness。没有 tool call 的 fake 对它们没用。
- 踩过的坑：
  - 时间全是真的：aimock 的 latency 用墙钟导致 abort/steer 窗口 flaky；paseo 的 retry 测试真等 12 秒。
  - 请求日志丢字段：oar 为了断言 `reasoning.effort` 不得不给 aimock 加 `captureRaw`。结论：日志必须原样保存 raw body。
  - 剧本无限重复匹配：pi 在 aimock 上 30 秒执行了 1143 次 tool。需要 `times` 和 `has_tool_result` 这类有状态条件。
  - 旁路请求抢剧本：opencode 第一个请求是生成标题，CodeBuddy 有 "Generate a concise, sentence-case title" 的
    system prompt。要能用单独规则（`system` 正则、`no_tools`）兜住。
  - 上下文溢出触发压缩：opencode 靠 `error.code == context_length_exceeded` 或文本
    "maximum context length is N tokens" 触发 compaction。错误文案要和 OpenAI 一致。

## Chat Completions 硬约束（违反会让客户端直接报错）

来源：`@ai-sdk/openai` / `@ai-sdk/openai-compatible` 的 zod schema、xsai 的解析器、pi-ai 的 openai-completions 适配器、openai 官方 SDK。

流式 chunk：
- 每个 chunk 必须有 `choices` 数组。usage chunk 用 `"choices": []`，不能省略这个键。
- 每个 choice 必须有 `index`（数字）。`delta`、`finish_reason`、`logprobs` 可为 null。
- `delta.tool_calls[]` 每项必须有 `index`（数字）和 `function` 对象；`function.name` / `function.arguments` 可缺。
  第一个片段带 `id`、`type: "function"`、`function.name`、`function.arguments: ""`；后续片段只带 `index` 和
  `function.arguments` 的一段。客户端按 `index` 拼接，参数可以是不完整 JSON（pi 和 oar 都测 cut-off JSON）。
- **必须有一个 `finish_reason` 非 null 的 chunk**。没有的话 ai-sdk 判 `interrupted`，xsai 判 `truncated-stream`，
  pi 抛 "Stream ended without finish_reason"。
- 第一个 chunk 是 `delta: {"role":"assistant","content":"","refusal":null}`（真实 OpenAI 的行为）。
- `stream_options.include_usage: true` 时：每个 chunk 带 `"usage": null`，最后一个 chunk `choices: []` 带 usage 对象。
  不带 include_usage 时 chunk 里没有 usage 键。
- 结尾 `data: [DONE]`。ai-sdk 和 xsai 会跳过它，但 openai SDK 用它结束迭代。
- `event:` 行会被忽略，只有 `data:` 行有意义。每帧 `data: <json>\n\n`。
- finish_reason 映射：`stop`、`length`、`tool_calls`、`content_filter`（pi 把 content_filter 当 error）。
- ai-sdk 只消费 `choices[0]`；xsai 跳过 `index !== 0`。`n > 1` 很少用，但格式上要正确。
- 未知顶层字段会被 zod 剥掉（`system_fingerprint`、`obfuscation`、`service_tier`、`refusal` 都安全）。

同步响应：
- `choices` 必填且非空，`choices[].index`、`message` 必填；`message.tool_calls[].type` 必须是 `"function"`，
  `function.name` / `function.arguments` 必须是字符串。
- `usage` 结构：`prompt_tokens`、`completion_tokens`、`total_tokens`、`prompt_tokens_details.cached_tokens`、
  `completion_tokens_details.reasoning_tokens`（pi 和 xsai 读这两个 details）。

错误：
- 体：`{"error":{"message","type","param","code"}}`，`message` 必填（ai-sdk 和 openai-compatible 都要求），
  其余可为 null。
- HTTP 响应必须有正确的 status text（hyper 自动给已知状态码加；529 没有标准短语，用 "Overloaded" 自定义或接受空）。
  ai-sdk 解析失败时退回 `statusText` 作为消息。
- 默认重试：ai-sdk 重试 408/409/429/5xx，最多 2 次，2s/4s 真实定时器；openai SDK 重试 408/409/429/5xx 2 次；
  pi 重试 429/500/502/503/504/520/524 并读 `retry-after-ms` / `retry-after`；opencode 按 status + code + 文本分类。
  **流开始前的错误会被重试，开始后的不会**。
- `retry-after` 秒数；ai-sdk 只接受 < 60s 的值。

opencode 的错误分类（故障注入要能触发这些分支）：
- context-overflow：code `context_length_exceeded` / `model_context_window_exceeded` / `request_too_large`，
  或文本 "prompt is too long" / "maximum context length is N tokens" / "exceeds the context window"。非 429。
- RateLimit：429，或 code 含 `rate_limit` / `too_many_requests`。
- QuotaExceeded：402，或 429 且文本含 quota。
- Authentication：401/403。
- ProviderInternal：>= 500、408、409，或 code `overloaded_error` / `server_error` / `internal_error`。
- `x-should-retry: true|false` 头可以覆盖。

## 请求字段（必须能解析而不报错，未知的放进 extra）

`model`、`messages`（role system/developer/user/assistant/tool；content 字符串或 parts 数组：`text`、`image_url`、
`input_audio`、`file`、`refusal`；assistant 的 `tool_calls`、`reasoning_content`、`refusal`；tool 的 `tool_call_id`；
`name`）、`tools[].function {name, description, parameters, strict}`、`tool_choice`（`auto|none|required` 或
`{type:"function",function:{name}}`）、`parallel_tool_calls`、`stream`、`stream_options.include_usage`、
`max_tokens`、`max_completion_tokens`、`temperature`、`top_p`、`n`、`seed`、`stop`（字符串或数组）、
`response_format`（`text` / `json_object` / `json_schema {name, schema, strict, description}`）、`reasoning_effort`、
`verbosity`、`store`、`metadata`、`user`、`service_tier`、`prompt_cache_key`、`prompt_cache_retention`、
`safety_identifier`、`logprobs`、`top_logprobs`、`logit_bias`、`presence_penalty`、`frequency_penalty`、`prediction`、
`chat_template_kwargs`、`enable_thinking`、`top_k`、`tool_stream`。

请求头：`Authorization: Bearer <key>`（xsai 没 key 时不发）、`x-api-key`、`OpenAI-Organization`、`OpenAI-Project`、
`user-agent`（含 `ai-sdk-openai/<v>`）、`x-session-id` / `x-session-affinity`（pi）、`originator`（codex）、
`Accept: text/event-stream`。opencode 的 system prompt 约 2 KB，日志不能截断 IR 文本。

## 其它接口

- `GET /v1/models` → `{"object":"list","data":[{"id","object":"model","created","owned_by"}]}`。paseo 用
  `@ai-sdk/openai-compatible` 跑 opencode 时会先请求它。
- 任意模型名都要接受（CLIProxyAPI 等代理用自定义名字；Claude CLI 会发 `sonnet`、`glm-5.2[1m]` 之类）。

## 时间与节奏

- 虚拟时钟管服务端：ttft、token 间隔、`retry-after` 的值、`created`。管不住客户端自己的 backoff 定时器
  （ai-sdk 的 2s/4s、openai SDK 的退避），README 要写明。
- pi 的 faux provider 用 `tokenSize`（每 chunk 3～5 token）和 `tokensPerSecond` 控制节奏；fakellm 对应
  `chunk_tokens` 和 `tokens_per_sec`，默认每 chunk 1 token（真实 API 的行为）。
- 流中途断开（不发 finish、不发 [DONE]、直接关连接）是 pi、opencode、raft 都要测的故障。

## 真实 OpenAI 流式样本（ai-sdk 的 fixture，2026-02 录制）

```
data: {"id":"chatcmpl-D8Z5oo6uDh67AD85p73ksdT1KxhE0","object":"chat.completion.chunk","created":1770933892,"model":"gpt-4.1-nano-2025-04-14","service_tier":"default","system_fingerprint":"fp_de604bd877","choices":[{"index":0,"delta":{"role":"assistant","content":"","refusal":null},"logprobs":null,"finish_reason":null}],"usage":null,"obfuscation":"abc"}
data: {"id":"chatcmpl-D8Z5oo6uDh67AD85p73ksdT1KxhE0","object":"chat.completion.chunk","created":1770933892,"model":"gpt-4.1-nano-2025-04-14","service_tier":"default","system_fingerprint":"fp_de604bd877","choices":[{"index":0,"delta":{"content":"**"},"logprobs":null,"finish_reason":null}],"usage":null,"obfuscation":"yhjoJbEF"}
data: {"id":"chatcmpl-D8Z5oo6uDh67AD85p73ksdT1KxhE0","object":"chat.completion.chunk","created":1770933892,"model":"gpt-4.1-nano-2025-04-14","service_tier":"default","system_fingerprint":"fp_de604bd877","choices":[{"index":0,"delta":{},"logprobs":null,"finish_reason":"stop"}],"usage":null,"obfuscation":"x"}
data: {"id":"chatcmpl-D8Z5oo6uDh67AD85p73ksdT1KxhE0","object":"chat.completion.chunk","created":1770933892,"model":"gpt-4.1-nano-2025-04-14","service_tier":"default","system_fingerprint":"fp_de604bd877","choices":[],"usage":{"prompt_tokens":16,"completion_tokens":300,"total_tokens":316,"prompt_tokens_details":{"cached_tokens":0,"audio_tokens":0},"completion_tokens_details":{"reasoning_tokens":0,"audio_tokens":0,"accepted_prediction_tokens":0,"rejected_prediction_tokens":0}},"obfuscation":"q"}
data: [DONE]
```

fakellm 不发 `obfuscation`（随机填充，无语义）。

## codex（openai/codex，codex-rs，2026-10 主干）

对 M1 的 Responses API 有直接约束。路径相对 `codex-rs/`。

- **只有 Responses，没有 Chat Completions**。`WireApi` 只剩 `Responses`，`wire_api = "chat"` 启动报错
  （`model-provider-info/src/lib.rs:109`）。要让 codex 连 fakellm，M1 必须做 Responses。
- **默认走 HTTP SSE**，WebSocket 只在 provider 配 `supports_websockets = true` 时用（自定义 provider 默认 false），
  所以 M1 不需要 WebSocket。`previous_response_id` 只在 WebSocket 请求里出现，HTTP 不发；`store` 永远是 false。
- **接线**：`OPENAI_BASE_URL` 不被 core 读取；`[model_providers.openai].base_url` 被忽略；要用自定义 provider：

  ```toml
  model = "gpt-5.5"
  model_provider = "fakellm"

  [model_providers.fakellm]
  name = "fakellm"
  base_url = "http://127.0.0.1:PORT/v1"
  env_key = "FAKELLM_API_KEY"        # 必须设置且非空，否则启动失败
  wire_api = "responses"
  request_max_retries = 0           # 默认 4
  stream_max_retries = 0            # 默认 5
  stream_idle_timeout_ms = 5000     # 默认 300000
  ```

  未知模型名只会警告。`GET {base}/models?client_version=...` 可以 404。这是 `fakellm wire codex` 要生成的内容。
- **测试用的最小 SSE 序列**（`core/tests/common/responses.rs`，`event:` 行加 `data:` 行）：

  ```
  event: response.created
  data: {"response":{"id":"resp-1"},"type":"response.created"}

  event: response.output_item.done
  data: {"item":{"content":[{"text":"hi","type":"output_text"}],"id":"msg-1","role":"assistant","type":"message"},"type":"response.output_item.done"}

  event: response.completed
  data: {"response":{"id":"resp-1","usage":{"input_tokens":0,"input_tokens_details":null,"output_tokens":0,"output_tokens_details":null,"total_tokens":0}},"type":"response.completed"}
  ```

  `app-server-test-client/src/loopback_responses_server.rs` 是一个现成的最小假服务参考。
- **解析器只认 `output_item.done` 和 `response.completed`**（`codex-api/src/sse/responses.rs`）：所有 `*.delta` 只用于 UI，
  `response.in_progress`、`function_call_arguments.*`、`sequence_number`、`output_index` 都不读。
  `output_item.done` 的 item 解析失败会**静默丢弃**：function_call 缺 `call_id`/`name`/`arguments`、reasoning 缺 `summary`、
  message 缺 `role`/`content` 都会让 tool 永远不执行而不报错。
- **必填**：`response.completed.response.id` 字符串；`usage` 若存在必须有 `input_tokens`/`output_tokens`/`total_tokens`，
  `input_tokens_details` 若非 null 必须有 `cached_tokens`，`output_tokens_details` 若非 null 必须有 `reasoning_tokens`。
  流在 `response.completed` 之前结束或空闲超时 → Stream 错误，整个 POST 重试（默认 5 次）。
- **错误注入的入口是 `response.failed`**，不是 HTTP 400：400 的 body 不做分类（`InvalidRequest`，不重试），
  `context_length_exceeded` 要通过 `response.failed` 的 `response.error.code` 给（`codex-api/src/sse/responses_error.rs`）。
  其它 code：`insufficient_quota`、`usage_not_included`、`rate_limit_exceeded` / `slow_down`（重试，延迟从
  `error.headers` 或消息里的 "try again in Ns" 取）、`server_is_overloaded`（有 Retry-After 才重试），未知 code 一律重试。
  HTTP 层：429 带 `{"error":{"type":"usage_limit_reached"}}` → UsageLimitReached（读 `x-codex-active-limit`）；
  `insufficient_quota` → QuotaExceeded；其它 429 → RetryLimit；5xx 重试；401/404 也算 UnexpectedStatus 会重试。
- **`response.incomplete`**：`reason = content_filter` 重试；`interrupted` 当正常完成；`max_output_tokens` 是硬错误，不重试。
- **请求体**（`codex-api/src/common.rs:281`）：`model`、`stream: true`、`input[]`、`tool_choice: "auto"`、
  `parallel_tool_calls`、`reasoning {effort?, summary?}`、`store: false`、`include: ["reasoning.encrypted_content"]`、
  可选 `tools`、`service_tier`、`prompt_cache_key`、`text {verbosity?, format?}`、`client_metadata`。没有 `instructions`：
  系统提示是第一条 `role: "developer"` 的 message item。input item 类型：message、function_call、function_call_output、
  custom_tool_call(_output)、reasoning、tool_search_call、web_search_call、local_shell_call、compaction 等，未知类型要能接受。
  tools 类型：`function`、`namespace`、`tool_search`、`web_search`、`custom`（freeform）。
- **请求头**全部可选：`Authorization`、`Accept: text/event-stream`、`session-id`、`thread-id`、`x-client-request-id`、
  `x-codex-turn-state`（服务端发了要原样回传）、`originator`、`User-Agent`。限流头 `x-codex-primary-used-percent` 等
  有则解析，无则忽略。
- 测试里 `validate_request_body_invariants` 检查每个 `function_call` 都有同 `call_id` 的 `function_call_output`，
  fakellm 的 fake agent 轨迹模式可以做同样的校验。
