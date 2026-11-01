# TODO

这里只放两类东西：已知差异（有意接受的、和真实 API 或理想状态不一致的地方，每条写明原因），以及需要作者决定的事（agent 不能做、或缺外部条件的动作，每条写明试过什么）。其余都已经实现并有测试，不留在这里；交付历史见 [CHANGELOG.md](CHANGELOG.md)。

## 已知差异

- **Responses 的 WebSocket、`background`、`/cancel`、`/input_items`**：返回 404 `Invalid URL`。codex、ai-sdk、openai-python、openai-node 的非后台调用都不用它们，八个真实仓库的实验里没有消费方用到；需要时用自定义端点规则伪造。原因见 [docs/control-plane.md](docs/control-plane.md)。
- **响应保真的有意差异**：带日期的模型 id、`message_start` 里的 `output_tokens`、分片粒度等不与真实服务逐字节一致，清单和理由见 [docs/wire.md](docs/wire.md#与真实-api-有意保留的差异)。
- **`bindings/node` 没有 macOS / Windows 预编译包**：没有这两个平台的环境可以验证，发一个没跑过的二进制比不发更糟；`ci.yml` 在这两个平台只做 `cargo check`。可以用 `fakellm` 二进制配合 `sdk/js`。见 [docs/release.md](docs/release.md#有意不支持)。
- **内置 `tjs` 的弱隔离**：策略和 fakellm 同进程，`tjs.exit()` / `tjs.kill(tjs.pid)` 会结束服务器，原生崩溃会带走服务器（txiki.js 的 `tjs` 全局被冻结，没法在不改子模块的前提下拦住 `tjs.exit`）。要隔离就显式用 `node` / `bun` 子进程。见 [docs/agent.md](docs/agent.md#内置-txikijs-说明)。
- **`tests/node_sdk.rs` 和 `tests/python_sdk.rs` 在缺少 `node_modules` / `uv` 时打印 skip 并跳过**：这两个套件依赖外部包管理器和网络，装不上时不应该让 `cargo test` 失败；CI 里会装好并真正跑（`ci.yml`）。
- **`bindings/node` 和 `sdk/js` 自己的测试只走 OpenAI 路径**：Responses / Anthropic 的轨迹、策略、自定义端点、路径前缀由 `tests/*_protocols.rs` 和 `sdk/node`（官方 SDK）覆盖，这两个包只是同一个引擎的薄封装，重复测每个协议没有新信息。
- **内置运行时的构建代价**：txiki.js 作为 git 子模块（约 145 MB 工作区）从源码编，要 cmake、make 和 C / C++ 编译器；冷编译多约 45 秒，release 二进制大约多 5 MB。只支持 Linux 和 macOS，Windows 上用 `--no-default-features`（策略退回 node / bun 子进程）。
- **`fakellm import` 的 `xlam` 转换器按文档 schema 写，夹具是合成的**：该数据集受限，没有 `HF_TOKEN` 拿不到真实行（401 的处理已验证）。其余格式都对着真实数据集跑过，结果表见 [docs/import.md](docs/import.md)。

## 需要作者决定

- **发布**：推 `v*` tag（触发 GHCR 镜像和 GitHub release 二进制）、发布 npm（`sdk/js`、`@fakellm/native`）、发布 crates.io、创建 GitHub 仓库。这些都是对外的、不可撤回的动作，agent 被禁止做。已验证到发布前一步：`npm pack --dry-run`（`sdk/js`、`bindings/node`）、`e2e/docker.sh` 通过。**`cargo publish` 现在发不了**：`tjs-sys` 是 `publish = false`、带约 145 MB 的子模块源码，超过 crates.io 10 MB 的上限；要发布得先决定怎么带内置运行时（把 `tjs` 做成默认关的可选 feature 并发布 `tjs-sys` 的精简源码、改成 git 依赖，或者发布不带内置运行时的版本）。清单见 [docs/release.md](docs/release.md)。
- **对着付费的真实上游录制**：`--upstream` 只在对着另一个 fakellm 时端到端测过。这台机器没有 `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`，`127.0.0.1:8317` 没有 CLIProxyAPI，claude / codex 的登录态在用户家目录配置里（约定不碰）。门控测试 `real_upstream_round_trip_when_configured` 没设变量时打印 skip 并通过。需要作者提供密钥后按 [docs/cassette.md](docs/cassette.md#对真实上游的验证状态) 的命令跑。
- **是否和 fakes3 拆出公共 crate**：目前的决定是各自维护（只有几十行时钟和日志骨架相同，拆开要两个仓库同步发版），见 [docs/design.md](docs/design.md#还没想清楚的问题)；要改这个决定的是作者。
- **LICENSE 的版权人**：`LICENSE`（仓库根、`sdk/js`、`bindings/node`）写的是 MIT、`Copyright (c) 2026 AkaraChen`，来自 git 用户名，需要作者确认这个署名对不对。
