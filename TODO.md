# TODO

## 已知问题（读代码和测试时发现的）

- [ ] `FAKELLM_STRICT` 只认 `true` / `false`（clap 的 bool 环境变量），`FAKELLM_STRICT=1` 会启动失败；`--ttft`、`--tokens-per-sec`、`--fallback-tokens`、`--no-request-log`、`--request-log-capacity` 没有环境变量，Docker 里只能用命令行
- [ ] `--scripts` 文件读不出来时报 `Error: Os { code: 2 ... }`，没有文件名；解析失败的错误有文件名。`--ttft` 不支持小数（`1.5s`），错误信息里的例子也没提
- [ ] 默认 `--models` 里有 `text-embedding-3-*`，但 `POST /v1/embeddings` 是 404 `Invalid URL`；`GET /v1/models/{id}` 对不在列表里的模型 404，而 chat 接口接受任何模型名，两边行为不一致
- [ ] `fakellm ctl scripts list` 的文本输出是每条规则一行紧凑 JSON，不如 `requests list` 的一行摘要好读（序号、名字、hits/times、匹配条件摘要）
- [ ] 同步响应的 `x-request-id` 和流式响应一致地由请求内容派生，相同请求（比如重试）拿到相同的 `x-request-id`；真实 API 每次都不同。重试断言“两次请求是不同的请求”时会误导。可以混入 `ID_COUNTER`
- [ ] `PUT /_fakellm/scripts` 总是把命中计数清零；想在不丢计数的情况下改规则只能 `POST`。需要的话加 `PATCH` 或 `?keep_hits`
- [ ] `RequestRecord.headers` 里 `authorization` 原样保存。测试服务无所谓，但把日志导出给别人之前要脱敏，加一个 `redact` 开关
- [ ] `Config` 没有 `Delivery` 的 `chunk_tokens` / `args_chunk_bytes` 的命令行参数（只有规则里能改 `chunk_tokens`；`args_chunk_bytes` 固定 16，规则里也改不了）
- [ ] `--scripts` 没有热加载；`ctl scripts put` 是唯一的运行时入口

## 测试覆盖缺口

- [ ] `tests/` 和 `sdk/node` 里没有 HTTP 层的测试覆盖 `tool_result_contains`、`any_message`、`turn`、`system` + `no_tools` 的组合（文档里的“标题旁路”例子是手工验证的），应该把 `docs/scripts.md` 的三个例子做成集成测试
- [ ] `cut_after_chunks` 没有用官方 SDK 验证客户端看到的是 `interrupted` / 流提前结束（pi、opencode 要测的故障）
- [ ] `n > 1` 的流式输出没有用官方客户端验证
- [ ] 虚拟时钟下的 `retry-after` 与 SDK 重试的交互没有测试（SDK 退避是真实定时器，见 README 的边界一节）；至少补一个 `maxRetries: 0` 的断言
- [ ] `tests/node_sdk.rs` 在没有 `node_modules` 时静默跳过，CI 之外很容易忘了跑；Python `openai` 客户端还没有覆盖
- [ ] 请求日志容量缩小、`enabled: false` 之后再打开、`since` / `after` 组合过滤没有 HTTP 层测试

## 路线图（设计见 docs/design.md）

- [ ] M1：Anthropic Messages 和 Responses API，复用同一套剧本和 IR（`ir::Protocol` 目前只有 `OpenaiChat` 一个变体）。ai-sdk 的 `openai('gpt-4o')` 默认走 Responses，现在会得到 404，README 里要求用 `.chat()`
- [ ] M1：故障注入：按概率、固定或随机延迟、流中途 TCP 重置（现在 `cut_after_chunks` 是干净结束）、`x-should-retry` 头、`retry-after-ms`
- [ ] M1：`fakellm wire` helper
- [ ] M2：napi-rs 绑定（Node 和 Bun，Bun 的兼容性单独在 CI 里测）
- [ ] M2：GHCR 镜像（linux/amd64 + arm64，静态 musl），默认监听 `0.0.0.0:4010`，文档里补 testcontainers 用法
- [ ] M2：Go helper（下载并启动二进制、等就绪信号、控制面客户端）
- [ ] M2：真实 harness 的 e2e（paseo、pi、opencode 等指向 fakellm 跑一遍）
- [ ] M3：fake agent：轨迹回放、工具映射、txiki.js 策略脚本
- [ ] M4：cassette 录制回放（`--upstream`）、Hugging Face 数据集导入
- [ ] M4：Embeddings、按 `json_schema` 生成结构化输出
- [ ] 发布到 crates.io，README 里的 git 依赖改成版本号
- [ ] 控制面支持快照（fakes3 有 `POST /_fakes3/snapshots`），在 `reset` 之外提供“回到某个剧本 + 日志状态”的能力

## 与 fakes3 共用的部分

- [ ] 时钟、请求日志、`ctl` 的框架和 fakes3 重复了一份；设计文档里“拆公共 crate 还是各自维护”还没决定
