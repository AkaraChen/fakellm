# TODO

分三组：已知差异（现状和真实行为不一致、或有已知的坑，暂时接受）、想做的（功能、验证、发布、整理）、消费方缺口（来自 [docs/consumers.md](docs/consumers.md) 的真实仓库实验）。每条尽量只出现一次；已经做完的不留。

## 已知差异

与真实 API 的差异：

- [ ] 响应保真（ai-sdk 夹具对照）：Anthropic usage 缺 `cache_creation`、`service_tier`、`inference_geo`；`model` 回显请求别名而不是带日期的 id；`message_start` 的 `output_tokens` 为 1；`message_delta` 缺 `context_management`；`tool-no-args` 多发一个 `{}` 的 `tool-input-delta`；Responses 的 `encrypted_content` 在 `output_item.added` 与 `.done` 上相同（真实 API 不同）；每个 delta 一个分词 token，块数多于真实 API
- [ ] Anthropic 的缓存读取与 `input_tokens` 耦合：规则的 `usage.input_tokens` 要包含缓存读取，响应里被扣减，`count_tokens` 不扣所以返回被抬高的值（paseo、ai-sdk 都踩到）；`input_tokens` 出现在 `message_start` 还是 `message_delta` 也不可控，需要规则级开关
- [ ] 正则不支持环视（Rust regex 没有 lookahead）；评估是在文档里明确说明，还是换 `fancy-regex`（oar）
- [ ] cassette 的 `fuzzy` 只做“末尾消息相同”的近似选择，没有真正的相似度
- [ ] Responses 的 WebSocket 传输、`background`、`/cancel`、`/input_items` 没做（codex 默认走 HTTP SSE，暂时不需要）

已知的坑：

- [ ] `hang_ms` 与 `delay_ms` / `stream.ttft_ms` 的语义容易混淆（前者不发响应头，后者流式时先发响应头）；考虑在注册时对同时写 `hang_ms` 和 `delay_ms` 的规则给出提示
- [ ] `cursor: by_hits` 的轨迹会被重试推进（SDK 的 5xx 自动重试也算），不能和故障注入混用；考虑按请求内容去重，或只在成功回答后推进
- [ ] 策略脚本只支持单文件（没有相对 import），源码只能通过控制面或 `--policy FILE` 给出；需要多文件时再考虑把整个目录写进临时目录
- [ ] 官方 Docker 镜像（alpine）里没有 JS 运行时，策略脚本跑不起来；要么提供带 node 的变体镜像，要么在 `--policy` 时给出明确的启动错误（现在是 “no JavaScript runtime found”）
- [ ] `bindings/node` 的 `policies.put` / `add` 是同步方法，启动并加载策略子进程时阻塞 JS 线程（约 100 ms）；`Server` 没有可持有的句柄，要改成 async 得先改这一层
- [ ] `bindings/node` 的 `watch` 默认什么都不监视，`sdk/js` 和 CLI 默认监视，三者行为不一致
- [ ] 快照还原策略时总会重启子进程；快照数量没有上限；快照里的 cassette 共享而不复制，但每个快照仍占内存
- [ ] 录制只脱敏请求里名字像凭证的 `extra` 字段，消息文本、工具参数和结果原样保存（见 `docs/cassette.md#脱敏`）；考虑给 `--record` 加一个可配置的正则脱敏
- [ ] 导入的 cassette 只能按请求内容精确回放，不能按 seed 从语料里抽一条回复（`docs/design.md` 数据集回放的第一点）
- [ ] Claude Code 启动时会 `HEAD /api/hello`（连通性检查，只在对着桩服务时观察到过）；对 fakellm 是 404 `Invalid URL`，当前不影响运行，但没有专门验证
- [ ] `fakellm wire opencode` 的 `shell` 输出把 `opencode.json` 打印了两遍（文件内容一遍，说明片段里一遍）
- [ ] `fakellm ctl` 没有 `trajectories` / `policies` 子命令，且 `ctl scripts put` 不能从 stdin 读（`-` 被当成文件名）；`ctl --help` 的简介还写着 “reset, scripts, clock, request log”，没提 cassettes 和 snapshot；现在 trajectories 和 policies 只能用 curl、`sdk/js`、`bindings/node` 或 Go helper

