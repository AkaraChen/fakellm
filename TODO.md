# TODO

## 已知问题（读代码、测试和真实 harness 冒烟时发现的）

- [ ] `--scripts` 没有热加载；`ctl scripts put` 是唯一的运行时入口
- [ ] `fakellm wire opencode` 的 `shell` 输出把 `opencode.json` 打印了两遍（文件内容一遍，说明片段里一遍）
- [ ] `hang_ms` 与 `delay_ms` / `stream.ttft_ms` 的语义容易混淆（前者不发响应头，后者流式时先发响应头）；考虑在注册时对同时写 `hang_ms` 和 `delay_ms` 的规则给出提示
- [ ] `docs/m1-spec.md` 第 6.1 节还写着 Anthropic 的消息角色只接受 `user` 和 `assistant`，实际代码为了兼容 Claude Code 也接受 `system`（见 `src/anthropic/messages.rs`）；这份内部文档需要同步

- [ ] 策略脚本的 `tjs` shim（`src/policy/shim_tjs.js`）是按 txiki.js 的 API 写的，没有在真实的 `tjs` 上跑过（环境里没有）；默认运行时偏偏是 `tjs`，装了它的机器上可能在这里出问题。拿到二进制后先跑一遍 `tests/policy.rs` 的场景
- [ ] `fakellm ctl` 没有 `trajectories` / `policies` 子命令，且 `ctl scripts put` 不能从 stdin 读（`-` 被当成文件名）；现在只能用 curl、`sdk/js`、`bindings/node` 或 Go helper
- [ ] `bindings/node` 的 `policies.put` / `add` 是同步方法，启动并加载策略子进程时阻塞 JS 线程（约 100 ms）；`Server` 没有可持有的句柄，要改成 async 得先改这一层
- [ ] `bindings/node` 的 `npm test` 里轨迹和策略只测了 OpenAI 路径；`sdk/js` 同理。Responses / Anthropic 路径由 `tests/agent.rs`、`tests/policy.rs` 和 `sdk/node` 覆盖
- [ ] 官方 Docker 镜像（alpine）里没有 JS 运行时，策略脚本在里面跑不起来；要么提供带 node 的变体镜像，要么在 `--policy` 时给出明确的启动错误（现在是“no JavaScript runtime found”）
- [ ] `cursor: by_hits` 的轨迹会被重试推进（SDK 的 5xx 自动重试也算），不能和故障注入混用；考虑按请求内容去重，或只在成功回答后推进
- [ ] 策略脚本只支持单文件（没有相对 import），源码只能通过控制面或 `--policy FILE` 给出；需要多文件时再考虑把整个目录写进临时目录
- [ ] `docs/m3-spec.md` 是内部实现规格，`go/README.md` 和部分代码注释链接到它；M3 稳定后与 `docs/m1-spec.md` 一起整理或删除，并把第 6 节里 D5/D7 已完成的描述标成历史
- [ ] `docs/m3-spec.md` 的 D6（`reset_after_chunks` / `reset` 用 `SO_LINGER(0)` 发真正的 RST）没有实现，`tests/chores.rs` 也不存在；README 和 `docs/scripts.md` 仍然写“以 FIN 关闭，不是 RST”，与现状一致。见下面“消费方实验”的对应条目
- [ ] 手工运行 `e2e/codex-trajectory.sh`、`e2e/claude-trajectory.sh`、`e2e/opencode-policy.sh` 的最近一次结果没有记录；发版前跑一遍，并把结果写进这里

## 真实 harness 的 e2e（`e2e/*.sh`，手工运行）

