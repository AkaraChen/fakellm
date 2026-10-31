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
