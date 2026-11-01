# 测试控制面

fakellm 在 API 所在的同一个端口上提供一组 HTTP API，前缀是 `/_fakellm/`，用来在测试之间重置状态、注册剧本、轨迹和策略脚本、控制服务器时间、检查收到过哪些请求。
真实的 OpenAI 和 Anthropic 路径（`/v1/chat/completions`、`/v1/responses`、`/v1/messages` 等）都不以 `/_fakellm` 开头，所以不会冲突。三种协议的请求共用同一份剧本、同一个请求日志、同一个时钟。

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

**注册策略脚本等于远程代码执行。** `PUT|POST /_fakellm/policies` 的 `source` 会作为 JS 运行：默认在 fakellm 进程里（内置的 txiki.js），`runtime` 选 `node` / `bun` 时在子进程里，还可以是任意可执行文件的路径；
控制面没有认证，所以任何连得上端口的人都能以 fakellm 进程的用户身份执行代码。这是不要绑定 `0.0.0.0` 的又一个理由；需要远程访问时请放在受信任的网络里，或不要注册策略（不要用 `--policy`，也不要让不可信的调用方访问控制面）。

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
| `reset [--keep-clock] [--keep-requests] [--keep-scripts] [--keep-responses] [--drop-snapshots]` | `POST /_fakellm/reset?clock=&requests=&scripts=&responses=&snapshots=` |
| `scripts list` | `GET /_fakellm/scripts`，文本输出每条规则一行：`#序号 名字 hits/times [exhausted] 匹配条件 -> text\|tool_calls\|error 429\|hang`，`times` 不限时写 `inf`，没有名字写 `-`；`protocol`、`probability`、`every_nth` 也在匹配条件里 |
| `scripts put FILE [--keep-hits]` | `PUT /_fakellm/scripts[?keep_hits=true]`，`.yaml` / `.yml` 按 YAML 发，其他按 JSON；文本输出 `N rules loaded (M total)` |
| `scripts add FILE` | `POST /_fakellm/scripts` |
| `scripts clear` | `DELETE /_fakellm/scripts` |

所有 `put` / `add` 的 `FILE` 写 `-` 都从标准输入读（内容按 JSON、JSONL 或 YAML 自动识别；策略从 stdin 读模块源码时要加 `--name`），如 `cat rules.yaml | fakellm ctl scripts put -`。

| `trajectories list` / `get NAME [--jsonl]` | `GET /_fakellm/trajectories[/NAME]`，文本输出每条轨迹一行（步数、命中数、cursor、on_divergence） |
| `trajectories put FILE` / `add FILE` | `PUT` / `POST /_fakellm/trajectories`，JSON、JSONL 或 YAML 文件 |
| `trajectories delete NAME` / `clear` | `DELETE /_fakellm/trajectories[/NAME]` |
| `policies list` / `get NAME` | `GET /_fakellm/policies[/NAME]`，文本输出名字、运行时、命中数和运行时是否在运行（JSON 里还有 `in_process`） |
| `policies put FILE [--name N] [--runtime R]` / `add FILE ...` | `PUT` / `POST /_fakellm/policies`。`FILE` 是 ES 模块（`.js` / `.mjs` / `.cjs`，以文件名或 `--name` 作为策略名）或 JSON / YAML 注册体 `{name, source, runtime?}`；模块加载失败时服务器返回 400，命令退出码非 0 |
| `policies delete NAME` / `clear` | `DELETE /_fakellm/policies[/NAME]` |
| `cassettes list` / `get NAME` | `GET /_fakellm/cassettes[/NAME]`；`get recording` 是上游录到的内容 |
| `cassettes put FILE` / `add FILE` | `PUT` / `POST /_fakellm/cassettes`，JSON、JSONL 或 YAML 文件 |
| `cassettes delete NAME` / `clear` | `DELETE /_fakellm/cassettes[/NAME]` |
| `snapshot create [NAME]` / `list` | `POST` / `GET /_fakellm/snapshots` |
| `snapshot restore NAME` / `delete [NAME]` | `POST /_fakellm/snapshots/NAME/restore` / `DELETE /_fakellm/snapshots[/NAME]` |
| `clock get` | `GET /_fakellm/clock` |
| `clock freeze [AT]` | `POST /_fakellm/clock/freeze` |
| `clock unfreeze` | `POST /_fakellm/clock/unfreeze` |
| `clock set AT` | `POST /_fakellm/clock/set` |
| `clock advance BY` | `POST /_fakellm/clock/advance`，`BY` 可以是负数，如 `-2w` |
| `clock reset` | `POST /_fakellm/clock/reset` |
| `responses list` | `GET /_fakellm/responses`，每行 id 和创建时间 |
| `responses clear` | `DELETE /_fakellm/responses` |
| `requests list [--model G] [--has-tool N] [--stream B] [--protocol P] [--status S] [--source K] [--rule R] [--path P] [--since T] [--after SEQ] [--limit N] [--full] [--count]` | `GET /_fakellm/requests?...` |
| `requests clear` | `DELETE /_fakellm/requests` |
| `requests config [--enable \| --disable] [--capacity N]` | 不带参数是 `GET`，否则 `PUT /_fakellm/requests/config` |

