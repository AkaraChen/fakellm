# TODO

## 已知问题（读代码、测试和真实 harness 冒烟时发现的）

- [ ] `--scripts` 没有热加载；`ctl scripts put` 是唯一的运行时入口
- [ ] `fakellm wire opencode` 的 `shell` 输出把 `opencode.json` 打印了两遍（文件内容一遍，说明片段里一遍）
- [ ] `hang_ms` 与 `delay_ms` / `stream.ttft_ms` 的语义容易混淆（前者不发响应头，后者流式时先发响应头）；考虑在注册时对同时写 `hang_ms` 和 `delay_ms` 的规则给出提示
- [ ] `docs/m1-spec.md` 第 6.1 节还写着 Anthropic 的消息角色只接受 `user` 和 `assistant`，实际代码为了兼容 Claude Code 也接受 `system`（见 `src/anthropic/messages.rs`）；这份内部文档需要同步

## 真实 harness 的 e2e（`e2e/*.sh`，手工运行）

- [ ] 三个脚本都不在 `cargo test` 和 CI 里（需要机器上有 codex / claude / opencode）；想办法在 CI 里按需运行，或者至少在发版前跑一遍
- [ ] `e2e/claude.sh` 总是用 `target/debug/fakellm`，不读 `FAKELLM_BIN`，另外两个读
- [ ] Claude Code 只测了一轮纯文本；没有用 `tool_calls` 规则测它的工具循环（它发 Bash、Read、Edit 等十几个工具），也没测 `claude` 在 4xx / 5xx / 529 / 流内错误下的重试行为
- [ ] codex 只测了纯文本、`exec_command` 工具调用和 `context_length_exceeded`（`e2e/codex-faults.sh`）；没有用真实 codex 测 `response.failed`（`insufficient_quota`、`rate_limit_exceeded` + `error.headers`）、`response.incomplete`、`apply_patch`（`custom_tool_call`）、挂起超过 `stream_idle_timeout_ms`
- [ ] opencode 的 e2e 第一次运行曾卡死 120 秒（请求日志为空），之后都在约 3 秒完成，没能复现，原因未查明；`OPENCODE_CONFIG_CONTENT`（`wire opencode` 的环境变量形式）没有被 e2e 覆盖，脚本用的是 `OPENCODE_CONFIG` 指向的文件
- [ ] pi 没有 e2e：`wire pi` 的 `models.json` 格式是按约定写的，没有对照真实的 pi 验证过
- [ ] Claude Code 启动时会 `HEAD /api/hello`（连通性检查，只在对着桩服务时观察到过）；对 fakellm 是 404 `Invalid URL`，当前不影响运行，但没有专门验证

## 测试覆盖缺口

- [ ] `tests/node_sdk.rs` 在没有 `node_modules` 时会打印 skip 并跳过；`sdk/node` 的测试还依赖 `sdk/js/dist`，`npm test` 的 `pretest` 在缺少时会自动构建（`npm --prefix ../js ci && build`，需要网络）（`sdk/node` 有 openai、openai-responses、anthropic、ai-sdk、ai-sdk-responses、ai-sdk-anthropic 六个套件），CI 之外很容易忘了跑；Python 的 `openai` 和 `anthropic` 客户端还没有覆盖（`wire` 打印的 Python 片段只是照 SDK 文档写的）
- [ ] 故障注入没有用官方 SDK 逐类验证：`x-should-retry`、`retry-after-ms` 对 SDK 重试行为的影响，Anthropic 的流内 `error` 事件在 `@anthropic-ai/sdk` 里的表现

## 路线图（设计见 docs/design.md）

