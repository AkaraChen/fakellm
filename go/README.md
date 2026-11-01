# fakellm Go 客户端

在 Go 测试里启动 fakellm 并控制它，用法像 testcontainers，但不需要 Docker：它直接把 fakellm 二进制作为子进程跑起来，然后包装 `/_fakellm` 控制面。
没有外部依赖，只用标准库；需要 Go 1.24。

```bash
go get github.com/AkaraChen/fakellm/go
```

## 快速开始

```go
import fakellm "github.com/AkaraChen/fakellm/go"

func TestAgent(t *testing.T) {
	ctx := context.Background()
	s, err := fakellm.Start(ctx, fakellm.Options{
		Scripts: `
- match: { last_user: "ping" }
  reply: { text: "pong" }
`,
	})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { s.Stop() })

	// OpenAI 兼容客户端用 s.BaseURL()（带 /v1），Anthropic 客户端用 s.URL()。
	client := openai.NewClient(option.WithBaseURL(s.BaseURL()), option.WithAPIKey("x"))
	...

	reqs, _ := s.Requests().List(ctx, fakellm.Filter{Rule: "agent-step-1"})
}
```

`Start` 以 `--port 0` 启动，从二进制 stdout 的第一行读出实际地址；`Stop` 杀掉进程并删除临时文件，可以重复调用。
传给 `Start` 的 `ctx` 取消时进程也会被杀掉。

## 找到二进制

按顺序：

1. `Options.Bin`
2. 环境变量 `FAKELLM_BIN`
3. `PATH` 里的 `fakellm`

都没有就返回错误。也可以先用 `Download` 取一个：

```go
bin, err := fakellm.Download(ctx, "0.1.0", filepath.Join(os.TempDir(), "fakellm-cache"))
s, err := fakellm.Start(ctx, fakellm.Options{Bin: bin})
```

`Download(ctx, version, cacheDir)` 从
`https://github.com/AkaraChen/fakellm/releases/download/v<version>/fakellm-<arch>-unknown-linux-musl.tar.gz`
下载（`<arch>` 是 `x86_64` 或 `aarch64`），解出 `fakellm` 放进缓存目录并返回路径；已缓存则直接复用；如果旁边发布了 `.sha256` 文件会校验。只支持 Linux。
**注意：这个地址现在还不存在**，第一次推 `v*` tag 触发 release 工作流之后才有；在此之前请自己构建（`cargo build`）并用 `FAKELLM_BIN`。包内的 `fakellm.ReleaseBaseURL` 可以改，测试里用本地 `httptest` 服务器验证过下载和解压。

## Options

| 字段 | 说明 |
|---|---|
| `Bin` | 二进制路径 |
| `Host`、`Port` | 监听地址；默认 `127.0.0.1` 和随机端口 |
| `APIKey` | 设置后 API 路径要求这个 key（控制面不受影响） |
| `Models` | `GET /v1/models` 列出的模型 |
| `Strict` | 没有规则命中的请求返回 400，而不是默认回复 |
| `Scripts` | 启动时加载的剧本：JSON 或 YAML 字符串（`string` / `[]byte`），或 `[]Rule`。不合法则 `Start` 返回错误 |
| `FallbackTokens` / `ChunkTokens` / `ArgsChunkBytes` | 对应 `--fallback-tokens`、`--chunk-tokens`、`--args-chunk-bytes`，0 为服务器默认值 |
| `Trajectories` | 启动后注册的轨迹：`Trajectory`、`[]Trajectory`，或 JSON / YAML / JSONL 字符串（`string` / `[]byte`） |
| `Policies` | 启动后注册的策略，`[]fakellm.Policy{{Name, Source, Runtime}}`；需要机器上有 node、bun 或 tjs |
| `PolicyRuntime` | `--policy-runtime`：`tjs`、`node`、`bun` 或路径；默认由服务器探测 |
| `PathPrefix` | `--path-prefix`：API 同时挂在这个前缀下；`BaseURL()` 和 `AnthropicBaseURL()` 会带上它（控制面不变） |
| `Cassettes` | 启动后注册的 cassette：`Cassette`、`[]Cassette`，或 JSON / YAML / JSONL 字符串（`string` / `[]byte`） |
| `Upstream`、`UpstreamAPIKey` | `--upstream`、`--upstream-api-key`：没有规则和 cassette 命中的请求转发给这个上游并录下来 |
| `Record` | `--record`：把从上游录到的交换追加到这个 JSONL 文件，需要 `Upstream` |
| `RecordRedact`、`RecordErrors` | `--record-redact`（正则，匹配到的录制文本换成 `<redacted>`）、`--record-errors`（也录上游的 401、429、5xx 等），需要 `Upstream` |
| `UpstreamTimeoutSecs`、`PolicyTimeoutMs`、`MaxSnapshots` | `--upstream-timeout`、`--policy-timeout-ms`、`--max-snapshots`；0 为服务器默认 |
| `TrajectoryFiles`、`PolicyFiles` | `--trajectory`、`--policy`：由二进制自己加载并监视的文件（`Watch` 为 false 时不监视） |
| `Watch` | `*bool`；`fakellm.Ptr(false)` 传 `--no-watch`，不再监视 `Args` 里给的剧本、轨迹、策略和 cassette 文件；nil 为默认（监视） |
| `Args` | 追加的命令行参数，如 `--ttft`、`--tokens-per-sec` |
| `Env` | 追加的环境变量，`KEY=value` |
| `StartTimeout` | 等待启动的时间，默认 10 秒 |

