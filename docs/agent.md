# Fake agent：轨迹与策略脚本

被测的 agent（codex、Claude Code、opencode，或者你自己写的循环）在一个循环里反复请求模型、执行工具、把结果发回来。fakellm 在这个循环里扮演**模型**这一方，工具由 agent 自己去执行，所以 fake agent 测的是 agent 框架：工具分发、结果回传、上下文管理、重试、中断、审批。

```
agent 框架 ──messages + tools──▶ fakellm（扮演模型）
     ▲                               │
     │                     按轨迹或策略返回 tool_call
     │                               ▼
  执行工具 ◀─────────────────── tool_call
     │
     └──tool result──▶ fakellm 检查结果，然后走下一步
```

回复来自两种东西，都由[剧本](scripts.md)里的规则交给它们：

- **轨迹（trajectory）**：一次 agent 运行写成固定的回合列表，按请求里的历史回放，可以检查客户端送回来的工具结果，还能把轨迹里的工具名和参数改写成被测客户端的写法。
- **策略脚本（policy）**：一个 JS 模块根据请求决定回复，适合“先读文件再改；工具报错就重试一次，再失败就放弃”这类分支逻辑。

两者都跨协议：Chat、Responses、Anthropic Messages 的请求先被解析成同一个结构，同一条轨迹或策略可以回答它们。请求日志里 `source` 分别是 `{"kind":"trajectory",...,"trajectory":"名字","step":2}` 和 `{"kind":"policy",...,"policy":"名字"}`，`?source=script` 过滤能同时看到它们。

## 在规则里引用

```yaml
- name: side                     # 生成标题之类不带工具的旁路请求
  match: { no_tools: true }
  reply: { text: ok }
- name: agent
  reply: { trajectory: fix-bug } # 或 policy: reader
```

- 规则自己的 `match`、`times` 照常生效，可以有多条规则共用同一条轨迹。
- `reply` 里 `trajectory` / `policy` 不能和 `text`、`tool_calls`、`reasoning`、`finish_reason`、`error` 同时写，两者也互斥；`stream`、`delay_ms`、`hang*`、`reset`、`usage` 写在规则上是步骤（或策略回复）的默认值，步骤自己写了就用自己的。
- 轨迹和策略的名字不在注册规则时检查，哪怕先注册规则、后注册轨迹也行；请求到来时找不到就是 400 `fakellm_trajectory_not_found` / `fakellm_policy_not_found`。
- 工具、助手回合等信息每次都取自请求本身，fakellm 不在服务端保存会话，所以重试和并行的对话互不干扰。

## 轨迹

### 格式

```yaml
kind: trajectory
name: fix-bug
on_divergence: strict        # strict（默认）| lenient | stop
stop_text: 轨迹偏离，停止。   # lenient / stop 用完或偏离时的回复，默认 "Trajectory diverged; stopping."
cursor: by_turn              # by_turn（默认）| by_hits
unknown_tool: error          # error（默认）| stop | pass
check_pairing: false
meta: { source: swe-agent }  # 随便写，只用来导出
tool_map:
  bash: { name: run_shell, args: { cmd: "$command" } }
steps:
  - reply: { tool_calls: [{ name: bash, arguments: { command: ls }, id: call_1 }] }
  - reply: { text: Done. }
    expect:
      tool_results: [{ name: bash, id: call_1, contains: Cargo, is_error: false }]
      any_user: fix
```

- `name` 非空、不含 `/`、唯一；`steps` 至少一步；字段拼错会被拒绝（`deny_unknown_fields`）。
- 每个 `step` 有 `reply`（字段同剧本的[回复](scripts.md)：`text`、`tool_calls`、`reasoning`、`usage`、`finish_reason`、`stream`、`delay_ms`、`error`、`hang*`、`reset`）和可选的 `expect`。`reply` 写 `error` 就是在第 N 步注入故障，写 `hang` 就是在第 N 步卡住。
- 工具调用里的 `id` 不写时由 fakellm 生成；要让 `expect` 或 `check_pairing` 对得上就自己写。
- 注册时校验：正则写错、步骤里有 `trajectory` / `policy` 都会 400，错误信息带步骤号（`trajectory "x": step 2: ...`），已有的轨迹不会被改动。

