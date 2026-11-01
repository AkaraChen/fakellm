# 导入 Hugging Face 数据集

`fakellm import` 把公开数据集（或你本地的一份导出）里的真实对话和 agent 轨迹转成 fakellm 的文件：[cassette](cassette.md)（按请求含义回放）或[轨迹](agent.md)（按步骤推进，扮演 agent 的模型）。数据集不打包进 fakellm，导入是一次性的离线步骤，产物是你自己仓库里的夹具文件。

两种用途：

- **真实语料**：长文本、代码块、markdown、CJK、很长的上下文，用来测 UI、流式渲染和截断。导成 cassette，`--cassette` 加载。
- **agent 轨迹**：SWE-agent、OpenHands、函数调用的多轮会话，用来测 agent 框架的工具分发、结果回传和上下文管理。导成轨迹，`--trajectory` 加载，用[工具映射](agent.md)接上你自己的工具名。

## 用法

```bash
# 从 datasets-server 取行（要网络）
fakellm import hf allenai/WildChat-1M --limit 50 -o fixtures/wildchat.jsonl
fakellm import hf nebius/SWE-agent-trajectories --limit 200 -o fixtures/swe.jsonl

# 本地文件：JSON 数组、{"rows": [...]}，或 JSONL（每行一行数据；- 读 stdin）
fakellm import file rows.jsonl --format sharegpt -o fixtures/chats.jsonl

# 回放
fakellm --cassette fixtures/wildchat.jsonl
fakellm --trajectory fixtures/swe.jsonl --scripts agent.yaml    # agent.yaml 里写 reply: { trajectory: ... }
```

`import hf DATASET`（`owner/name`）的选项：

| 选项 | 默认 | 说明 |
|---|---|---|
| `--config` | 第一个有该 split 的 config | 数据集的 config |
| `--split` | `train` | |
| `--limit` | `100` | 取多少行（每页最多 100 行，自动翻页） |
| `--offset` | `0` | 从第几行开始 |
| `--format` | `auto` | `auto`、`sharegpt`、`wildchat`、`hermes-fc`、`swe-agent`、`openhands`；`auto` 逐行看字段猜 |
| `--as` | 见下 | `cassette` 或 `trajectory` |
| `--name` | 数据集名（`/` 换成 `-`）或文件名 | cassette 的名字，或轨迹名的前缀 |
| `--model` | `imported` | 行里没有模型名时，exchange 里记的模型 |
| `-o, --output-file` | 必填 | 输出文件（JSONL），`-` 写到 stdout |

`import file PATH` 除了没有 `--config`、`--split`、`--limit`、`--offset` 之外，选项相同。
`--as` 缺省时：`sharegpt` 和 `wildchat` 导成 cassette，`hermes-fc`、`swe-agent`、`openhands` 导成轨迹；可以用 `--as` 反过来，但 cassette 里只有请求和助手回复，轨迹则带着工具映射。

导完 stderr 有一行统计：`fetched N rows (T truncated), converted C conversations -> E exchanges|S steps, skipped K (原因: n, ...)`。看不懂的行（没有助手回复、字段不对）被跳过并计数，不算失败；什么都没转出来才是错误。datasets-server 把某个单元格截断过的行（`truncated_cells`）会被丢掉，因为内容不完整。

- 受限（gated）数据集要有 token：设环境变量 `HF_TOKEN`，且账号已在数据集页面同意了条款。没有权限时报错会提示这一点。
- 被 datasets-server 限流（429）时会等一次再试。
- 测试或自建镜像时，`FAKELLM_HF_ENDPOINT` 可以换掉 `https://datasets-server.huggingface.co`。

## 格式

| 格式 | 行的形状 | 转换 |
|---|---|---|
| `sharegpt` | `conversations: [{from, value}]`，`id` | `human`/`user`、`gpt`/`assistant`、开头的 `system` 对应角色；`tool`/`observation` 作为上一个工具调用的结果 |
| `wildchat` | `conversation: [{role, content, ...}]`，`conversation_hash`，`model` | 角色照搬；`model` 是 exchange 的模型；`language`、`toxic`、`redacted`、`timestamp` 放进 `meta` |
| `hermes-fc` | `conversations: [{from, value}]` + `tools` 列 | `gpt` 轮里的 `<tool_call>{json}</tool_call>` 变成工具调用，`tool` 轮里的 `<tool_response>` 变成结果，`<think>` 变成 `reasoning`；`tools` 列成为工具定义；系统提示里的 `<tools>` 块去掉 |
| `swe-agent` | nebius：`trajectory: [{role: system\|user\|ai, text}]`；SWE-smith：`messages`（带结构化 `tool_calls`） | nebius 的 `ai` 文本里围栏代码块是动作（`bash {command}`，或 `str_replace_editor`），其余是说明文字，下一条 `user` 是观察结果；SWE-smith 的调用原样保留。轨迹带默认工具映射 |
| `openhands` | `events: [{source, action\|observation, args, content, cause}]` | `run` 变 `execute_bash {command}`，`edit`/`read` 变 `str_replace_editor`，`finish` 是最终回答，`think` 变下一条的 `reasoning`；每个 observation 是对应调用的结果 |