## 控制面

所有方法第一个参数是 `context.Context`，类型对应 [docs/control-plane.md](../docs/control-plane.md) 和 [docs/scripts.md](../docs/scripts.md) 里的 JSON。控制面返回非 2xx 时错误是 `*fakellm.Error`（`Status`、`Code`、`Message`）。

```go
s.Status(ctx)                          // 版本、时钟、剧本数、请求日志状态
s.Reset(ctx, fakellm.ResetOptions{})   // 每个测试开始前；KeepClock/KeepRequests/KeepScripts/KeepResponses 保留对应项

n, err := s.Scripts().Put(ctx, rules) // 替换全部规则（返回现有规则数）；rules 是 []Rule，或 JSON/YAML 字符串。Rule 里的 Text 等可空字符串用 fakellm.Ptr("")
s.Scripts().PutKeepHits(ctx, rules)    // 同上，按位置保留命中计数
s.Scripts().Add(ctx, rules)            // 追加
s.Scripts().List(ctx)                  // []RuleInfo：规则加 Hits / Reached / Exhausted
s.Scripts().Clear(ctx)

s.Requests().List(ctx, fakellm.Filter{Rule: "x", Status: "4xx", Full: true})
s.Requests().Count(ctx, fakellm.Filter{Protocol: "anthropic.messages"})
s.Requests().Config(ctx)                                  // 读取日志设置
s.Requests().SetConfig(ctx, fakellm.LogUpdate{Capacity: fakellm.Ptr(50)}) // 开关 / 改容量
s.Wire(ctx, "claude", "shell")                             // 等同 `fakellm wire`
w, _ := s.WireJSON(ctx, "codex")                           // 解析后的 env / files / snippet；files[NAME] 代替 --file
s.AnthropicBaseURL()                                      // 同 s.URL()
s.Requests().Clear(ctx)

s.Clock().Freeze(ctx, at)              // at 可省略，停在当前服务器时间
s.Clock().Advance(ctx, 500*time.Millisecond)
s.Clock().Set(ctx, at); s.Clock().Unfreeze(ctx); s.Clock().Reset(ctx); s.Clock().Now(ctx)

s.Responses().List(ctx)                // 保存的 Responses 对象（id 和创建时间）
s.Responses().Clear(ctx)
```

### 轨迹和策略

轨迹是一段录好的 agent 过程，每个请求回放一步；策略是一个 JS 模块，由它按请求决定回复。规则里用 `Reply: fakellm.ReplySpec{Trajectory: "名字"}` 或 `Policy: "名字"` 引用它们，细节见 [docs/agent.md](../docs/agent.md) 和 [docs/m3-spec.md](../docs/m3-spec.md)。