## 想做的

发布：

- [ ] 第一次推 `v*` tag，真正发布 GHCR 镜像、GitHub release 二进制（`go/` 的 `Download` 依赖它）和 npm 包（`@fakellm/native`、`sdk/js`）；之后把 README 里 testcontainers 片段在真实镜像上跑一遍（现在没有验证过）
- [ ] 发布到 crates.io，README 里的 git 依赖改成版本号
- [ ] `bindings/node` 只构建了 Linux；macOS / Windows 没有；Bun 下只跑了 `openai.test.mjs`
- [ ] Docker 镜像加了 `hyper-rustls`（`ring`、`webpki-roots`）用于 `https://` 上游，alpine 镜像里的构建没有验证过（`e2e/docker.sh` 只验证了启动和一次 chat completion）

真实 harness 与上游的验证：

- [ ] e2e 脚本（claude / codex / opencode 及其 trajectory、policy、faults 变体）都不在 `cargo test` 和 CI 里（需要机器上有这些 CLI）；想办法在 CI 里按需运行，或者至少在发版前跑一遍（最近一次的结果见 README 的“测试与验证”）
- [ ] Claude Code 只测了一轮纯文本；没有用 `tool_calls` 规则测它的工具循环（它发 Bash、Read、Edit 等十几个工具），也没测 `claude` 在 4xx / 5xx / 529 / 流内错误下的重试行为
- [ ] codex 只测了纯文本、`exec_command` 工具调用和 `context_length_exceeded`（`e2e/codex-faults.sh`）；没有用真实 codex 测 `response.failed`（`insufficient_quota`、`rate_limit_exceeded` + `error.headers`）、`response.incomplete`、`apply_patch`（`custom_tool_call`）、挂起超过 `stream_idle_timeout_ms`
- [ ] opencode 的 e2e 第一次运行曾卡死 120 秒（请求日志为空），之后都在约 3 秒完成，没能复现，原因未查明；`OPENCODE_CONFIG_CONTENT`（`wire opencode` 的环境变量形式）没有被 e2e 覆盖，脚本用的是 `OPENCODE_CONFIG` 指向的文件
- [ ] pi 没有 e2e：`wire pi` 的 `models.json` 格式是按约定写的，没有对照真实的 pi 验证过
- [ ] 策略脚本的 `tjs` shim（`src/policy/shim_tjs.js`）是按 txiki.js 的 API 写的，没有在真实的 `tjs` 上跑过（环境里没有）；默认运行时偏偏是 `tjs`，装了它的机器上可能在这里出问题。拿到二进制后先跑一遍 `tests/policy.rs` 的场景
- [ ] 上游代理只在对着另一个 fakellm 时测过；对着真实 OpenAI / Anthropic / CLIProxyAPI（`FAKELLM_UPSTREAM_TEST`）的录制与回放，尤其是 Claude Code 和 codex 的整段会话，没有在本机验证；`docs/cassette.md` 里 CLIProxyAPI 的默认地址和密钥来自其文档，没有实测
- [ ] `fakellm import` 的转换器是照数据集已知的列名写的，只对着合成夹具（`tests/fixtures/hf/`）和假的 datasets-server 测过；真实数据集（WildChat、nebius/SWE-agent-trajectories、hermes、OpenHands）用 `FAKELLM_HF_TEST=1` 手工跑一遍，看 `skipped` 的行数，把真实形状补进夹具
- [ ] `xlam-function-calling-60k` 在 `docs/design.md` 的候选里，但没有转换器；`ShareGPT_Vicuna_unfiltered` 的 `from` 取值也没有对着真实数据确认

测试覆盖：

