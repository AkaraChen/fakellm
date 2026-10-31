# 测试控制面

fakellm 在 API 所在的同一个端口上提供一组 HTTP API，前缀是 `/_fakellm/`，用来在测试之间重置状态、注册剧本、控制服务器时间、检查收到过哪些请求。
真实的 OpenAI 路径都不以 `/_fakellm` 开头，所以不会冲突。

不需要写客户端库：用命令行 `fakellm ctl`，或者任何能发 HTTP 请求的工具（`curl`）。
Rust 进程内使用时，同样的操作是 `fakellm::Server` 上的方法（见[最后一节](#rust-api)）。

```bash
fakellm --port 4010 &
export FAKELLM_ENDPOINT=http://127.0.0.1:4010     # ctl 的默认值就是这个

fakellm ctl scripts put agent.yaml                # 注册剧本
fakellm ctl reset                                 # 每个测试开始前
fakellm ctl clock freeze                          # 冻结服务器时间
fakellm ctl clock advance 500ms                   # 放行被时钟挂住的流
fakellm ctl requests list --rule step-1 --status 4xx
```

## 安全

**控制面没有认证**，包括配了 `--api-key` 的时候（`--api-key` 只管 API 路径）。任何连得上端口的人都能清空剧本、改时钟。
这是一个测试服务，默认只监听 `127.0.0.1`；改成 `--host 0.0.0.0` 之前请想清楚。

唯一的防护：带 `Origin` 头的非 GET 请求会被拒绝（403），防止用户浏览器里打开的网页向 `localhost` 发跨站 POST 把服务清空。
curl、`fakellm ctl` 和测试框架的 HTTP 客户端不会发 `Origin`。注意在浏览器环境里跑的测试（比如 Playwright 页面里的 `fetch`）会带 `Origin`，要把控制面请求放到测试进程（Node 侧）发。

```
$ curl -X POST -H 'Origin: http://evil.example' $E/_fakellm/reset
{"error":{"code":"Forbidden","message":"cross-origin requests are not allowed"}}
```

## 约定

- 请求体和响应体都是 JSON（例外：`scripts` 的请求体也可以是 YAML）。控制面的请求体可以为空（等价于 `{}`），未知字段会报 400。
- 时间用 RFC 3339，响应里是毫秒精度的 UTC，例如 `2030-01-02T03:04:05.000Z`；请求里接受任何带时区的 RFC 3339（`2030-01-02T04:04:05+01:00`）。时间都是**服务器时钟**的时间（见[时钟](#时钟)）。
- 错误响应：`{"error": {"code": "NotFound", "message": "no such endpoint: /_fakellm/nope"}}`，`code` 为 `BadRequest`（400）/ `Forbidden`（403）/ `NotFound`（404）/ `MethodNotAllowed`（405）/ `TooLarge`（413，请求体超过 4 MiB）。
  注意这里的错误体比 OpenAI 的简单，只有 `code` 和 `message`。
- 路径末尾的 `/` 会被忽略。
- 控制面自己的请求不记录在请求日志里。
- 查询参数里的布尔值接受 `true` / `1` / 空串（即只写 `?full`）为真，`false` / `0` 为假，其他值是 400。查询参数未知的一律 400。
- `fakellm ctl` 默认打印整理过的文本，`--json` 打印服务器返回的 JSON。出错时往 stderr 写信息，退出码非 0。

## CLI

`fakellm ctl [--endpoint URL] [--json] <命令>`。`--endpoint` 也可以用环境变量 `FAKELLM_ENDPOINT` 指定，默认 `http://127.0.0.1:4010`（只支持 `http://`）。
不带 `ctl` 的 `fakellm --port 0 ...` 仍然是启动服务。

| 命令 | 对应的请求 |
|---|---|
| `status` | `GET /_fakellm` |
| `reset [--keep-clock] [--keep-requests] [--keep-scripts]` | `POST /_fakellm/reset?clock=&requests=&scripts=` |
| `scripts list` | `GET /_fakellm/scripts` |
| `scripts put FILE` | `PUT /_fakellm/scripts`，`.yaml` / `.yml` 按 YAML 发，其他按 JSON |
| `scripts add FILE` | `POST /_fakellm/scripts` |
| `scripts clear` | `DELETE /_fakellm/scripts` |
| `clock get` | `GET /_fakellm/clock` |
| `clock freeze [AT]` | `POST /_fakellm/clock/freeze` |
| `clock unfreeze` | `POST /_fakellm/clock/unfreeze` |
| `clock set AT` | `POST /_fakellm/clock/set` |
| `clock advance BY` | `POST /_fakellm/clock/advance`，`BY` 可以是负数，如 `-2w` |
| `clock reset` | `POST /_fakellm/clock/reset` |
| `requests list [--model G] [--has-tool N] [--stream B] [--status S] [--source K] [--rule R] [--path P] [--since T] [--after SEQ] [--limit N] [--full] [--count]` | `GET /_fakellm/requests?...` |
| `requests clear` | `DELETE /_fakellm/requests` |
| `requests config [--enable \| --disable] [--capacity N]` | 不带参数是 `GET`，否则 `PUT /_fakellm/requests/config` |

`--stream` 需要一个值：`--stream true`。`--count` 只打印匹配数。

## 路由

下面的输出都是对一个真实运行的服务 `curl` 得到的（`$E` 是 stdout 打印的 endpoint）。

| 方法和路径 | 作用 |
|---|---|
| `GET /_fakellm` | 状态 |
| `POST /_fakellm/reset` | 重置 |
| `GET /_fakellm/scripts` | 列出规则 |
| `PUT /_fakellm/scripts` | 替换全部规则 |
| `POST /_fakellm/scripts` | 追加规则 |
| `DELETE /_fakellm/scripts` | 清空规则 |
| `GET /_fakellm/clock` | 读时钟 |
| `POST /_fakellm/clock/{freeze,unfreeze,set,advance,reset}` | 改时钟 |
| `GET /_fakellm/requests` | 查询请求日志 |
| `DELETE /_fakellm/requests` | 清空请求日志 |
| `GET` / `PUT /_fakellm/requests/config` | 读 / 改日志设置 |

方法不对返回 405（`MethodNotAllowed`），路径不存在返回 404。

### 状态

```
$ curl $E/_fakellm
{"version":"0.1.0","clock":{"now":"2026-10-31T15:29:38.658Z","frozen":false,"shifted":false},"scripts":{"count":0},"strict":false,"request_log":{"enabled":true,"capacity":10000,"stored":0,"dropped":0}}
```

### 重置

```
$ curl -X POST $E/_fakellm/reset
{"reset":true}
$ curl -X POST "$E/_fakellm/reset?clock=false&requests=false"
{"reset":true}
```

默认清空三样东西：剧本（包括命中计数）、请求日志、时钟（回到宿主机时间）。查询参数 `clock` / `requests` / `scripts` 设为 `false` 可以保留对应的那一项。

- 请求日志的序号 `seq` 不会因为清空而归零，`dropped` 计数也不清。
- 不会清空的东西：启动时的配置（`--strict`、`--models`、`--fallback-tokens`、`--ttft` 等），以及日志的 `enabled` / `capacity` 设置。
- **已经在飞的流不受影响**：reset 之后，还没发完的流继续按各自已经确定的截止时间发送。重置时钟会唤醒等待中的流，让它们重新计算剩余等待（见下）。

### 剧本

`PUT` 替换全部规则，`POST` 追加到末尾，请求体是规则数组或 `{"rules": [...]}`。`Content-Type` 是 `application/yaml`、`text/yaml` 或 `application/x-yaml` 时按 YAML 解析，否则看内容：以 `[` 或 `{` 开头按 JSON，其他按 YAML。规则格式见 [scripts.md](scripts.md)。

```
$ curl -X PUT $E/_fakellm/scripts -d '[{"name":"x","match":{"last_user":"ping"},"reply":{"text":"pong"}}]'
{"added":1,"count":1}
$ curl -X POST $E/_fakellm/scripts -H 'content-type: application/yaml' --data-binary '- reply: {text: "catch-all"}'
{"added":1,"count":2}
$ curl $E/_fakellm/scripts
{"rules":[{"index":0,"name":"x","match":{"last_user":"ping"},"reply":{"text":"pong"},"hits":0,"exhausted":false},{"index":1,"match":{},"reply":{"text":"catch-all"},"hits":0,"exhausted":false}]}
$ curl -X DELETE $E/_fakellm/scripts
{"cleared":true}
```

- `index` 是匹配顺序里的位置（从 0 开始）；`hits` 是注册以来回答了几个请求；`exhausted` 表示 `times` 用完了。
- `PUT` 会重置命中计数（因为所有规则都是新的），`POST` 不影响已有规则的计数。
- 整个文档先全部解析和编译，任何一条规则有问题就整体拒绝，已有规则不变：

```
$ curl -X PUT $E/_fakellm/scripts -d '[{"reply":{}}]'
{"error":{"code":"BadRequest","message":"rule 0: reply needs `text`, `tool_calls` or `error`"}}
$ curl -X PUT $E/_fakellm/scripts -d '[{"match":{"bogus":1},"reply":{"text":"x"}}]'
{"error":{"code":"BadRequest","message":"rule 0: unknown field `bogus`, expected one of `model`, `last_user`, `system`, `any_message`, `has_tool`, `no_tools`, `has_tool_result`, `last_tool_result`, `tool_result_contains`, `stream`, `turn`, `any`"}}
```

### 时钟

```
$ curl $E/_fakellm/clock
{"now":"2026-10-31T15:29:56.911Z","frozen":false,"shifted":false}
$ curl -X POST $E/_fakellm/clock/freeze -d '{"at":"2030-01-02T03:04:05Z"}'
{"now":"2030-01-02T03:04:05.000Z","frozen":true,"shifted":true}
$ curl -X POST $E/_fakellm/clock/advance -d '{"by":"30d"}'
{"now":"2030-02-01T03:04:05.000Z","frozen":true,"shifted":true}
$ curl -X POST $E/_fakellm/clock/set -d '{"at":"2031-01-01T00:00:00Z"}'
{"now":"2031-01-01T00:00:00.000Z","frozen":true,"shifted":true}
$ curl -X POST $E/_fakellm/clock/unfreeze
{"now":"2031-01-01T00:00:00.000Z","frozen":false,"shifted":true}
$ curl -X POST $E/_fakellm/clock/reset
{"now":"2026-10-31T15:29:56.968Z","frozen":false,"shifted":false}
```

| 操作 | 请求体 | 语义 |
|---|---|---|
| `freeze` | `{"at": "<RFC 3339>"}`，`at` 可省 | 在 `at` 停住；省略则停在当前服务器时间 |
| `unfreeze` | 无 | 从当前服务器时间继续走（不会跳回宿主机时间） |
| `set` | `{"at": "<RFC 3339>"}`，必填 | 跳到 `at`，保持冻结或运行的状态 |
| `advance` | `{"by": "<duration>"}`，必填 | 向前（或用负数向后）移动 |
| `reset` | 无 | 回到宿主机时钟 |

duration 的单位：`ns` `us` `ms` `s` `m` `h` `d`（24 小时）`w`，可以组合（`1h30m`），裸数字按秒，可以带 `-`。不支持小数（`1.5s` 是 400，写 `1500ms`）。
响应字段：`now` 是服务器时间，`frozen` 是否冻结，`shifted` 是否与宿主机时钟不同（冻结，或带偏移地运行）。
时钟范围大约是 1824 到 2116 年，超出返回 400。

**时钟读取点**：`created`、请求日志的 `time`、首 token 延迟（`ttft_ms` / `delay_ms`）、token 间隔（`tokens_per_sec`）、错误响应前的 `delay_ms`。每个 `Server` 一个时钟，互不影响。它**不影响**客户端自己的定时器（SDK 的 backoff、超时），那些是真实时间。

**`wait_until` 的行为**（流式和延迟响应都靠它）：一个需要等到某个服务器时刻 T 才能发出的 chunk（或同步响应、错误响应）这样等待：

- 时钟**冻结**时，一直挂着，直到控制面把时间推到 T 之后才放行；真实时间流逝不会让它醒来。
- 时钟**运行**时，按宿主机时间睡到 T，但每次控制面改时钟都会唤醒它重新计算，所以 `advance` 也能缩短运行中的等待。
- 每个 chunk 的截止时间是在响应开始时按 `ttft` 和 `token 间隔` 算好的绝对服务器时刻（首个 content chunk 在 `ttft`，之后每个 chunk 比前一个晚 `token 间隔 × chunk_tokens`；role chunk 和结尾的 finish / usage chunk 紧跟前一个）。因此向前 `advance` 会一次放行所有已到期的 chunk，向后 `set` / `reset` 会拉长剩余等待。
- 流的响应头在 `ttft` 之前就已经发出，客户端能拿到 200 和 `text/event-stream`；冻结时第一个 chunk 也不会出现，所以 `await create({stream: true})` 会返回，而第一次 `next()` 挂住。
- 非流式响应和错误响应在等待结束之后才发响应头，所以客户端的 `await create()` 本身就会挂住。这种请求在请求日志里要等被放行后才出现。
- 客户端断开后，等待中的流立刻停止，不会泄漏任务。

一个典型的流程（来自 `sdk/node/openai.test.mjs`）：冻结，发一个 `ttft_ms: 500` 的流式请求，确认 100ms 内没有任何 chunk，`advance 500ms`，首个 chunk 到达。

### 请求日志

每个 API 请求（`/v1/*` 和不带前缀的 `/chat/completions`、`/models`，包括 404 和认证失败的）都会被记录，除非日志被关掉。

```
$ curl "$E/_fakellm/requests?full=true&limit=1"
{"requests":[{"seq":33,"time":"2030-01-02T03:04:05.000Z","method":"POST","path":"/v1/chat/completions","status":429,"latency_us":266,"protocol":"openai.chat","model":"m","stream":false,"source":{"kind":"script","index":0,"name":"rl"},"rule":"rl","request":{...},"reply":{"status":429,"error":{"message":"Rate limit reached for requests. Please try again later.","type":"requests","param":null,"code":"rate_limit_exceeded"},"retry_after":2.0},"headers":[["host","127.0.0.1:42313"],...],"raw":{"model":"m","messages":[{"role":"user","content":"x"}]}}],"total":1,"dropped":0}
```

响应是 `{"requests": [...], "total": N, "dropped": N}`：`total` 是 `limit` 之前的匹配数，`dropped` 是因为缓冲满了被挤出去的记录总数。记录按到达顺序，旧的在前。

记录字段：

| 字段 | 含义 |
|---|---|
| `seq` | 到达顺序，从 1 开始，`reset` / 清空后继续增长 |
| `time` | 请求到达时的服务器时间 |
| `method`、`path` | 方法，和带查询串的路径（原样） |
| `status` | 响应状态码 |
| `latency_us` | 从到达到响应头就绪，微秒（真实时间；流的 body 不计，被时钟挂住的同步响应要等放行后才记录，包含等待） |
| `protocol` | `openai.chat`；body 解析失败时为 `null` |
| `model`、`stream` | 请求的模型名和是否流式；没有解析出请求时为 `null` / `false` |
| `source` | 谁回答的：`{"kind": "script", "index", "name"}`、`{"kind": "fallback"}`、`{"kind": "strict"}`、`{"kind": "rejected"}`（在匹配之前就被拒绝：认证失败、JSON 不合法、校验失败、404） |
| `rule` | 命中规则的 `name`；没有名字或不是规则回答时为 `null` |
| `request` | 解析后的结构化请求（IR）：完整的 messages、tools、`tool_choice`、参数、`extra`（不认识的字段）。不截断文本；解析失败时 `null` |
| `reply` | `{"finish", "text", "tool_calls": [{id, name, arguments}], "reasoning", "usage": {prompt_tokens, completion_tokens, total_tokens, reasoning_tokens, cached_tokens}}`，或 `{"error": {...}}`；没有决定时 `null` |
| `headers` | 仅 `full=true`：请求头 `[[名字(小写), 值], ...]`，`authorization` 不脱敏 |
| `raw` | 仅 `full=true`：请求 body 的原始 JSON |
| `raw_text` | 仅 `full=true`：body 不是 JSON 时的文本，最多 64 KiB |

流式请求的 `reply` 是完整的回复（不管流是否被 `cut_after_chunks` 截断），日志不记录每个 chunk 的实际发送情况。

过滤条件（查询参数，可以组合，都是“与”）：

| 参数 | 含义 |
|---|---|
| `model` | 模型名 glob（`*` 和 `?`，区分大小写） |
| `has_tool` | 请求里提供了这个名字的工具 |
| `stream` | `true` / `false` |
| `status` | 精确（`404`）、类别（`4xx`）、或 `error`（>= 400） |
| `source` | `script` / `fallback` / `strict` / `rejected` |
| `rule` | 规则名（精确） |
| `path` | 路径前缀，如 `/v1/chat/completions` |
| `since` | RFC 3339，只要这个服务器时间或之后的 |
| `after` | 序号，只要 `seq` 大于它的（轮询新请求时用上次最大的 `seq`） |
| `limit` | 只保留最近的 N 条匹配 |
| `full` | 附带 `headers` 和 `raw`（不是过滤条件） |

```
$ curl -X DELETE $E/_fakellm/requests
{"cleared":true}
$ curl $E/_fakellm/requests/config
{"enabled":true,"capacity":10000,"stored":1,"dropped":0}
$ curl -X PUT $E/_fakellm/requests/config -d '{"capacity":50,"enabled":true}'
{"enabled":true,"capacity":50,"stored":1,"dropped":0}
```

`PUT .../config` 的两个字段都可省；`capacity` 至少为 1。缩小容量时，超出的旧记录会被丢掉（计入 `dropped`）。`enabled: false` 之后新请求不再记录，已有的记录保留。
`fakellm ctl requests list` 的文本输出每条一行：`seq 时间 状态 方法 路径 模型 stream=… 来源 回复摘要`，末尾有 `showing N of M matching requests`：

```
7  2026-10-31T15:29:49.541Z  429 POST   /v1/chat/completions  gpt-4o  stream=false  script/rate-limit-once  error: Rate limit reached for requests. Please try again later.
8  2026-10-31T15:29:49.549Z  200 POST   /v1/chat/completions  gpt-4o  stream=false  fallback  stop "Elit proident esse veniam non cillum in ex esse amet dolore ..."
9  2026-10-31T15:29:49.560Z  200 POST   /v1/chat/completions  gpt-4o  stream=true  script/read-file  tool_calls tools=[read_file]
```

## Rust API

同样的操作在 `fakellm::Server` 上（`BlockingServer` 通过 `Deref` 拥有相同的方法）：

| HTTP | Rust |
|---|---|
| `POST /reset` | `server.reset()`；`server.control().reset_with(ResetOptions { clock, requests, scripts })` |
| `/scripts` | `server.scripts()`：`replace(Vec<Rule>)`、`append`、`clear`、`list() -> Vec<RuleInfo>`、`len`；`Rule::parse_many(text, ScriptFormat::{Json,Yaml,Auto})` |
| `/clock` | `server.clock()`：`now`、`freeze(Option<SystemTime>)`、`unfreeze`、`set`、`advance(i64 纳秒)`、`reset`、`is_frozen`；`fakellm::parse_duration("30d")` 返回纳秒 |
| `GET /requests` | `server.requests(&RequestFilter) -> (Vec<RequestRecord>, usize)` |
| `DELETE /requests` | `server.control().clear_requests()` |

`RequestFilter` 的字段与查询参数同名（`path` 对应 `path_prefix`，`status` 是 `StatusFilter`，可以用 `"4xx".parse()` 构造）。`RequestRecord` 的字段是 `seq`、`time`、`method`、`path`、`status`、`latency`、`headers`、`raw`、`raw_text`、`request`、`outcome`，另有方法 `model()`、`stream()`、`protocol()`。

```rust
let server = fakellm::Server::start(fakellm::Config::default()).await?;
let rules = fakellm::Rule::parse_many(&std::fs::read_to_string("agent.yaml")?, fakellm::ScriptFormat::Yaml)?;
server.scripts().replace(rules)?;
server.clock().freeze(None)?;
// ... 发请求；流被挂住 ...
server.clock().advance(500_000_000)?;   // 500ms
let filter = fakellm::RequestFilter { has_tool: Some("read_file".into()), ..Default::default() };
let (rows, total) = server.requests(&filter);
```

Rust API 不经过 HTTP，也就没有 `Origin` 检查。