```go
s, _ := fakellm.Start(ctx, fakellm.Options{
	Trajectories: fakellm.Trajectory{
		Name: "fix-bug",
		Steps: []fakellm.TrajectoryStep{
			{Reply: fakellm.ReplySpec{ToolCalls: []fakellm.ToolCallSpec{{Name: "shell", Arguments: map[string]any{"cmd": "go test"}}}}},
			{
				Reply:  fakellm.ReplySpec{Text: fakellm.Ptr("done")},
				Expect: &fakellm.Expect{ToolResults: []fakellm.ToolResultExpect{{Name: "shell", Contains: "ok"}}},
			},
		},
		ToolMap: map[string]fakellm.ToolMapping{"shell": {Name: "Bash", Args: map[string]any{"command": "$cmd"}}},
	},
	Policies: []fakellm.Policy{{Name: "p", Source: `export default (req, ctx) => ({ text: "hit " + ctx.hits })`}},
	Scripts:  []fakellm.Rule{{Match: fakellm.Match{Any: fakellm.Ptr(true)}, Reply: fakellm.ReplySpec{Trajectory: "fix-bug"}}},
})

s.Trajectories().Put(ctx, v)           // 替换全部；v 同 Options.Trajectories，返回现有数量
s.Trajectories().Add(ctx, v)           // 追加，同名原位替换
s.Trajectories().List(ctx)             // []TrajectoryInfo：Steps / Hits / LastStep 等
s.Trajectories().Get(ctx, "fix-bug")   // Trajectory；JSONL(ctx, name) 取 JSON 行
s.Trajectories().Delete(ctx, "fix-bug"); s.Trajectories().Clear(ctx)

s.Policies().Put(ctx, fakellm.Policy{Name: "p", Source: src}) // 全部先试加载，失败返回 400 的 *fakellm.Error，原状态不变
s.Policies().Add(ctx, policies...)     // 同名替换（状态重置）
s.Policies().List(ctx)                 // []PolicyInfo：Runtime / Running / Hits / Restarts / State / Log
s.Policies().Get(ctx, "p"); s.Policies().Delete(ctx, "p"); s.Policies().Clear(ctx)
```

`Options.Trajectories` / `Policies` 在 `Start` 里通过控制面注册，不合法则 `Start` 返回错误并杀掉进程。`Reset` 会同时清掉轨迹和策略。

### Cassette 和快照

Cassette 是录好或导入的请求/回复交换，按请求含义回放，详见 [docs/cassette.md](../docs/cassette.md)。快照保存并还原整个控制面状态（剧本、轨迹、策略、cassette、保存的响应和时钟）。

```go
// 录制：proxy 把没人答的请求转给 up，并追加到 rec.jsonl
proxy, _ := fakellm.Start(ctx, fakellm.Options{Upstream: up.URL() + "/v1", Record: "rec.jsonl"})
jsonl, _ := proxy.Cassettes().JSONL(ctx, "recording") // 内存里的录制；名字固定为 recording，注册前请改名

// 回放
s, _ := fakellm.Start(ctx, fakellm.Options{Strict: true, Cassettes: jsonl})
s.Cassettes().Put(ctx, v)               // 替换全部；v 同 Options.Cassettes，返回现有数量
s.Cassettes().Add(ctx, v)               // 追加，同名替换
s.Cassettes().List(ctx)                 // []CassetteInfo：Exchanges / Hits / Consumed / Recording 等
s.Cassettes().Get(ctx, name)            // Cassette；JSONL(ctx, name) 取 JSON 行
s.Cassettes().Delete(ctx, name); s.Cassettes().Clear(ctx)

// 快照：先拍基线，每个测试前还原
s.Snapshots().Create(ctx, "base")       // 名字为空则自动取 snapshot-N；重名返回 409 的 *fakellm.Error
s.Snapshots().Restore(ctx, "base")      // 快照保留
s.Snapshots().List(ctx); s.Snapshots().Get(ctx, "base")
s.Snapshots().Delete(ctx, "base"); s.Snapshots().Clear(ctx)
s.Reset(ctx, fakellm.ResetOptions{DropSnapshots: true}) // Reset 默认保留快照
```

写规则时可选字段用指针，`fakellm.Ptr(v)` 帮你取地址：

```go
rule := fakellm.Rule{
	Name:  "rate-limit",
	Match: fakellm.Match{LastUser: "hi", Stream: fakellm.Ptr(true)},
	Times: fakellm.Ptr(uint32(1)),
	Reply: fakellm.ReplySpec{Error: &fakellm.ErrorSpec{Status: 429, RetryAfter: fakellm.Ptr(2.0)}},
}
```

用时钟控制流：冻结后发 `ttft_ms: 500` 的流式请求，头会先到、第一个 chunk 不会出现，`Advance(ctx, 500*time.Millisecond)` 后才放行。详见控制面文档的[时钟](../docs/control-plane.md#时钟)一节。

## 测试

```bash
cargo build                     # 在仓库根目录
cd go && go test ./...
```

测试用 `FAKELLM_BIN`，没设置时用 `../target/debug/fakellm`，找不到就跳过。
