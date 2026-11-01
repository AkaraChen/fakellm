# 更新记录

按里程碑记录，新的在前。版本号还是 `0.1.0`，还没有推过 `v*` tag，所以没有正式发布；日期是提交日期。设计思路见 [docs/design.md](docs/design.md)，待办见 [TODO.md](TODO.md)。

## 未提交（M4 之后）

M4 之后的补充，来自 [docs/consumers.md](docs/consumers.md) 里的消费方缺口。

- 自定义端点：规则里写 `match.path`（路径 glob）和 `reply.body`（原样返回的 JSON，可加 `status`、`headers`）就能回答非标准路径，如 xsai 的 `POST /v1/systemone`；`match.body` 用正则匹配原始请求正文。请求日志里 `protocol` 为 `custom`，`ctl` 能显示。见 [docs/scripts.md](docs/scripts.md#自定义端点)。
- 策略脚本可以放弃（decline）：`decide` 返回 `null`、`undefined` 或 `{decline: true}` 时，这条规则当作没匹配上，请求落到后面的规则。见 [docs/agent.md](docs/agent.md#策略脚本)。
- 启动错误：规则不合法、文件读不出来、端口被占用时，打印 `fakellm: <原因>` 并以退出码 1 结束。
- `sdk/js`、`bindings/node`、`go/` 补上自定义端点的类型和测试。
- 加了 MIT `LICENSE`（仓库根、`sdk/js`、`bindings/node`），`sdk/js` 补了 `repository`、`bugs`、`homepage`。
- 文档：README 补上快速开始里的环境变量、模型数量、测试与验证一节；`docs/consumers.md` 整理成总览加复现加结论；TODO 去重分组。

## M4：cassette、上游代理、导入、快照、热加载（2026-11-01）

- **cassette**：录好的请求/回复交换存成 IR，按请求的含义回放（忽略采样参数，规范化时间戳、UUID、工具调用 id；`once`、`fuzzy`、`model_glob`）；用一个协议录的，另外两个协议也能命中；`fakellm cassette to-trajectory` 把录好的会话转成轨迹；`/_fakellm/cassettes`。
- **上游代理**：`--upstream`、`--upstream-api-key`、`--record`、`--record-errors`、`--upstream-timeout`。原样转发 Chat、Responses、Messages 和 Embeddings，把 SSE 还原成 IR 写进 cassette，脱敏凭证，用 `x-fakellm-hop` 防回环；查找顺序是剧本 > cassette > 上游 > 兜底。
- **导入**：`fakellm import hf|file`，datasets-server 客户端（分页、`HF_TOKEN`），转换 ShareGPT、WildChat、Hermes function calling、SWE-agent 和 OpenHands 轨迹；用合成夹具测，真实数据集的联网测试用 `FAKELLM_HF_TEST=1` 手工跑。
- **结构化输出**：`json_schema` 响应格式和被强制调用的工具，兜底回复是符合 schema 的确定性实例。
- **快照**：`/_fakellm/snapshots` 保存和还原剧本、轨迹、策略、cassette、Responses 存储和时钟。
- **热加载**：`--scripts`、`--trajectory`、`--policy`、`--cassette` 的文件变了自动重读，`--no-watch` 关闭；控制面的管理操作串行化。
- `sdk/js`、`bindings/node`、`go/`、`ctl` 都有对应的 `cassettes` / `snapshots`；新增 `docs/cassette.md`、`docs/import.md`；`cargo publish --dry-run` 通过。

## M3：fake agent、embeddings、路由整理（2026-11-01）

- **轨迹（trajectory）**：与协议无关的多轮回放（JSON、YAML、JSONL），按助手回合或命中次数选步骤；`expect` 检查工具结果，偏离时 `strict` / `lenient` / `stop`；`tool_map` 改写工具名和参数；`unknown_tool` 三种处理；`/_fakellm/trajectories`，规则里用 `reply.trajectory` 引用。
- **策略脚本（policy）**：JS 模块 `decide(request, context)` 通过 JSONL 标准输入输出协议返回回复，带按策略保存的状态、真实时钟的超时、崩溃重启、加载时试跑；运行时 node 和 bun 有测试，txiki.js（`tjs`）的 shim 没在真实环境验证；`/_fakellm/policies`，规则里用 `reply.policy` 引用。
- **`POST /v1/embeddings`**：确定性单位向量，`float` 和 `base64`。
- 整理：`last_user` 忽略 `<system-reminder>`（`last_user_raw` 取原文）；路径前缀容忍（`/anthropic/v1/messages`）；每条规则的 `stream.args_chunk_bytes`；Anthropic 的 `usage` 可以按字面覆盖；`SO_LINGER(0)` 让 `reset` 类故障真的发 RST；SIGINT / SIGTERM 正常退出；`ctl` 里标出挂起的请求。
- `sdk/js`、`bindings/node`、`go/` 补上 `trajectories` / `policies`；`e2e/codex-trajectory.sh`、`claude-trajectory.sh`、`opencode-policy.sh` 用真实二进制通过；新增 `docs/agent.md`。

## M2：SDK、原生绑定、Docker（2026-11-01）

- **`sdk/js`**：纯 TypeScript 包 `fakellm`，启动二进制并包装控制面（`scripts`、`requests`、`clock`、`responses`、`wire`）；`sdk/node` 的套件改用它。
- **`bindings/node`**：napi-rs 插件 `@fakellm/native`，服务器在 Node / Bun 进程内运行，不起子进程；`node.yml` 的 CI 矩阵。
- **`go/`**：只用标准库的 Go helper（启动与停止、控制面客户端、带缓存的 release 下载）。
- **Docker 与发布**：基于 Alpine 的静态 musl 镜像（约 6 MB），`release.yml`（发布受 `publish` 开关控制），`e2e/docker.sh`。
- 来自消费方实验的整理：`reply.usage` 覆盖（也驱动 `count_tokens`）、`match.last_tool_call_id`、流内错误头、404 回显请求的模型名、挂起的请求进日志（`status: 0`、`hung: true`）并被 `reset` 唤醒、`wire --file` 和可选的 codex `env_key`、流中途连接中断（`reset_after_chunks`、`reply.reset`）、`Server` 关闭时中止连接。
- 新增 `docs/consumers.md`：xsai、pi-mono、claude-agent-sdk-python、opencode、raft、oar 的测试对着 fakellm 跑的结果。

## M1：Responses、Anthropic Messages、故障注入、wire（2026-10-31）

- **Anthropic Messages**：`/v1/messages` 同步和 SSE（`thinking`、`tool_use` 块），`count_tokens`，Anthropic 格式的模型列表和错误体。
- **OpenAI Responses**：`/v1/responses` 同步和 SSE（`sequence_number`、`function_call` 和推理项、`response.incomplete` / `response.failed`），内存存储与 `previous_response_id`，`GET` / `DELETE`。
- 三个协议共用同一个 SSE 写入器（`src/sse.rs`），都跟着服务端时钟走。
- **故障注入**：`protocol`、`probability`、`every_nth`、`hang` / `hang_ms`、`error.in_stream`、`error.headers`、`reasoning_opaque`，`PUT` 加 `keep_hits`；按协议的错误体。
- **`fakellm wire`**：`claude`、`codex`、`opencode`、`pi` 等的接线配置；`e2e/` 脚本用真实的 claude、codex、opencode 跑通。
- 整理：所有参数都有环境变量、小数时长、`--redact-auth`、`--chunk-tokens` / `--args-chunk-bytes`、每个请求的 `x-request-id`、`ctl` 摘要。
- 测试：anthropic、responses、faults、scripts_http、wire；`sdk/node` 加 `@anthropic-ai/sdk`、`@ai-sdk/anthropic` 和 Responses 套件。

## M0：Chat Completions（2026-10-31）

- OpenAI Chat Completions 同步和 SSE，分块方式贴近真实 API（role chunk、逐 token 内容、工具调用参数分片、finish chunk、`choices: []` 的 usage chunk、`[DONE]`）；`GET /v1/models`。
- 剧本：模型 glob、正则、有状态的 tool 结果条件、`times`；确定性兜底；`--strict`；带 `retry-after` 的 OpenAI 错误体。
- 请求日志（原始正文加解析后的 IR）；服务端虚拟时钟，流式节奏跟着它走，测试用 `advance` 代替等待。
- 控制面 `/_fakellm/*` 与 `fakellm ctl`；Rust 库 `Server::start` 和 `BlockingServer`。
- 用官方 `openai` Node SDK 和 Vercel AI SDK 验证（`sdk/node`，装了 node_modules 时 `cargo test` 也会跑，Bun 下同样通过）。
