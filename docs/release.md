# 发布清单

下面这些都是**需要外部账号、权限或发布动作**的步骤，不是代码缺口，所以不放在 [TODO.md](../TODO.md)。代码、工作流和脚本都已就绪，按顺序做即可。

## 第一次发布

1. 确认 `cargo test`、`cargo clippy --all-targets -- -D warnings`、`cargo fmt --check`、`sdk/js` / `sdk/node` / `bindings/node` 的测试、Go 的 `go test`、`e2e/docker.sh` 都是绿的。
2. 手动触发 `e2e` 工作流（`workflow_dispatch`，在 GitHub Actions 里装最新的 claude / codex / opencode），或者在装了这三个 CLI 的机器上跑 `e2e/*.sh`；结果记进 README 的“测试与验证”。
3. 推 `v<版本>` tag：`release.yml` 构建并推送 GHCR 镜像（`latest`、版本、`主.次`、短 sha，另有 `node` 变体）和 GitHub release 的二进制（`go/` 的 `Download` 依赖它）；`node.yml` 构建 Linux 的四个 `.node` 并在有 `NPM_TOKEN` 时发布 `@fakellm/native`。没有 tag 或手动触发时它们都是 dry run。
4. 发布 `sdk/js` 到 npm（`npm publish`，需要 npm 账号）。
5. 发布到 crates.io（`cargo publish`，需要 crates.io token）；之后把 README 里的 git 依赖改成版本号。
6. 镜像发布后，把 README 的 Testcontainers 片段里的镜像名在真实镜像上跑一遍（本地已经用 `fakellm:local` 验证过同样的流程，见下）。

## 已经在本地验证过的

- Docker：`e2e/docker.sh` 构建默认镜像（alpine，含 `hyper-rustls` / `ring` / `webpki-roots`）和 node 变体，两者都启动并回答 chat completion，默认镜像拒绝策略、node 变体跑通策略。
- Testcontainers：片段做的事是“暴露 4010、等日志里出现 `http://`、用映射端口访问、`PUT` 规则、请求”，用 `docker run -p` 在 `fakellm:local` 上逐步做过一遍，等价且通过。
- Bun：`bun test` 跑 `bindings/node/__test__/` 下的全部文件（包括 `agent.test.mjs`），以及 `sdk/js`、`sdk/node`。

## 有意不支持

- `bindings/node` 的 macOS / Windows 预编译包：没有这两个平台的运行环境来验证（napi 的轮询、`SO_LINGER` 复位等行为依赖平台），发一个没跑过的二进制比不发更糟。需要的人用 `fakellm` 二进制配合 `sdk/js`（它只是启动子进程）或自己 `npm run build`；`ci.yml` 的 `compile` 任务在 macOS 和 Windows 上只做 `cargo check`，证明库和二进制能编译，没有在那两个平台跑过测试。
