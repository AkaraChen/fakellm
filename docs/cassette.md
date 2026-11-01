# Cassette：录制、回放与上游代理

cassette 是一组录好的请求/回复交换（exchange）。没有剧本规则命中的请求会先在已注册的 cassette 里按**含义**查找（不是比对字节），命中就回放；没命中再走上游（如果配置了），最后才是默认兜底或 `--strict` 的 400。
剧本规则永远排在 cassette 前面，所以可以在回放之上叠加故障注入。嵌入请求（`/v1/embeddings`）从不走 cassette。

## 文档格式

JSON、YAML 或 JSONL（第一行是头，后面每行一个 exchange），扩展名决定格式，其他按内容判断：

```json
{
  "kind": "cassette",
  "name": "weather",
  "ignore": ["!max_tokens", "system"],
  "normalize": [{"pattern": "Today is \\w+", "replace": "Today is <day>"}],
  "normalize_defaults": true,
  "match_tool_schemas": false,
  "model_glob": "*",
  "fuzzy": false,
  "once": false,
  "exchanges": [
    {"request": {"model": "gpt-4o", "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]},
     "outcome": {"outcome": "reply", "reply": {"text": "hello"}}}
  ]
}
```

导入 Hugging Face 数据集产出的也是这个格式，见 [import.md](import.md)。`request` 就是请求日志 `?full=true` 里看到的 IR 请求，`outcome` 是 `reply` 或 `error`。录制的和导入的文件都是这个格式，可以手改。文件头里的 `name` 缺省时取文件名（去掉扩展名）；`recording` 是内存里录制的保留名，不能用来注册。控制面和 SDK 的 `cassettes.put` / `add` 收同样的内容（JSON、YAML 或 JSONL 文本）。

## 怎么匹配

两边（录好的请求和进来的请求）先变成同一种规范形式，再比较是否相等：

- 消息按角色和文本比较；多个文本块各自比较，`["a","b"]` 和 `["a\nb"]` 不同；图片按 URL、音频按内容的哈希比较。
- 工具调用比名字和参数（参数按 JSON 解析，键顺序和空白无关）；**工具调用 id 不参与比较**（每次运行都不同），工具结果按位置和文本比较。
- 工具列表默认只比名字；`match_tool_schemas: true` 时连参数 schema 一起比。
- `model` 不参与比较，用 `model_glob` 限定 cassette 适用的模型。
- 采样参数默认忽略：`temperature`、`top_p`、`seed`、`user`、`metadata`、`stream`、`include_usage`、`store`、`max_tokens`、`reasoning_effort`、`tool_choice=auto`、`previous_response_id`、`extra.*`。`ignore` 在默认之上追加，`!字段` 去掉一个默认；还可以忽略 `n`、`stop`、`parallel_tool_calls`、`thinking`、`response_format`、`tool_choice`、`extra.<key>`，以及两个伪字段 `system`（不比系统消息）和 `tools`（不比工具列表）。exchange 上也可以有自己的 `ignore`。

### 归一化

文本比较前先过一组正则替换。默认的几条（`normalize_defaults: false` 全部关掉）：

| 匹配 | 替换为 |
|---|---|
| ISO 8601 时间戳（`2026-11-01T10:00:00Z`） | `<timestamp>` |
| 单独的日期（`2026-11-01`，客户端常把“今天是……”写进系统提示） | `<date>` |
| UUID | `<uuid>` |
| 形如 `req_` 加 16 个以上字母数字的 id | `<req_id>` |

`normalize` 里写自己的规则，追加在默认之后，对每条消息文本、工具结果和工具参数里的字符串生效。常用的：
`{"pattern": "\\b1[5-9]\\d{8}\\b", "replace": "<epoch>"}`（epoch 秒）、`{"pattern": "/tmp/[A-Za-z0-9._-]+", "replace": "/tmp/<tmp>"}`、`{"pattern": "\\b[0-9a-f]{40}\\b", "replace": "<sha>"}`、`{"pattern": "(?s)<system-reminder>.*?</system-reminder>", "replace": ""}`。
如果两个只差一个日期的提示本来就该得到不同回复，用 `normalize_defaults: false`。

### 查找顺序

1. 按注册顺序看每个 cassette：模型符合 `model_glob`，且有 key 相等的 exchange（`once` 的跳过已用掉的），第一个命中的 cassette 和 exchange 胜出。
2. 没有精确命中时，再看 `fuzzy: true` 的 cassette：消息条数相同、最后一条用户文本相同，并且**最后一条消息也相同**；从末尾往前相等的消息越多得分越高，同分取靠前的。
3. 都没有：走上游，或兜底，或 strict 的 400。没命中不是错误。