- [ ] 真实 harness 的 e2e 脚本（claude / codex / opencode 及其 trajectory、policy、faults 变体）都不在 `cargo test` 和 CI 里（需要机器上有 codex / claude / opencode）；想办法在 CI 里按需运行，或者至少在发版前跑一遍
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
- [ ] M4：`fakellm import hf`（SWE-agent 等轨迹数据集转成 `docs/agent.md` 的 JSONL 格式）、cassette 录制回放（`--upstream`）、Hugging Face 数据集导入
- [ ] M4：按 `json_schema` 生成结构化输出
- [ ] Responses 的 WebSocket 传输、`background`、`/cancel`、`/input_items`（codex 默认走 HTTP SSE，暂时不需要）
- [ ] 发布到 crates.io，README 里的 git 依赖改成版本号
- [ ] 控制面支持快照（fakes3 有 `POST /_fakes3/snapshots`），在 `reset` 之外提供“回到某个剧本 + 日志状态”的能力

## 文档与一致性（M2 文档核对时发现）

- [ ] `docs/research.md`、`docs/m1-spec.md` 里的 codex 配置还写着 `env_key` 必须设置；`wire codex` 现在把它注释掉了（见 `docs/wire.md`）

## 与 fakes3 共用的部分

- [ ] 时钟、请求日志、`ctl` 的框架和 fakes3 重复了一份；设计文档里“拆公共 crate 还是各自维护”还没决定

## 消费方实验

来自 `docs/consumers.md`：六个真实仓库（xsai、pi-mono、claude-agent-sdk-python、opencode、raft、oar）对着 fakellm 跑测试得到的缺口，按需要它的仓库数排序。

- [ ] `POST /v1/systemone`（xsai，Ollama 专有；两个 decide 用例在 404 处失败，响应格式未知，要先确认是否值得做）
- [ ] 任意谓词或 turn 范围匹配（oar：`currentTurnSays`、`request.tools.length==0`，14 个 `match.predicate` 失败）
- [ ] 函数式/动态回复（oar：`echoFixtures`/`scriptEchoes`，6 个失败）
- [ ] 正则环视（oar：Rust regex 不支持 lookahead；评估是否在文档里明确说明，或用 `fancy-regex`）
- [ ] `reset_after_chunks` 以 FIN 而不是 TCP RST 关闭连接，客户端看到 `terminated` / `unexpected EOF` 而不是 `ECONNRESET`（pi-mono、raft 的断言可能要 RST）；需要 `SO_LINGER(0)` 才能真正发 RST
- [ ] 各仓库的实验分支（`fakellm-research/*` 的 `fakellm`）都只在本地；挑选通用的 helper（spawn、读首行 URL、reset、rules、requests）做成 SDK 里的官方 helper
- [ ] 第二轮（paseo、ai-sdk、oar 重跑）：Anthropic 的 `input_tokens` 出现在 `message_start` 还是 `message_delta` 不可控（paseo：Claude `start`/`delta` 用例变成同一 wire 形状）；需要规则级开关
- [ ] 与真实 API 的响应保真差异（ai-sdk 夹具对照）：Anthropic usage 缺 `cache_creation`、`service_tier`、`inference_geo`；`model` 回显请求别名而不是带日期的 id；`message_start` 的 `output_tokens` 为 1；`message_delta` 缺 `context_management`；`tool-no-args` 多发一个 `{}` 的 `tool-input-delta`；Responses 的 `encrypted_content` 在 `output_item.added` 与 `.done` 上相同（真实 API 不同）
- [ ] 跨请求的序列/状态匹配原语（paseo：guardian 请求与主请求只靠 `times:1` 的顺序区分；oar：18 个 `match.predicate` 失败）；和上面的「任意谓词」是同一族问题，先定语义再实现
- [ ] 诊断 oar 的三个未定位失败：codex `failure-classes`（unknown model 结果多一个字段，疑为 `status`）、codex `service-tier`（`expected 'default' to be 'priority'`）、claude `claude-partials` "native background child"（`expected false to be true`），先确认是不是 fakellm 的问题
- [ ] 验证 `POST /v1/embeddings`：在 xsai 上重跑 embed 的两个用例（`embedding.length > 0`、`usage.inputTokens > 0`、`totalTokens >= inputTokens`、不同输入向量不同、同批长度一致），通过后这一条删掉；官方 `openai` SDK 的 `embeddings.create` 已在 `sdk/node` 里测过