`--stream` 需要一个值：`--stream true`。`--count` 只打印匹配数。`--status` 接受 `200`、`4xx`、`error`，也接受 `0`（挂起后客户端放弃的请求，见[请求日志](#请求日志)）。`ctl status` 打印版本、时钟、剧本数、cassette 数、快照数、保存的 Responses 数、strict 和请求日志状态。

## 路由

下面的输出都是对一个真实运行的服务 `curl` 得到的（`$E` 是 stdout 打印的 endpoint）。

| 方法和路径 | 作用 |
|---|---|
| `GET /_fakellm` | 状态 |
| `POST /_fakellm/reset` | 重置 |
| `GET /_fakellm/scripts` | 列出规则 |
| `PUT /_fakellm/scripts` | 替换全部规则；`?keep_hits=true` 保留同一位置规则的命中计数 |
| `POST /_fakellm/scripts` | 追加规则 |
| `DELETE /_fakellm/scripts` | 清空规则 |
| `GET` / `PUT` / `POST` / `DELETE /_fakellm/trajectories` | 轨迹的列出、替换、追加、清空，见 [fake agent](agent.md) |
| `GET` / `DELETE /_fakellm/trajectories/{name}` | 读（`?format=jsonl` 导出 JSONL）/ 删一条轨迹 |
| `GET` / `PUT` / `POST` / `DELETE /_fakellm/policies` | 策略脚本的列出、替换、追加、清空（会运行代码，见上面的[安全](#安全)） |
| `GET` / `DELETE /_fakellm/policies/{name}` | 读 / 删一个策略 |
| `GET` / `PUT` / `POST` / `DELETE /_fakellm/cassettes` | cassette 的列出、替换、追加、清空（同名的原地替换），见 [Cassette](cassette.md) |
| `GET` / `DELETE /_fakellm/cassettes/{name}` | 读（`?format=jsonl` 导出 JSONL）/ 删一个 cassette；名字 `recording` 是内存里的录制（`--upstream` 时填充），删它就是清空录制，不能用这个名字注册 |
| `POST` / `GET` / `DELETE /_fakellm/snapshots` | 创建（body `{"name"?}`，或 `?name=`，201）、列出、全部删除 |
| `GET` / `DELETE /_fakellm/snapshots/{name}` | 读 / 删一个快照 |
| `POST /_fakellm/snapshots/{name}/restore` | 还原快照（快照保留；未知名字 404，重名创建 409，非法名字 400） |
| `GET /_fakellm/responses` | 列出保存的 Responses API 对象（只有 id 和创建时间） |
| `DELETE /_fakellm/responses` | 清空这些对象 |
| `GET` / `DELETE /v1/responses/{id}` | 这不是控制面路由，是 API：读取、删除一个保存的对象，见[下面](#responses-对象) |
| `GET /_fakellm/clock` | 读时钟 |
| `POST /_fakellm/clock/{freeze,unfreeze,set,advance,reset}` | 改时钟 |
| `GET /_fakellm/requests` | 查询请求日志 |
| `DELETE /_fakellm/requests` | 清空请求日志 |
| `GET` / `PUT /_fakellm/requests/config` | 读 / 改日志设置 |

`fakellm ctl` 对这两组端点也有 `trajectories` 和 `policies` 子命令（见下面的 CLI 一节）；启动时可用 `--trajectory`、`--policy` 加载文件。

方法不对返回 405（`MethodNotAllowed`），路径不存在返回 404。

### 状态

```
$ curl $E/_fakellm
{"version":"0.1.0","clock":{"now":"2026-10-31T15:29:38.658Z","frozen":false,"shifted":false},"scripts":{"count":0},"trajectories":{"count":0},"policies":{"count":0},"responses":{"count":0},"strict":false,"request_log":{"enabled":true,"capacity":10000,"stored":0,"dropped":0}}
```

状态里还有：`cassettes.count`、`snapshots.count`，以及配置了 `--upstream` 时的 `upstream`：`{"url", "record", "recorded"}`（地址不含查询串；`record` 是记录文件路径，`recorded` 是已录的 exchange 数）。

### 重置

```
$ curl -X POST $E/_fakellm/reset
{"reset":true}
$ curl -X POST "$E/_fakellm/reset?clock=false&requests=false"
{"reset":true}
```

默认清空四样东西（另外 cassette 和录制随 `scripts` 一起清空；快照默认保留，`?snapshots=true` 才删）：剧本（包括命中计数，同时清掉全部轨迹和策略脚本并停掉它们的运行时（子进程被杀掉，内置运行时的线程被取消并 join））、请求日志、时钟（回到宿主机时间）、保存的 Responses 对象。查询参数 `clock` / `requests` / `scripts` / `responses` 设为 `false` 可以保留对应的那一项；`snapshots=true` 额外删除全部快照（默认保留）。

- 挂起（`hang`）的请求一被挂住就以 `status: 0`、`hung: true` 记入日志，放行或客户端放弃时原地更新；错误延迟或同步回复延迟期间客户端断开的请求，以 `status: 0` 记入日志。如果期间发生过清空请求日志的 `reset`，这些记录会被丢弃，不会污染新的日志。
- `reset` 会唤醒所有挂起的请求：有 `hang_ms` 的按规则的其余部分回答，只写 `hang: true` 的回 503。
- 只有 `requests` 和 `responses` 同时清空时，响应 id 的计数才归零；否则重放同一个请求会得到相同的 `resp_` id，覆盖旧对象。
- 请求日志的序号 `seq` 不会因为清空而归零，`dropped` 计数也不清。
- 不会清空的东西：启动时的配置（`--strict`、`--models`、`--fallback-tokens`、`--ttft` 等），以及日志的 `enabled` / `capacity` 设置。
- **已经在飞的流不受影响**：reset 之后，还没发完的流继续按各自已经确定的截止时间发送。重置时钟会唤醒等待中的流，让它们重新计算剩余等待（见下）。

### Responses 对象

`POST /v1/responses` 默认保存每个响应（请求里 `store: false` 除外），供三件事使用：`previous_response_id` 续写对话、`GET /v1/responses/{id}` 读取、`DELETE /v1/responses/{id}` 删除。控制面的两个路由只负责查看和清空：

```
$ curl $E/_fakellm/responses
{"responses":[{"id":"resp_...","created_at":"2026-10-31T15:29:38.000Z"}],"count":1}
$ curl -X DELETE $E/_fakellm/responses
{"deleted":1}
```

列表按创建时间从旧到新，`created_at` 是 RFC 3339（秒精度）。`GET /_fakellm` 的 `responses.count` 是保存的个数。

- **容量**：最多保留 1000 个，满了丢最旧的。
- **断流的响应也会保存**：`cut_after_chunks` 把流截断的请求，对象仍然按完整的回复保存。
- **`previous_response_id`**：新请求的 `messages` 是 `[instructions] + 被引用响应的完整对话 + 新 input`；请求日志里记录的是这个展开后的结果，所以 `has_tool_result`、`turn` 等条件看到的是整个对话。存下来的是快照：删除被引用的响应，不影响已经从它派生出的响应。
- **找不到**：`previous_response_id` 指向不存在（或 `store: false` 的、或已被 `DELETE`、已被清空的）响应时，返回 404 `invalid_request_error`，`param: previous_response_id`，`code: previous_response_not_found`，文案 “Previous response with id 'x' not found.”。这个请求以 `source: rejected` 记入日志。
- `GET` / `DELETE /v1/responses/{id}` 对未知 id 返回 404 “Response with id 'x' not found.”；`DELETE` 成功返回 `{"id","object":"response","deleted":true}`。
- **不做的 Responses 能力**（有意的，都返回 404 `Invalid URL`）：`/v1/responses/{id}/input_items`、`/cancel`、`background: true` 和 WebSocket 传输。原因：codex 走 HTTP SSE（见 [wire.md](wire.md) 的 codex 一节，请求里没有 `previous_response_id`、`store` 恒为 `false`），ai-sdk、openai-python、openai-node 的非后台调用也只用 `POST` / `GET` / `DELETE /v1/responses`；`background` 需要一个脱离请求的异步生命周期和轮询状态机，`/cancel` 只对它有意义，`/input_items` 需要把每次请求的 input 另存一份，而八个真实仓库的实验里没有任何消费方用到它们。需要时用自定义端点规则（`match.path` + `reply.body`）为单个用例伪造。

```
$ curl $E/v1/responses -d '{"model":"m","input":"hi"}'                       # 得到 resp_A
$ curl $E/v1/responses -d '{"model":"m","input":"again","previous_response_id":"resp_A"}'
$ curl $E/v1/responses -d '{"model":"m","input":"x","previous_response_id":"resp_nope"}'
{"error":{"message":"Previous response with id 'resp_nope' not found.","type":"invalid_request_error","param":"previous_response_id","code":"previous_response_not_found"}}
```

### 剧本

`PUT` 替换全部规则（默认命中计数清零，加 `?keep_hits=true` 则按位置保留已有规则的计数），`POST` 追加到末尾，请求体是规则数组或 `{"rules": [...]}`。`Content-Type` 是 `application/yaml`、`text/yaml` 或 `application/x-yaml` 时按 YAML 解析，否则看内容：以 `[` 或 `{` 开头按 JSON，其他按 YAML。规则格式见 [scripts.md](scripts.md)。响应是 `{"added": n, "count": m}`。`GET /_fakellm/responses` 返回 `{"responses": [...], "count": n}`，`count` 等于数组长度。

```
$ curl -X PUT $E/_fakellm/scripts -d '[{"name":"x","match":{"last_user":"ping"},"reply":{"text":"pong"}}]'
{"added":1,"count":1}
$ curl -X POST $E/_fakellm/scripts -H 'content-type: application/yaml' --data-binary '- reply: {text: "catch-all"}'
{"added":1,"count":2}
$ curl $E/_fakellm/scripts
{"rules":[{"index":0,"name":"x","match":{"last_user":"ping"},"reply":{"text":"pong"},"hits":0,"reached":0,"exhausted":false},{"index":1,"match":{},"reply":{"text":"catch-all"},"hits":0,"reached":0,"exhausted":false}]}
$ curl -X DELETE $E/_fakellm/scripts
{"cleared":true}
```

- `index` 是匹配顺序里的位置（从 0 开始）；`hits` 是注册以来回答了几个请求；`reached` 是其他条件都满足、到达过这条规则的请求数（`probability` 和 `every_nth` 就是按它计数，没被它们放行的请求也算）；`exhausted` 表示 `times` 用完了。
- `PUT ?keep_hits=true` 按**位置**把旧规则的 `hits` 和 `reached` 带到同一位置的新规则上（新规则比旧的多就从 0 开始），适合在测试中途改某条规则的回复又不想丢计数；布尔值的写法同其他查询参数。
- `PUT` 默认会重置命中计数（因为所有规则都是新的），`POST` 不影响已有规则的计数。
- 整个文档先全部解析和编译，任何一条规则有问题就整体拒绝，已有规则不变：

```
$ curl -X PUT $E/_fakellm/scripts -d '[{"reply":{}}]'
{"error":{"code":"BadRequest","message":"rule 0: reply needs `text`, `tool_calls`, `error`, `trajectory` or `policy`"}}
$ curl -X PUT $E/_fakellm/scripts -d '[{"match":{"bogus":1},"reply":{"text":"x"}}]'
{"error":{"code":"BadRequest","message":"rule 0: unknown field `bogus`, expected one of `model`, `last_user`, `system`, `any_message`, `body`, `has_tool`, `no_tools`, `has_tool_result`, `last_tool_result`, `tool_result_contains`, `last_tool_call_id`, `stream`, `turn`, `any`, `protocol`, `probability`, `every_nth`, `path`"}}
```

### Cassette 与快照

Cassette（格式和匹配规则见 [cassette.md](cassette.md)）：`PUT` 替换全部、`POST` 追加（同名的原地替换），请求体是一个 cassette、cassette 数组，或 JSON / YAML / JSONL 文本（`Content-Type: text/plain` 时按内容判断；JSONL 第一行是头）。响应是 `{"added": n, "count": m}`。名字必须非空、不含 `/`，也不能是 `recording`（内存里的录制）；不合法整体 400。`reset` 默认连 cassette 和录制一起清空，`?scripts=false` 则保留。

```
$ curl -X PUT $E/_fakellm/cassettes --data-binary @chats.jsonl -H 'content-type: text/plain'
{"added":1,"count":1}
$ curl $E/_fakellm/cassettes
{"cassettes":[{"name":"chats","exchanges":1,"once":false,"fuzzy":false,"hits":0,"consumed":0,"recording":false}]}
$ curl "$E/_fakellm/cassettes/chats?format=jsonl"       # 导出
$ curl "$E/_fakellm/cassettes/recording?format=jsonl"   # 导出 --upstream 录到的内容
$ curl -X DELETE $E/_fakellm/cassettes/chats
```

`hits` 是回放次数，`consumed` 是 `once` 的 cassette 已用掉的 exchange 数。

快照把当前的剧本（含命中计数）、轨迹进度、策略（含状态）、cassette（含已用标记和录制）、保存的 Responses 对象和时钟存下来；请求日志、启动配置和其他快照不在其中。还原不消耗快照，可以反复还原。名字是 1 到 64 个 `A-Za-z0-9._-` 字符，不给则是 `snapshot-N`；重名创建是 409，未知名字是 404，非法名字是 400。最多同时保留 100 个快照，超出再创建是 409（`--max-snapshots N` / `Config::max_snapshots` / `FAKELLM_MAX_SNAPSHOTS` 调整，删掉一些就能继续）。不要在有请求在飞时还原。

```
$ curl -X POST "$E/_fakellm/snapshots?name=after-login"
{"name":"after-login","created":"2026-11-01T01:59:19.155Z","scripts":0,"trajectories":0,"policies":0,"cassettes":1,"responses":0}
$ curl -X POST $E/_fakellm/snapshots/after-login/restore
$ curl $E/_fakellm/snapshots
$ curl -X DELETE $E/_fakellm/snapshots/after-login      # DELETE /_fakellm/snapshots 删全部
```

`reset` 默认**保留**快照，`?snapshots=true`（`ctl reset --drop-snapshots`）才删。快照里的 cassette 是共享的，不复制；还原策略总会重新创建它的运行时（新的子进程，或新的内置运行时线程）。命令行：`fakellm ctl cassettes list|put|add|get|delete|clear`，`fakellm ctl snapshot create|list|restore|delete`。

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

duration 的单位：`ns` `us` `ms` `s` `m` `h` `d`（24 小时）`w`，可以组合（`1h30m`），裸数字按秒，可以带 `-`。数字可以带小数（`1.5s` 等于 `1500ms`，`.5h` 等于 30 分钟），小于 1 纳秒的零头被截掉。
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

每个 API 请求（`/v1/*` 和不带前缀的 `/chat/completions`、`/messages`、`/responses`、`/models`，包括 404 和认证失败的）都会被记录，除非日志被关掉。三种协议在同一个日志里，用 `protocol` 区分。命中[自定义端点规则](scripts.md#自定义端点)的请求也会被记录（`protocol: "custom"`），不论路径在不在 `/v1` 下；没命中的未知路径照旧 404。

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
| `status` | 响应状态码；`0` 表示还没有（或永远不会有）响应：请求正被 `hang` / `hang_ms` 挂住，或客户端先放弃了。`reply` 里有 `hang` 字段 |
| `hung` | `true`：请求此刻正被 `hang` / `hang_ms` 挂住。放行或客户端断开后变回 `false`，`status` 和 `latency_us` 随之更新 |
| `latency_us` | 从到达到响应头就绪，微秒（真实时间；流的 body 不计，被时钟挂住的同步响应要等放行后才记录，包含等待） |
| `protocol` | `openai.chat`、`openai.responses`、`openai.embeddings`、`anthropic.messages` 或 `custom`（[自定义端点](scripts.md#自定义端点)）；body 解析失败时为 `null` |
| `model`、`stream` | 请求的模型名和是否流式；没有解析出请求时为 `null` / `false` |
| `source` | 谁回答的：`{"kind": "script", "index", "name"}`、`{"kind": "trajectory", "index", "name", "trajectory", "step"}`、`{"kind": "policy", "index", "name", "policy"}`、`{"kind": "cassette", "name", "index"}`（cassette 回放，`index` 是 exchange 序号）、`{"kind": "upstream"}`（转发给 `--upstream` 的）、`{"kind": "fallback"}`、`{"kind": "strict"}`、`{"kind": "rejected"}`（在匹配之前就被拒绝：认证失败、JSON 不合法、校验失败、404） |
| `rule` | 命中规则的 `name`；没有名字或不是规则回答时为 `null` |
| `request` | 解析后的结构化请求（IR）：完整的 messages、tools、`tool_choice`、参数、`extra`（不认识的字段）。不截断文本；解析失败时 `null` |
| `reply` | `{"finish", "text", "tool_calls": [{id, name, arguments}], "reasoning", "usage": {prompt_tokens, completion_tokens, total_tokens, reasoning_tokens, cached_tokens}}`，或 `{"status", "error", "retry_after", "in_stream", "headers"}`（`error` 是该协议的线上格式，Anthropic 请求用 Anthropic 的错误体）；被 `hang` 挂住的请求还有 `hang: {"until_ms": n \| null}`；没有决定时 `null` |
| `headers` | 仅 `full=true`：请求头 `[[名字(小写), 值], ...]`。`anthropic-version`、`anthropic-beta`、`openai-beta` 之类的头都在这里，可以断言客户端发了什么。`authorization` 默认原样保存；见下面的[脱敏](#脱敏) |
| `raw` | 仅 `full=true`：请求 body 的原始 JSON |
| `raw_text` | 仅 `full=true`：body 不是 JSON 时的文本，最多 64 KiB |

流式请求的 `reply` 是完整的回复（不管流是否被 `cut_after_chunks` 截断），日志不记录每个 chunk 的实际发送情况。
错误回复的 `reply.error` 是该协议的线上格式：Anthropic 请求记 `{"type":"error","error":{...}}`，其他记 OpenAI 的四键格式；`in_stream` 是错误是否作为流内事件发出，`headers` 是规则里设的额外响应头。

Anthropic 请求和 Responses 请求的 `request`（解析后的结构）与 Chat 同形：`messages` 里 Anthropic 的 `tool_result` 块是 `tool` 角色的消息，`thinking` 块是 `Thinking` part，Responses 的 `instructions` 是开头的 system 消息，`previous_response_id` 引用的历史已经展开在里面；协议自己的字段（`thinking` 配置、`previous_response_id`）有专门的键，不认识的字段在 `extra` 里原样保留。完整的原始 body 在 `raw`（`full=true`）。

#### 脱敏

请求头默认原样保存（方便断言客户端发的 key）。`--redact-auth`（环境变量 `FAKELLM_REDACT_AUTH`，Rust 里是 `Config::redact_auth`）打开后，`authorization`、`proxy-authorization`、`x-api-key` 三个头的值记成 `<redacted>`（头名保留），在写入日志时就替换，之后取不回原值。要导出日志给别人之前打开它。`raw` 里的请求体不会被脱敏。

过滤条件（查询参数，可以组合，都是“与”）：

| 参数 | 含义 |
|---|---|
| `model` | 模型名 glob（`*` 和 `?`，区分大小写） |
| `has_tool` | 请求里提供了这个名字的工具 |
| `stream` | `true` / `false` |
| `protocol` | `openai.chat` / `openai.responses` / `openai.embeddings` / `anthropic.messages` / `custom`（body 解析失败的请求没有协议，匹配不到任何一个） |
| `status` | 精确（`404`，挂起后放弃的是 `0`）、类别（`4xx`）、或 `error`（>= 400） |
| `source` | `script`（也包含轨迹和策略的回答）/ `cassette` / `upstream` / `fallback` / `strict` / `rejected` |
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
| `POST /reset` | `server.reset()`；`server.control().reset_with(ResetOptions { clock, requests, scripts, responses })` |
| `/scripts` | `server.scripts()`：`replace(Vec<Rule>)`、`append`、`clear`、`list() -> Vec<RuleInfo>`、`len`；`Rule::parse_many(text, ScriptFormat::{Json,Yaml,Auto})` |
| `/clock` | `server.clock()`：`now`、`freeze(Option<SystemTime>)`、`unfreeze`、`set`、`advance(i64 纳秒)`、`reset`、`is_frozen`；`fakellm::parse_duration("30d")` 返回纳秒 |
| `GET /requests` | `server.requests(&RequestFilter) -> (Vec<RequestRecord>, usize)` |
| `DELETE /requests` | `server.control().clear_requests()` |
| `GET /responses` | `server.response_ids() -> Vec<(String, i64)>`（id 和 unix 秒） |
| `DELETE /responses` | `server.clear_responses()`，返回删除的个数 |

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
