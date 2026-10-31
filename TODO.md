# TODO

## 已知问题（读代码、测试和真实 harness 冒烟时发现的）

- [ ] `--scripts` 没有热加载；`ctl scripts put` 是唯一的运行时入口
- [ ] 错误规则只写 `status: 404` 时，Chat / Responses 的默认文案里写死了模型名 `unknown`（“The model `unknown` does not exist ...”），不是请求里的模型名；Anthropic 的默认文案是 “Not found.”，只有 `GET /v1/models/{id}` 才是 “model: {id}”。生成接口上的 404 应该回显请求的模型名
- [ ] `error.in_stream: true` 时 `error.headers` 只在 Responses 的 `response.failed` 里（`response.error.headers`）出现；Chat 和 Anthropic 的流内错误是 200 的响应，规则里设的头（`x-should-retry`、`retry-after-ms`）没有地方放，被丢掉了
- [ ] `fakellm wire opencode` 的 `shell` 输出把 `opencode.json` 打印了两遍（文件内容一遍，说明片段里一遍）
- [ ] `fakellm wire pi` 没有环境变量也没有 `.toml`，默认格式是 `json`（带 `env`/`files`/`snippet` 的包装对象），想直接得到 `models.json` 要自己用 `jq` 取；可以给 `wire` 加 `--file NAME` 之类的选项
- [ ] 挂起（`hang`）的请求要等客户端放弃才会出现在请求日志里（`status: 0`），测试里没有办法在请求还挂着的时候看到它；`reset` 也不会唤醒已挂起的请求
- [ ] `hang_ms` 与 `delay_ms` / `stream.ttft_ms` 的语义容易混淆（前者不发响应头，后者流式时先发响应头）；考虑在注册时对同时写 `hang_ms` 和 `delay_ms` 的规则给出提示
- [ ] 流中途 TCP 重置：现在 `cut_after_chunks` 是干净地结束 chunked body，客户端看到的是“流提前结束”，不是 `ECONNRESET`；两种在 SDK 里的表现不同，需要补一个能直接 RST 的选项（比如 `stream.reset_after_chunks`）
- [ ] `docs/m1-spec.md` 第 6.1 节还写着 Anthropic 的消息角色只接受 `user` 和 `assistant`，实际代码为了兼容 Claude Code 也接受 `system`（见 `src/anthropic/messages.rs`）；这份内部文档需要同步

## 真实 harness 的 e2e（`e2e/*.sh`，手工运行）

- [ ] 三个脚本都不在 `cargo test` 和 CI 里（需要机器上有 codex / claude / opencode）；想办法在 CI 里按需运行，或者至少在发版前跑一遍
- [ ] `e2e/claude.sh` 总是用 `target/debug/fakellm`，不读 `FAKELLM_BIN`，另外两个读
- [ ] Claude Code 只测了一轮纯文本；没有用 `tool_calls` 规则测它的工具循环（它发 Bash、Read、Edit 等十几个工具），也没测 `claude` 在 4xx / 5xx / 529 / 流内错误下的重试行为
- [ ] codex 只测了纯文本和 `exec_command` 工具调用；没有用真实 codex 测 `response.failed`（`context_length_exceeded`、`insufficient_quota`、`rate_limit_exceeded` + `error.headers`）、`response.incomplete`、`apply_patch`（`custom_tool_call`）、挂起超过 `stream_idle_timeout_ms`
- [ ] opencode 的 e2e 第一次运行曾卡死 120 秒（请求日志为空），之后都在约 3 秒完成，没能复现，原因未查明；`OPENCODE_CONFIG_CONTENT`（`wire opencode` 的环境变量形式）没有被 e2e 覆盖，脚本用的是 `OPENCODE_CONFIG` 指向的文件
- [ ] pi 没有 e2e：`wire pi` 的 `models.json` 格式是按约定写的，没有对照真实的 pi 验证过
- [ ] Claude Code 启动时会 `HEAD /api/hello`（连通性检查，只在对着桩服务时观察到过）；对 fakellm 是 404 `Invalid URL`，当前不影响运行，但没有专门验证

## 测试覆盖缺口

- [ ] `tests/node_sdk.rs` 在没有 `node_modules` 时会打印 skip 并跳过（`sdk/node` 有 openai、openai-responses、anthropic、ai-sdk、ai-sdk-responses、ai-sdk-anthropic 六个套件），CI 之外很容易忘了跑；Python 的 `openai` 和 `anthropic` 客户端还没有覆盖（`wire` 打印的 Python 片段只是照 SDK 文档写的）
- [ ] 故障注入没有用官方 SDK 逐类验证：`x-should-retry`、`retry-after-ms` 对 SDK 重试行为的影响，Anthropic 的流内 `error` 事件在 `@anthropic-ai/sdk` 里的表现

## 路线图（设计见 docs/design.md）

- [ ] M2：napi-rs 绑定（Node 和 Bun，Bun 的兼容性单独在 CI 里测）
- [ ] M2：GHCR 镜像（linux/amd64 + arm64，静态 musl），默认监听 `0.0.0.0:4010`，文档里补 testcontainers 用法
- [ ] M2：Go helper（下载并启动二进制、等就绪信号、控制面客户端）
- [ ] M2：e2e 进 CI，补 paseo、pi 和故障场景
- [ ] M3：fake agent：轨迹回放、工具映射、txiki.js 策略脚本（codex 的测试里有 `validate_request_body_invariants`：每个 `function_call` 都有同 `call_id` 的 `function_call_output`，轨迹模式可以做同样的校验）
- [ ] M4：cassette 录制回放（`--upstream`）、Hugging Face 数据集导入
- [ ] M4：Embeddings、按 `json_schema` 生成结构化输出
- [ ] Responses 的 WebSocket 传输、`background`、`/cancel`、`/input_items`（codex 默认走 HTTP SSE，暂时不需要）
- [ ] 发布到 crates.io，README 里的 git 依赖改成版本号
- [ ] 控制面支持快照（fakes3 有 `POST /_fakes3/snapshots`），在 `reset` 之外提供“回到某个剧本 + 日志状态”的能力

## 与 fakes3 共用的部分

- [ ] 时钟、请求日志、`ctl` 的框架和 fakes3 重复了一份；设计文档里“拆公共 crate 还是各自维护”还没决定