- [ ] RST（`reset_after_chunks` / `reset`，`src/lib.rs` 里 `SO_LINGER(0)`，非 unix 退回 FIN）只有 `tests/faults.rs` 断言“没有结尾 chunk”，没有断言 RST 本身；补一个用裸 socket 读 `ECONNRESET` 的测试
- [ ] `tests/node_sdk.rs` 在没有 `node_modules` 时会打印 skip 并跳过；`sdk/node` 的测试还依赖 `sdk/js/dist`，`npm test` 的 `pretest` 在缺少时会自动构建（`npm --prefix ../js ci && build`，需要网络），CI 之外很容易忘了跑
- [ ] Python 的 `openai` 和 `anthropic` 客户端还没有覆盖（`wire` 打印的 Python 片段只是照 SDK 文档写的）
- [ ] 故障注入没有用官方 SDK 逐类验证：`x-should-retry`、`retry-after-ms` 对 SDK 重试行为的影响，Anthropic 的流内 `error` 事件在 `@anthropic-ai/sdk` 里的表现
- [ ] 轨迹、策略和自定义端点的覆盖只在 OpenAI 路径上：`bindings/node` 的 `npm test` 和 `sdk/js` 的测试只测 OpenAI 路径（Responses / Anthropic 由 `tests/agent.rs`、`tests/policy.rs` 和 `sdk/node` 覆盖）；pi-mono 的 agent 测试只用 openai-completions；bun / tjs 运行时、`args_chunk_bytes`、`last_user_raw`、路径前缀容忍没有专门测试

整理：

- [ ] `docs/m1-spec.md`、`docs/m3-spec.md`、`docs/m4-spec.md` 是内部实现规格，`go/README.md` 和部分代码注释链接到 m3；稳定后整理或删除，并把 m3 第 6 节里 D5/D6/D7 已完成的描述标成历史；`docs/m4-spec.md` 第 13 节写的是 `docs/cassettes.md`，实际文件名是 `docs/cassette.md`
- [ ] `docs/design.md` 的里程碑表（M2 含 cassette，M3 含 import）与实际交付不一致（cassette、import 在 M4），“cassette 的语义匹配规则怎么设计”这个开放问题已经有答案（`docs/cassette.md`）
- [ ] `docs/scripts.md` 的 `match.protocol` 取值列表没有 `custom`
- [ ] 时钟、请求日志、`ctl` 的框架和 fakes3 重复了一份；设计文档里“拆公共 crate 还是各自维护”还没决定

## 消费方缺口

来自 [docs/consumers.md](docs/consumers.md)：八个真实仓库对着 fakellm 跑测试得到的缺口。第三轮和补充一轮已经解决的（`reply.usage`、embeddings、路径前缀、`args_chunk_bytes`、真正的 RST、谓词与函数式回复、自定义端点）不在这里。

- [ ] oar 适配器改用策略的 `decline`：现在为每个动态夹具起一个 node 子进程（约 100 ms），并在回调里用 JS 重写静态匹配器、自己返回 404；`decline` 已经能让策略落到后面的规则（见 `docs/agent.md`），换上后重跑 oar，看子进程数和失败数
- [ ] 跨请求的序列/状态匹配原语（paseo：guardian 请求与主请求只靠 `times:1` 的顺序区分，codex 一调整调用顺序就失效）；先定语义再实现
- [ ] oar 的三个未解决失败：claude-partials “native background child”（原因已查清：Anthropic 适配用 `any_message` 命中了历史里的 Task tool_use 参数；用 M3 的 `last_user` 重试，看是否修好且不让 Claude 其他用例回退）；codex `failure-classes`（unknown model 结果多一个字段，疑为 `status`）；codex `service-tier`（`expected 'default' to be 'priority'`）。后两个在 aimock 基线上就失败，先确认是不是 fakellm 的问题
- [ ] 验证 CONNECT 代理转发 `reset_after_chunks` 的 RST 还是转成 FIN（二进制本身发 RST；经代理和 pi-ai 看到的都是 `terminated`，区分不了）
- [ ] 各仓库的实验分支（`$RESEARCH_DIR/*` 的 `fakellm`）都只在本地；挑选通用的 helper（spawn、读首行 URL、reset、rules、requests）做成 SDK 里的官方 helper