`once: true` 的 cassette 每个 exchange 只回放一次，同一个请求录了两次不同回复时，按录制顺序依次回放，用完后才落空。不加 `once` 则同一个 exchange 可以回答任意多次。
回放按**请求的协议**和流式与否渲染，所以用 Chat 录的可以用 Responses 或 Anthropic 回放。请求日志里这类请求的 `source` 是 `{"kind":"cassette","name":"...","index":N}`，可以用 `?source=cassette` 过滤。

## 上游代理与录制

```bash
fakellm --upstream https://api.openai.com --upstream-api-key $OPENAI_API_KEY --record chats.jsonl
```

没有剧本和 cassette 命中的请求会用**请求自己的协议**转发给上游，响应原样回给客户端，同时重新拼成回复并记录：追加进内存里的 `recording` cassette，并写入 `--record` 文件（JSONL，第一行头，每个 exchange 一行）。

- `--upstream` 是 `http://` 或 `https://` 的地址，可带路径（`/v1` 不会重复）和查询串（会合并进转发的 URL，如 Azure 的 `api-version`）。**地址里不能带用户名密码**，密钥用 `--upstream-api-key`（`FAKELLM_UPSTREAM_API_KEY`）给。查询串不会出现在记录文件的头、状态接口和错误信息里。
- 不给 `--upstream-api-key` 时，客户端自己的凭证原样转发（fakellm 自己的 `--api-key` 除外，它不会外传）。
- `--record` 追加写已有文件，可以跨多次运行继续录；若上次中途被杀导致最后一行不完整，会先补一个换行再追加。
- **默认不录** 401、403、408、409、429 和 5xx 的回答（仍然会转发给客户端）：它们与随后重试成功的请求 key 相同，录下来会在回放时盖住成功的那次。要录就加 `--record-errors`。
- 上游响应体超过 64 MiB 时照常转发但不录；上游 5 分钟没有任何数据则断开并记为未录制。
- 录制文件里不含凭证（请求头不录），但**请求和回复的内容原样保存**，别把含敏感内容的录制提交进仓库。详见下面的[脱敏](#脱敏)。
- 上游迟迟不发响应头时（非流式的长推理最典型）最多等 `--upstream-timeout` 秒（`FAKELLM_UPSTREAM_TIMEOUT`，默认 600，0 为不限），超时返回 502 `upstream_unreachable`。
- 内存里的 `recording` 最多保留最近 10000 个 exchange（`--record` 文件不截断）。
- 回环保护：转发带 `x-fakellm-hop`，上游指回自己或环状连接返回 508。
- `GET /_fakellm/cassettes/recording` 导出内存里的录制，`?format=jsonl` 得到 JSONL。


### 用 CLIProxyAPI 当上游

[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) 把 Claude Code、Codex、Gemini CLI 等订阅账号包装成 OpenAI、Anthropic、Responses 兼容的接口，用它当上游就能把真实 agent 的往返录下来，之后离线回放（它的默认地址是 `http://127.0.0.1:8317`，密钥是它配置里 `api-keys` 的某一项，具体以它自己的文档为准）：

```bash
# 1. 录制：fakellm 夹在 agent 和 CLIProxyAPI 之间
fakellm --port 4010 --upstream http://127.0.0.1:8317 \
        --upstream-api-key $CLIPROXY_KEY --record fixtures/fix-bug.jsonl
# agent 照常接线，请求原协议转发
eval "$(fakellm wire claude --endpoint http://127.0.0.1:4010)"   # codex、opencode 见 wire.md
claude -p "fix the failing test"

# 2. 回放：不需要 CLIProxyAPI，不需要密钥，也不需要网络
fakellm --port 4010 --cassette fixtures/fix-bug.jsonl --strict
```

不加 `--upstream-api-key` 时 agent 自己的凭证原样转发，适合上游认的就是 agent 的密钥的情形。agent 的主请求之外还有标题生成之类的旁路请求，它们一并录进去，回放时因为内容相同同样命中；想把整段会话变成按步骤推进的轨迹，见[转成轨迹](#转成轨迹)。

### 凭证怎么转发

| `--upstream-api-key` | 客户端带了 | 上游收到 |
|---|---|---|
| 设了 | 任意 | 客户端的 `authorization` / `x-api-key` 都不转发；发 `authorization: Bearer <key>`，Anthropic 请求另加 `x-api-key: <key>` |
| 没设 | `authorization` 和/或 `x-api-key` | 原样转发（fakellm 自己的 `--api-key` 的值除外，它不会外传） |
| 没设 | 都没有 | 都没有（多半是上游的 401） |

`anthropic-version` 缺省时补 `2023-06-01`，`anthropic-beta`、`openai-organization` 等其他头原样转发。请求体原样发给上游，不从 IR 重新生成。

### 脱敏

录制只保存请求体和回复内容，不保存任何请求头。除此之外，请求体里**不认识的顶层字段**（IR 的 `extra`）如果名字里含 `api_key`、`apikey`、`authorization`、`token`、`secret` 或 `password`（不分大小写，`-` 当 `_`），值在录制里写成 `<redacted>`。匹配默认忽略 `extra.*`，所以这不影响回放。**消息文本、工具参数、工具结果不做脱敏**：如果 agent 把密钥读进了对话（`cat .env`），它就在录制文件里。录完检查一遍再入库，或者用 `normalize` 在比较时抹掉可变部分（那只影响匹配，不改文件）。

把录制文件用 `--cassette chats.jsonl` 加载就是回放。`--cassette` 的文件和 `--record` 是同一个文件时，这个文件不会被热加载（否则每录一条就会重置命中计数）。


## 回放顺序：剧本、cassette、上游、兜底

一个请求按下面的顺序找回答者，谁先有答案谁赢：

1. **剧本规则**（含轨迹和策略，`reply: {trajectory: ...}` 之类）。所以可以在回放之上叠加故障：先写一条 `probability: 0.2` 的 429 规则，没命中时请求继续落到 cassette。
2. **cassette**（按注册顺序，精确命中优先，然后才是 `fuzzy`）。
3. **上游**（配置了 `--upstream` 时）：转发并录制。
4. **兜底**：默认的 lorem 回复，或 `--strict` 的 400。

由此：cassette 命中的请求不会到达上游，也不会再被录一遍；`--strict` 配 `--cassette` 是“只许回放”的好用法，缺一条录制就立刻 400（`fakellm_no_matching_rule`）。`/v1/embeddings` 不走 cassette。请求日志的 `source` 能看出每个请求是谁答的（`script` / `cassette` / `upstream` / `fallback` / `strict`），见 [control-plane.md](control-plane.md#请求日志)。

## 跨协议回放

cassette 里存的是 IR（见上面的格式），不是原始字节。用 Chat Completions 录的 exchange，可以被 Responses 或 Anthropic Messages 请求命中：匹配只看规范化之后的消息和工具，回复再按**进来的请求**的协议、流式与否渲染（Anthropic 回 `message`，Responses 回 `response` 对象，流式则是对应的 SSE 事件）。反过来也一样。`reasoning`、工具调用、错误和 usage 都跟着走。协议专有的细节不保证还原（例如 Anthropic 的 `thinking` 签名、Responses 的 `encrypted_content` 只有录下来的才有），缺的按各协议的默认值补。

## 转成轨迹

```bash
fakellm cassette to-trajectory recorded.jsonl -o agent.jsonl --with-expect
```

按顺序取出开启一段对话的那些 exchange（`assistant` 轮数等于已取步数），旁路请求（标题生成等）和重试会被丢掉，结果以 `cursor: by_turn` 回放。`--keep-errors` 保留错误步，`--model GLOB` 只取匹配的模型。

## 快照

`POST /_fakellm/snapshots` 把剧本（含命中计数）、轨迹进度、策略（含状态）、cassette（含已用标记和录制）、保存的 Responses 对象和时钟存下来，`POST /_fakellm/snapshots/{name}/restore` 原样放回，快照不会被消耗。请求日志、启动配置和其他快照不在其中。适合在测试之间使用，不要在有请求在飞时还原。名字是 1 到 64 个 `A-Za-z0-9._-` 字符，不给则是 `snapshot-N`。`fakellm ctl snapshot create|list|restore|delete`。
快照里的 cassette 是共享的，不会复制；每个快照仍然占用与其内容相当的内存，用完请删除（`reset?snapshots=true` 会删掉全部）。

## 热加载

`--scripts`、`--trajectory`、`--policy`、`--cassette` 给的文件每 500 ms 检查一次修改时间和长度，变了就重新读整类文件并替换对应的注册表，命中计数从头开始（策略会重启）。文件读不了或内容无效时保留旧的并打一条警告，内容不变不会重试。`--no-watch` 关闭。重新加载会替换通过控制面注册的同类内容。