### 文件格式：JSON、YAML、JSONL

同一条轨迹有三种写法，导入时按内容自动识别，`--trajectory` 还看扩展名（`.json`、`.jsonl` / `.ndjson`、`.yaml` / `.yml`）：

- **JSON / YAML**：一个文档；也可以是文档数组，或 `{"trajectories": [...]}`。
- **JSONL**：第一行是不含 `steps` 的表头（必须有 `kind: "trajectory"` 和 `name`），之后每行一步 `{"reply": {...}, "expect": {...}}`；空行忽略；再来一个表头就开始下一条轨迹，所以一个文件可以放多条。每行一整条带 `steps` 的 JSON 也认。适合从数据集或日志按行转换，也方便 diff。

```jsonl
{"kind":"trajectory","name":"fix-bug","tool_map":{"bash":{"name":"run_shell","args":{"cmd":"$command"}}}}
{"reply":{"tool_calls":[{"name":"bash","arguments":{"command":"ls"},"id":"call_1"}]}}
{"reply":{"text":"Done."},"expect":{"tool_results":[{"id":"call_1","contains":"Cargo"}]}}
```

控制面里 `Content-Type: application/x-ndjson`（还认 `application/jsonl`、`application/json-lines`、`text/jsonl`）按 JSONL，YAML 类型按 YAML，其余自动识别。导出用 `GET /_fakellm/trajectories/{name}`（JSON）或加 `?format=jsonl`；导出再导入得到同一条轨迹（`parse(to_json(t)) == t`），默认值 `on_divergence`、`cursor`、`unknown_tool` 总是写出来。

### 加载

```bash
fakellm --trajectory fix-bug.yaml --trajectory more.jsonl      # 启动时，可重复
curl -X PUT  $E/_fakellm/trajectories -H 'content-type: application/x-ndjson' --data-binary @fix-bug.jsonl
curl -X POST $E/_fakellm/trajectories -d @extra.json            # 追加，同名则替换（计数清零）
```

响应是 `{"added": n, "count": 总数}`。`PUT` 全部先校验，有一个不合法就一个都不改。`POST /_fakellm/reset` 清掉全部轨迹。

### 步骤怎么选

`cursor: by_turn`（默认）用请求里**已有的 assistant 回合数**当步骤号：第一个请求是第 0 步，客户端带着第一步的回复和工具结果再来，就是第 1 步。因为步骤号只看请求，所以

- 客户端重试同一个请求，得到同一步；
- 两个并行的对话互不干扰；
- Responses 请求用 `previous_response_id` 也行（fakellm 先把保存的历史接上）。

`cursor: by_hits` 用这条轨迹已经答过多少个**不同的**请求当步骤号，给**不带历史**的客户端用。请求与上一个请求完全相同（解析后的消息一样）时重放上一步而不前进，所以 SDK 的自动重试（5xx、网络错误之后）不会吃掉步骤，也可以和故障注入混用。注意：一个每轮都发完全相同请求的客户端因此永远停在第 0 步；这种客户端写 `dedupe: false` 恢复旧行为（每个请求都前进）。每条轨迹的计数独立，重新注册或 `reset` 时清零，快照会保存它。

步骤号超过 `steps` 的长度：`strict` 返回 400 `fakellm_trajectory_exhausted`，`lenient` 和 `stop` 回复 `stop_text` 并结束。

### 检查工具结果：`expect`

`expect` 检查的是**请求本身**：请求第 N 步时，最后一条 assistant 消息之后的工具结果就是第 N-1 步那些调用的结果。