通用规则：没有助手回复的行被跳过；工具调用没有 id 时按 `call_1`、`call_2` 编号，对应的工具结果带同一个 id。
**cassette 产物**：每条助手消息一个 exchange，请求是它之前的全部消息（工具定义取自行里的 `tools`，没有就按用过的工具名生成只有名字的定义），所以多轮对话的每一轮都能被命中；回复里的 `usage` 全是 0。
**轨迹产物**：每行一条轨迹，名字是 `前缀-行 id`（重名加 `-2`、`-3`），每条助手消息一步，不带 `expect`；`swe-agent` 的 `tool_map` 把 `bash` 和 `str_replace_editor` 的每个参数都写了出来，要接到自己的工具上就改这份映射（见 [agent.md](agent.md)）。多条轨迹在同一个 JSONL 里。

行是什么形状取决于数据集当时的版本，转换器按已知的列名写，数据集改版后可能出现 `unrecognized row shape`，这时用 `import file` 加 `--format` 手工试，或者改写成上面的形状。

## 数据集与许可证

2026-10-31 查过 Hugging Face API，许可证以数据集页面为准。**fakellm 不分发任何数据，使用之前你要自己确认许可证和条款是否允许你这样用**（尤其是把导出的夹具提交进仓库、再分发或用于商业项目）。

| 数据集 | 内容 | 许可证 | 备注 |
|---|---|---|---|
| `allenai/WildChat-1M` | 真实用户对话 | ODC-BY | 不需要申请访问，适合做语料分布；对话里可能含用户的个人信息，别随意入库 |
| `anon8231489123/ShareGPT_Vicuna_unfiltered` | ShareGPT 对话 | 标的是 apache-2.0 | 数据来源有争议，只建议本地使用 |
| `lmsys/lmsys-chat-1m` | 真实对话 | 自定义协议，需申请访问 | 不能再分发，只能用户自己导入（要 `HF_TOKEN`） |
| `NousResearch/hermes-function-calling-v1` | 函数调用多轮对话 | apache-2.0 | 适合测 tool call |
| `Salesforce/xlam-function-calling-60k` | 函数调用 | CC-BY-4.0，需申请访问 | 单轮为主；没有专门的转换器，格式不同时需要先整理成上面的形状 |
| `nebius/SWE-agent-trajectories` | SWE-agent 完整轨迹 | CC-BY-4.0 | fake agent 的主力素材 |
| `SWE-bench/SWE-smith-trajectories` | SWE-agent 轨迹 | MIT | 同上 |
| `SWE-Gym/OpenHands-Sampled-Trajectories` | OpenHands 轨迹 | 页面上没标许可证 | 用之前要确认许可证 |

CC-BY 和 ODC-BY 要求署名；导出的文件里 `meta.source` 记了来源，再分发时请保留。

## 夹具与测试

仓库里的 `tests/fixtures/hf/` 有五份**手写的合成**数据（`sharegpt`、`wildchat`、`hermes`、`swe_agent`、`openhands`，每份三行，行里带 `"_comment": "synthetic"`），不是真实数据集的内容，只用来验证转换器的形状假设，也可以拿来试命令：

```bash
fakellm import file tests/fixtures/hf/hermes.jsonl -o /tmp/hermes.jsonl
# read 3 rows, converted 2 conversations -> 4 steps, skipped 1 (no assistant turn: 1)
```

`cargo test --test import` 在本地起一个假的 datasets-server 来测分页、gated、404、429。真正访问 Hugging Face 的那个测试默认跳过，`FAKELLM_HF_TEST=1`（受限数据集再加 `HF_TOKEN`）才会运行。因为转换器是照数据集的已知形状写的，没有对着完整的真实数据集逐行验证过，导入新数据集时先看统计里跳过了多少行。