- [ ] M2：第一次推 `v*` tag，真正发布 GHCR 镜像、GitHub release 二进制（`go/` 的 `Download` 依赖它）和 npm 包（`@fakellm/native`、`sdk/js`）；之后把 README 里 testcontainers 片段在真实镜像上跑一遍（现在没有验证过）
- [ ] M2：`bindings/node` 只构建了 Linux；macOS / Windows 没有；Bun 下只跑了 `openai.test.mjs`
- [ ] M2：e2e 进 CI，补 paseo 和 pi（codex 的故障场景有 `e2e/codex-faults.sh`，手工运行）
- [ ] M3：fake agent：轨迹回放、工具映射、txiki.js 策略脚本（codex 的测试里有 `validate_request_body_invariants`：每个 `function_call` 都有同 `call_id` 的 `function_call_output`，轨迹模式可以做同样的校验）
- [ ] M4：cassette 录制回放（`--upstream`）、Hugging Face 数据集导入
- [ ] M4：Embeddings、按 `json_schema` 生成结构化输出
- [ ] Responses 的 WebSocket 传输、`background`、`/cancel`、`/input_items`（codex 默认走 HTTP SSE，暂时不需要）
- [ ] 发布到 crates.io，README 里的 git 依赖改成版本号
- [ ] 控制面支持快照（fakes3 有 `POST /_fakes3/snapshots`），在 `reset` 之外提供“回到某个剧本 + 日志状态”的能力

## 文档与一致性（M2 文档核对时发现）

- [ ] 请求日志的文本输出（`ctl requests list`）不标出 `hung`，只能靠 `--status 0` 或 `--json` 看到
- [ ] `docs/research.md`、`docs/m1-spec.md` 里的 codex 配置还写着 `env_key` 必须设置；`wire codex` 现在把它注释掉了（见 `docs/wire.md`）
- [ ] Anthropic 规则里 `usage.input_tokens` 加 `cached_tokens` 时，响应里的 `input_tokens` 被扣减，而 `count_tokens` 不扣；是有意的还是应该统一，没定

## 与 fakes3 共用的部分

- [ ] 时钟、请求日志、`ctl` 的框架和 fakes3 重复了一份；设计文档里“拆公共 crate 还是各自维护”还没决定

## 消费方实验

来自 `docs/consumers.md`：六个真实仓库（xsai、pi-mono、claude-agent-sdk-python、opencode、raft、oar）对着 fakellm 跑测试得到的缺口，按需要它的仓库数排序。

- [ ] Anthropic 请求里的 `role:system` 消息被 IR 当成 `user`，且 Claude Code 的第一条 user 消息是 `<system-reminder>` 块，导致 `last_user` 匹配不到真实提示词（oar、claude-agent-sdk-python）；IR 应保留 system 角色，`last_user` 只看真实 user 文本
- [ ] `POST /v1/embeddings`（xsai：`embedding.length > 0`、数值有限、`usage.inputTokens > 0`、`totalTokens >= inputTokens`、不同输入向量不同、同批长度一致）
- [ ] `POST /v1/systemone`（xsai，Ollama 专有；两个 decide 用例在 404 处失败，响应格式未知，要先确认是否值得做）
- [ ] 可配置的路由前缀，或匹配任意前缀的 `.../v1/messages`（raft：`/anthropic/v1/messages` 404）
- [ ] 任意谓词或 turn 范围匹配（oar：`currentTurnSays`、`request.tools.length==0`，14 个 `match.predicate` 失败）
- [ ] 函数式/动态回复（oar：`echoFixtures`/`scriptEchoes`，6 个失败）
- [ ] 工具参数流式的分块大小与延迟（oar：`claude-partials` "slow tool arguments" 期望 500 ms 以上，实际 4 ms）
- [ ] 正则环视（oar：Rust regex 不支持 lookahead；评估是否在文档里明确说明，或用 `fancy-regex`）
- [ ] `reset_after_chunks` 以 FIN 而不是 TCP RST 关闭连接，客户端看到 `terminated` / `unexpected EOF` 而不是 `ECONNRESET`（pi-mono、raft 的断言可能要 RST）；需要 `SO_LINGER(0)` 才能真正发 RST
- [ ] 各仓库的实验分支（`fakellm-research/*` 的 `fakellm`）都只在本地；挑选通用的 helper（spawn、读首行 URL、reset、rules、requests）做成 SDK 里的官方 helper