| 字段 | 含义 |
|---|---|
| `tool_results` | 一个列表，每一项要对应请求末尾的一个工具结果，**各自独立**（写两项就需要两个结果）；没被任何一项提到的结果忽略 |
| `tool_results[].name` | 这个结果回答的工具名。写轨迹里的名字或经 `tool_map` 改写后的客户端名字都行；结果消息自己没有名字的协议（Chat），按 `tool_call_id` 到历史里的 assistant 调用去反查 |
| `tool_results[].id` | 精确的 `tool_call_id` |
| `tool_results[].contains` | 对结果文本的正则（`re`、`/re/`、`/re/i`；Rust regex，遇到环视和反向引用时自动改用 fancy-regex） |
| `tool_results[].equals` | 结果文本完全相等 |
| `tool_results[].is_error` | 是否是错误结果（Anthropic 的 `is_error`） |
| `any_user` | 对任意一条 user 消息的正则，匹配前会去掉 `<system-reminder>` 块 |

第 0 步一般不写 `expect`（写了 `tool_results` 它只会失败）。

### 偏离时怎么办：`on_divergence`

| 值 | 行为 |
|---|---|
| `strict`（默认） | 返回 400 `fakellm_trajectory_diverged`，消息写明期望什么、实际是什么（带结果的工具名、id 和前 80 个字符），测试直接看到差异。400 不会被官方 SDK 重试 |
| `lenient` | 忽略失败，照常回答这一步 |
| `stop` | 回复 `stop_text`（结束这次运行），不再往下走 |

实际的错误体：

```json
{"error": {"message": "fakellm: trajectory \"swe-astropy\" diverged at step 1 (by_turn: 1 assistant messages in the request): expected a tool result with name=bash id=c1 contains=convert, got tool results [run_shell#c1 \"nothing\"]",
           "type": "fakellm_error", "param": null, "code": "fakellm_trajectory_diverged"}}
```

`check_pairing: true` 额外要求历史里**每个**工具调用在后面都有同 id 的结果，否则算偏离（"tool call call_x has no result"）。这就是 codex 测试里的 `validate_request_body_invariants`，用来抓客户端丢结果或乱序的 bug。

### 工具映射：`tool_map`

数据集或录像里的工具名（比如 SWE-agent 的 `bash`、`str_replace_editor`）和被测 agent 注册的工具多半对不上。`tool_map` 在步骤发出时改写它的每个 `tool_calls`：

```yaml
tool_map:
  bash:
    name: run_shell
    args: { cmd: "$command", cwd: "/repo", note: "run: ${command}" }
    keep_rest: false
```

- **改名**：`name` 换成映射后的名字；没有映射的工具原样发出；`id` 不动。
- **改参数**（写了 `args` 才改）：按 `args` 的键重新组成一个对象。`"$field"` 原样拷贝轨迹参数 `field`（类型不变，`"$a.b"` 取嵌套字段，源不存在就省略这个参数）；`"${field}"` 嵌在更长的字符串里时插值成文本（字符串不带引号，其他值是紧凑 JSON，缺失为空）；`"$$"` 是字面的 `$`；其他值（非字符串，或不含 `$` 的字符串）是字面量。`keep_rest: true` 把没被模板拷贝的轨迹参数按原名追加在后面（同名时映射的优先）。
- 参数如果是字符串又不是 JSON 对象，没法改写，只改名。
- **客户端没有这个工具时**由 `unknown_tool` 决定，检查的是映射**之后**的名字，只检查正在回答的这一步：`error`（默认）返回 400 `fakellm_unknown_tool`，消息里列出客户端提供的工具；`stop` 回复 `stop_text` 结束；`pass` 照发不误。请求没带 `tools`（比如生成标题的旁路请求）等于什么都没提供，所以会被当成缺失，旁路请求最好用 `no_tools` 规则拦在前面，或者用 `pass`。

### 控制面

| 路径 | 作用 |
|---|---|
| `GET /_fakellm/trajectories` | 列表：`name`、`steps`、`on_divergence`、`cursor`、`unknown_tool`、`hits`、`last_step` |
| `PUT` / `POST /_fakellm/trajectories` | 替换全部 / 追加（同名替换） |
| `DELETE /_fakellm/trajectories` | 清空 |
| `GET /_fakellm/trajectories/{name}[?format=jsonl]` | 导出一条（404：不存在） |
| `DELETE /_fakellm/trajectories/{name}` | 删除一条 |

`hits` 每次请求都算（包括出错的），`last_step` 是最近一次尝试的步骤号。SDK：`llm.trajectories`（`sdk/js`、`bindings/node`）和 Go 的 `s.Trajectories()`。CLI：`fakellm ctl trajectories {list,put,add,get,delete,clear}`（见[控制面](control-plane.md#cli)）。

## 策略脚本

### 模块

```js
// 先读文件，再改文件；改失败重试一次，再失败就放弃
export default function decide(request, context) {
  const state = context.state ?? {};
  const last = request.messages.at(-1);
  const text = (last.content ?? []).map((p) => p.text ?? "").join("");
  context.log("hit", context.hits, "stage", state.stage);
  if (last.role !== "tool") {
    return { tool_calls: [{ name: "read_file", arguments: { path: "src/a.rs" } }], state: { stage: "read", failures: 0 } };
  }
  if (state.stage === "read") {
    return { tool_calls: [{ name: "edit_file", arguments: { path: "src/a.rs", patch: "-1 +2" } }], state: { stage: "edit", failures: 0 } };
  }
  if (/error/i.test(text)) {
    const failures = state.failures + 1;
    if (failures >= 2) return { text: "修改失败，放弃。", state: { stage: "gave_up", failures } };
    return { tool_calls: [{ name: "edit_file", arguments: { path: "src/a.rs", patch: "-1 +2" } }], state: { stage: "edit", failures } };
  }
  return { text: "完成。", state: { stage: "done", failures: state.failures } };
}
```

- **默认导出**一个函数（同步或 async），只支持 ES 模块语法；源码被写成临时目录里的 `policy.mjs`，可以用运行时自带的内置模块（`node:fs` 之类）。需要多个文件时用注册对象的 `files`：相对路径到源码的映射（`{"helper.js": "export const f = ...", "lib/x.mjs": "..."}`），连同 `policy.mjs` 一起写进临时目录，入口里就能 `import { f } from "./helper.js"`。路径必须是相对路径、不含 `..`、不能叫 `policy.mjs`，否则 400；临时目录里默认有 `{"type":"module"}` 的 `package.json`（`files` 里自己给一个则以它为准）。`--policy FILE` 仍然只读单个文件。
- `request` 是解析后的请求，和 `GET /_fakellm/requests?full=true` 里的 `request` 完全一样：`messages`（`role`、`content` 是 `{type, text}` 的数组、`tool_calls`、`tool_call_id`）、`tools`、`model`、`stream` 等，文本都是原文，没有去掉 `<system-reminder>`。
- `context` 有三样：`state`（上一次返回的状态，初始 `{}`）、`hits`（这个策略之前已经做过几次决定，第一次是 0）、`log(...)`（写一行日志）。
- 返回值是一个回复对象，字段同剧本的 `reply`（`text`、`tool_calls`、`error`、`usage`、`stream` 等），**可以**写 `trajectory` 让某条轨迹来回答这个请求，**不能**写 `policy`；加一个可选的 `state`。规则上的 `stream`、`delay_ms`、`hang*`、`reset`、`usage` 是策略回复的默认值。回复既没有内容、也没有 `error`、`hang` 或 `trajectory` 是 400 `fakellm_policy_error`。
- **放弃（decline）**：返回 `null`、`undefined` 或 `{decline: true}` 表示“这个请求不归我”。引擎把这条规则当作没匹配上：规则不计命中（`times` 不消耗），策略的 `hits` 不加，然后继续试后面的规则。`{decline: true, state: ...}` 仍会保存 `state`。只有策略有这个能力，轨迹没有。比如一个策略只接管带某个工具的请求，其余的落到后面的 `text` 规则。注意：放弃之后若再没有规则匹配，请求走 cassette / 上游 / 回退，和没有这条规则一样。

### JSONL 协议

fakellm 和运行时之间是按行分隔的 JSON，走子进程的 stdin / stdout。你写策略时不需要关心，它在你想自己实现一个 shim 或排查问题时有用：

```
shim -> fakellm   {"ready":true} | {"load_error":"msg"}                  启动时一次
fakellm -> shim   {"id":7,"request":{...},"state":{...},"hits":3}
shim -> fakellm   {"id":7,"reply":{...},"state":{...}}                   返回了 state 才带 state
                  {"id":7,"error":"msg"}
```

三个内置 shim（`src/policy/shim_node.js`、`shim_bun.js`、`shim_tjs.js`）都编进二进制，运行时写到临时目录。`console.log` / `info` / `debug` 被重定向到 stderr（stdout 留给协议），stderr 的每一行就是策略的日志。

### 状态

`state` 是 JSON，**替换而不是合并**：回复里带了 `state` 就整个换掉，不带就保持不变。状态存在 fakellm 里，不在脚本进程里，所以进程崩溃、被超时杀掉再重启之后状态还在（`restarts` 计数加一）。`POST` 同名策略（替换）、删除、`reset` 都会让状态归零。

同一个策略的决定是**串行**的（所以状态一致）；不同策略并行。失败的决定（超时、崩溃、抛错）不计入 `hits`，也不改状态。

### 运行时

每个策略一个常驻子进程，注册时启动并加载一次模块（试运行），之后每个请求复用，死了就在下一个请求时重启。

| 运行时 | 说明 |
|---|---|
| `tjs`（[txiki.js](https://github.com/saghul/txiki.js)） | 默认，体积小、启动快；`PATH` 里有它才会选。在 v26.6.0 上测过（见下）。官方没有 Linux 预编译包（release 只有 macOS / Windows），Linux 要自己编译 |
| `node` | 测试过 |
| `bun` | 测试过（`bun run`） |
| 其他路径 | 任意可执行文件；shim 按文件名选（`tjs`、`bun`，其余当 Node） |

选择顺序：注册时的 `runtime` 字段 > `--policy-runtime`（`FAKELLM_POLICY_RUNTIME`）> 在 `PATH` 里依次找 `tjs`、`node`、`bun`。一个都没有时注册返回 400（"no JavaScript runtime found: install node, bun or txiki.js (tjs), or pass runtime"）。

#### txiki.js 说明

- 编译：`git clone --recursive https://github.com/saghul/txiki.js && cd txiki.js && cmake -S . -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build -j8`，产物 `build/tjs`（只需要 cmake、gcc、make，不需要 ninja；约 5 分钟）。
- shim 用到的 API：`tjs.stdin.getReader()` / `tjs.stdout.getWriter()`（stdio 是 Web Streams，没有 `tjs.stdin.read(buffer)` 这种方法，早先按文档推测写的版本在第一次 `send` 就抛 `not a function`）、`tjs.env`、`tjs.exit`、顶层 `await import()`、`TextEncoder` / `TextDecoder`。策略里要立刻退出用 `tjs.exit(n)`，不是 `process.exit`。
- 测试：`FAKELLM_TJS=/path/to/tjs cargo test --test policy_tjs`（状态、命中数、日志、崩溃恢复、抛错、decline、坏模块、超时、30 万字节的请求体）。另外把 `tests/policy.rs` 的 23 个场景整体换成 tjs 运行时跑过一遍，22 个通过，剩下一个只是断言运行时名字是 `node`。

### 注册

```bash
fakellm --policy reader.mjs                    # 策略名是文件名去掉扩展名：reader
curl -X PUT $E/_fakellm/policies -H 'content-type: application/json' \
  -d '{"name": "reader", "runtime": "node", "source": "export default () => ({ text: \"hi\" })"}'
```

`PUT|POST /_fakellm/policies` 收一个 `{name, source, runtime?, files?}`、一个数组或 `{"policies": [...]}`（JSON 或 YAML）。名字只能用字母、数字、`.`、`_`、`-`。注册时会先加载一次模块：语法错误、没有默认导出函数、运行时起不来都是 400，信息里带模块自己的错误，已有的策略不会被改动。`PUT` 是全量替换（被换掉的策略的进程和状态都清掉），`POST` 同名替换。`--policy` 加载失败时 fakellm 启动失败。

### 超时与错误

| 情况 | 结果 |
|---|---|
| 超过 `--policy-timeout-ms`（默认 5000，**真实时间**，不跟虚拟时钟走）没有答复 | 杀掉进程，500 `fakellm_policy_timeout`；下个请求重启 |
| 进程退出或管道断了 | 500 `fakellm_policy_crashed`；下个请求重启 |
| 脚本抛错、返回不合法的回复、回复里有 `policy` | 400 `fakellm_policy_error`，带脚本自己的消息 |
| 名字没注册 | 400 `fakellm_policy_not_found` |

只有超时和崩溃是 500，因为 SDK 会重试 500，而重试一个有 bug 的策略没有意义。删除、`reset`、替换策略以及服务退出（包括 SIGINT / SIGTERM）都会杀掉子进程，包括正在决定中的那个，并删掉临时目录。

### 控制面

| 路径 | 作用 |
|---|---|
| `GET /_fakellm/policies` | 列表：`name`、`runtime`、`running`、`hits`、`restarts`、`state`、`log`（最近 200 行 stderr） |
| `PUT` / `POST /_fakellm/policies` | 替换全部 / 追加（同名替换，状态重置） |
| `DELETE /_fakellm/policies` | 清空 |
| `GET` / `DELETE /_fakellm/policies/{name}` | 读 / 删一个 |

### 安全

**注册策略就是在运行 fakellm 的机器上以它的用户身份执行代码**，`runtime` 还可以是任意可执行文件的路径。控制面没有认证（见[控制面的安全一节](control-plane.md#安全)），所以不要绑定 `0.0.0.0`，也不要让不可信的调用方访问控制面，不需要策略时不要用 `--policy`。官方 Docker 镜像（alpine）里没有 JS 运行时，策略在里面跑不起来；需要的话基于它做一个派生镜像（`apk add nodejs`），或者只用轨迹。

## 示例一：回放一次修复，并改写工具名

被测的 agent 有一个 `run_shell` 工具，参数是 `cmd`。轨迹按“通用的” `bash {command}` 写：

```yaml
kind: trajectory
name: fix-bug
tool_map:
  bash: { name: run_shell, args: { cmd: "$command" } }
steps:
  - reply: { tool_calls: [{ name: bash, arguments: { command: cargo test }, id: t1 }] }
  - reply: { text: 修好了。 }
    expect:
      tool_results: [{ name: bash, id: t1, contains: test result }]
```

`rules.yaml`：

```yaml
- { name: side, match: { no_tools: true }, reply: { text: ok } }
- { name: agent, reply: { trajectory: fix-bug } }
```

```bash
fakellm --trajectory fix-bug.yaml --scripts rules.yaml &
```

测试里让 agent 跑一遍，然后断言：

```bash
curl $E/_fakellm/trajectories      # hits: 2, last_step: 1
fakellm ctl requests list --source script
```

agent 没有跑 `cargo test`、或者没把输出发回来，第二个请求就是 400 `fakellm_trajectory_diverged`，消息里写着期望和实际的差别。

## 示例二：策略脚本，读—改—重试—放弃

把上面“模块”一节的脚本存成 `reader.mjs`：

```bash
echo '- { reply: { policy: reader } }' > rules.yaml
fakellm --policy reader.mjs --scripts rules.yaml &
```

agent 的每个请求会得到：`read_file` → `edit_file` →（结果带 error 就）再来一次 `edit_file` → 再带 error 就回复“修改失败，放弃。”，成功就回复“完成。”。`GET /_fakellm/policies/reader` 看得到最终的状态和 `log`：

```json
{"name":"reader","runtime":"node","running":true,"hits":4,"restarts":0,"state":{"stage":"gave_up","failures":2},
 "log":["hit 0 stage ","hit 1 stage read","hit 2 stage edit","hit 3 stage edit"]}
```

## 示例三：SWE-agent 风格的轨迹

数据集里的 SWE-agent 轨迹用 `bash` 和 `str_replace_editor`（`command: str_replace`、`path`、`old_str`、`new_str`）。转成 JSONL，每步一行，映射到被测 agent 的 `run_shell` 和 `edit_file`；客户端换了工具时停下，而不是报错：

```jsonl
{"kind":"trajectory","name":"swe-astropy","on_divergence":"strict","unknown_tool":"stop","stop_text":"换了工具，轨迹停止。","meta":{"source":"swe-agent","instance":"astropy__astropy-1234"},"tool_map":{"bash":{"name":"run_shell","args":{"cmd":"$command"}},"str_replace_editor":{"name":"edit_file","args":{"path":"$path","old":"$old_str","new":"$new_str"}}}}
{"reply":{"text":"先看一下仓库结构。","tool_calls":[{"name":"bash","id":"c1","arguments":{"command":"grep -rn 'def convert' astropy/units | head"}}]}}
{"reply":{"tool_calls":[{"name":"str_replace_editor","id":"c2","arguments":{"command":"str_replace","path":"astropy/units/core.py","old_str":"a < b","new_str":"a <= b"}}]},"expect":{"tool_results":[{"name":"bash","id":"c1","contains":"convert"}]}}
{"reply":{"tool_calls":[{"name":"bash","id":"c3","arguments":{"command":"pytest astropy/units -q"}}]},"expect":{"tool_results":[{"id":"c2","is_error":false}]}}
{"reply":{"text":"修复完成，测试通过。"},"expect":{"tool_results":[{"id":"c3","contains":"passed"}]}}
```

第二步发给客户端的是 `edit_file {"path": "astropy/units/core.py", "old": "a < b", "new": "a <= b"}`（`command` 没有被 `args` 模板引用，所以丢掉；要保留就加 `keep_rest: true`）。第一次请求时 `bash` 经映射成 `run_shell`，客户端必须有这个工具，否则按 `unknown_tool: stop` 直接以 `stop_text` 结束。Hugging Face 数据集可以用 `fakellm import hf` 批量转成这个格式（SWE-agent、OpenHands、Hermes 等），录好的 cassette 用 `fakellm cassette to-trajectory` 转，见 [cassette.md](cassette.md) 和 [import.md](import.md)。

## 驱动真实的 codex、Claude Code、opencode

`e2e/` 里的脚本起一个 fakellm，注册轨迹或策略，再让真实的 agent 去跑；需要机器上装了对应的程序，手工运行，不在 CI 里（`FAKELLM_BIN` 可以指定二进制，默认 `target/debug/fakellm`）：

| 脚本 | 做什么 |
|---|---|
| `e2e/codex-trajectory.sh` | codex（Responses API）回放三步轨迹：`exec_command` 回显、再写文件、最后答复；第二步 `expect` 回显的内容，所以通过说明 codex 真的执行了命令并把输出发了回来 |
| `e2e/claude-trajectory.sh` | Claude Code 回放轨迹：轨迹按通用的 `shell {cmd}` 写，`tool_map` 映射成 Claude Code 的 `Bash {command}`；`no_tools` 规则回答它的旁路请求 |
| `e2e/opencode-policy.sh` | opencode 由 node 策略驱动：先调一次 `bash`，再把工具输出里看到的内容写进答复（需要 node） |

接线用 `fakellm wire codex|claude|opencode`，见 [wire.md](wire.md)。agent 的旁路请求（标题、话题检测）不带工具，务必用 `match: {no_tools: true}` 的规则在前面拦住，否则它们会吃掉轨迹的步骤或让策略多算一次。

## 已知限制

- `tjs`、`node`、`bun` 的 shim 都在真实运行时上测过；`tjs` 的测试需要 `FAKELLM_TJS` 指向二进制，否则跳过。
- `fakellm ctl` 没有 trajectories / policies 子命令，用 curl、`sdk/js`、`bindings/node` 或 Go helper。
- 策略不能 import 相对路径的其他文件。
- `bindings/node` 的 `policies.put` / `add` 返回 Promise，子进程在工作线程里加载，不阻塞 JS 线程；`snapshots.restore` 还原策略时仍是同步的（要重启子进程，会阻塞）。
- `bindings/node` 的 `npm test` 只覆盖到 OpenAI 路径上的轨迹和策略；Responses 与 Anthropic 路径由 `tests/agent.rs`、`tests/policy.rs` 和 `sdk/node` 覆盖。
