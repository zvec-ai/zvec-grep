<p align="right">
  <a href="./README.md">English</a> | 中文
</p>

# SWE-QA benchmark

此 benchmark 用于衡量 `zvec-grep` 对 Agent 回答代码仓库级软件工程问题的能力有何影响。发布结果中，两个 profile 使用相同的 Claude Code Agent、Claude Opus 5 模型、任务 prompt、代码仓库 commit、环境和限制：

- **Baseline：** Claude Code 使用其标准工具。
- **zvec-grep：** 同一个 Agent 获得准备好的代码仓库索引，并通过 MCP 使用 zvec-grep。

索引构建单独测量，不计入 Agent 执行耗时。

## Benchmark 定义

本 benchmark 使用 [`peng-weihan/SWE-QA-Bench`](https://github.com/peng-weihan/SWE-QA-Bench) 中固定的 20 个任务子集。Benchmark 输入锁定在此目录中：

- [`selection.json`](zg_bench/swe_qa/data/selection.json) 记录任务 ID、任务 slug、代码仓库 commit、资源哈希和 runner tier 成员关系。
- [`references.json`](zg_bench/swe_qa/data/references.json) 包含独立评审器使用的隔离参考答案，Agent 无法访问该文件。
- [`datasets/`](datasets/) 包含锁定的 Harbor 任务环境、prompt 和 verifier。
- [`swe-qa-bench.yaml`](suites/swe-qa-bench.yaml) 将本地数据集提供给 benchmark runner。

下文的验证命令会在模型运行前检查锁定的任务选择、代码仓库 commit、哈希以及参考答案的隔离状态。

## 发布测试配置

- **覆盖范围：** 20 个检索密集任务，覆盖 What、Where、How、Why、
  8 个意图和 11 个代码仓库。
- **Agent：** Claude Code `2.1.212`。
- **模型：** Claude Opus 5（`claude-opus-5`），high 推理强度。
- **Treatment Embedding：** Qwen3.7 Text Embedding
  （`qwen/qwen3.7-text-embedding`）。
- **Embedding Endpoint：** 使用下方本地配置中给出的 Qwen OpenAI-compatible
  endpoint。
- **运行次数：** 每个任务、每个 profile 独立运行 3 次。
- **预算：** 每个任务/profile 最多 USD 4.00。

Baseline 与 zvec-grep 除工具访问外保持完全相同。索引构建单独计时，参考答案对两个 profile 均不可见。

## 指标与报告

Job Summary 单元格使用 `baseline / zvec-grep / change`。每个任务的 baseline 和 zvec-grep 数值是对应 profile 配置次数的 trial 平均值（CI 为五次）。

| 指标 | 变化 | 含义 |
| --- | --- | --- |
| Judge | `zvec-grep - baseline` | 正值表示评审质量更高。 |
| `input_token`、`toolcall`、Agent 执行耗时 | `(zvec-grep - baseline) / baseline` | 负值表示资源用量更低。 |

如果 baseline 分母为零，对应的效率比较结果为 `N/A`。索引设置时间与 Agent 执行耗时分开统计。

新 OpenCode trial 开启 `collect_session_usage=true`。适配器在环境停止前导出主会话及递归关联的全部子代理会话，保存为 `agent/session-usage.json`。采集要求导出完整；会话缺失、导出失败或总数不一致时 trial/报告失败，不能静默回退到主轨迹。报告标记 `opencode-session-tree-v1` 统计口径。JSON 报告和会话产物保留主会话、子代理的用量拆分；Markdown 报告移除 descendants 表格，但总量仍完整计入子代理用量。

- 输入 token 包含非缓存输入、缓存读取和缓存写入；输出 token 包含文本输出及 reasoning，并分别保留组成字段。
- 工具数包含主会话的委派调用和子代理内部调用，递归覆盖更深层子代理；使用会话、消息和工具标识去重。
- Agent 墙钟时间已经包含等待子代理的执行时间，不能再累加子代理耗时；采集导出的实测耗时会扣除。
- Judge 仍只评分主会话的最终答案，其消耗单独统计。
- 完整导出表示已持久化的任务会话完整，不代表完整账单：未写入会话数据库的后台标题/摘要调用无法计入；缺失的供应商费用仍记为未知。

没有会话导出的历史数据保留为 `legacy-root`，其子代理 token 和工具用量未知，可能漏计。采集和报告聚合均拒绝将它与新口径混用，包括没有标记统计口径的旧报告。旧版输入/输出 token 的组成也不同，不能把新旧资源差值当作同口径比较。失败尝试仍单独保存在 `.retry-history/`，不计入最终成功 trial 的均值。

Aggregate 汇总放在最前面，先于任务明细。每轮 workflow 独立筛选任务，两个条件均使用对应 profile 的 trial 平均值：

- Judge 分差：`zvec-grep - baseline`，单位为分。分差严格超出 `[-10, +10]` 的任务会被排除；恰好等于 `-10` 或 `+10` 仍保留。这里比较的是评分分差，不是百分比。
- 输入 token 变化率：`(zvec-grep - baseline) / baseline * 100`。变化率严格超出 `[-100%, +100%]` 的任务会被排除；恰好等于 `-100%` 或 `+100%` 仍保留。输入 token 非负，因此实际只有涨幅超过 `100%` 能触发这个阈值。baseline 为零、zvec-grep 大于零时，变化率未定义，该任务也会被排除；两者均为零则保留。

满足任一排除条件的任务，会从所有 Aggregate 指标和主结果表中整项移除；同时满足两个条件的任务只计为一项。Judge 分差超限的任务会单独列表，展示 baseline 与 zvec-grep 的平均评分、分差和排除原因；报告也会列明因输入 token 条件被排除的任务。

- Judge 数值是保留任务的等权平均值。
- Baseline 和 zvec-grep 的效率数值是保留任务各 profile 平均值之和；展示的变化率直接由这两个汇总值计算，不再平均任务级百分比。
- 某项效率指标的汇总 baseline 分母为零时，变化率为 `N/A`。若没有任务保留，则 Aggregate 各项数值均为 `N/A`。
- JSON 的 `cases` 及原始产物仍保留全部任务和子代理用量，包括被排除的任务。

这是敏感性筛选，不能据此认定被排除的数据有误。所有选中任务仍须执行，并完成规定的 trial 和评审调用；筛选不能让未完成的 benchmark 通过。`all-full` 每轮仍运行 20 题 × 2 个 profile × 5 次。若任务均已完成，仅因筛选后没有剩余任务，工作流仍可成功。

## GitHub Actions

[SWE-QA Bench 工作流](../../.github/workflows/swe-qa-bench.yml) 仅支持通过 `workflow_dispatch` 手动运行，push 和 PR 均不触发。核心维护成员限定为仓库角色明确为 `admin` 或 `maintain` 的用户。每个 job 的第一步都会实时检查 `github.actor` 和 `github.triggering_actor` 两人的当前角色，通过后才 checkout 和使用模型密钥；单独重跑某个 job 也会重新检查。权限不足或权限查询失败时停止执行。GitHub 的 write 用户可能仍能点击手动运行或重跑按钮，但工作流会拒绝未授权的 benchmark 执行。

`workflow_dispatch` 默认选择 `repro-3`（3 题），也可选择 `all-full`（20 题）或 `smoke`（5 题）。`model` 输入默认是 `glm-5.2`，也可选择 `qwen3.8-max`；所选模型同时用于执行和评审。

`repro-3` 固定运行 3 题 × 2 个 profile × 5 次 = 30 个 trial，供小规模复现与迭代。任务按[运行 35206585943](https://github.com/Cuiyus/zvec-grep/actions/runs/35206585943) 中的调查路径和评分波动选取。下表范围均来自该轮最终成功的五次测试：

| 相对波动 | 任务 | Baseline 评分 / 工具次数 | zvec-grep 评分 / 工具次数 |
|---|---|---|---|
| 低 | `reflex:6` | 87–96 / 7–12 | 84–92 / 3–5 |
| 中 | `requests:16` | 68–97 / 2–6 | 75–99 / 7–10 |
| 高 | `conan:39` | 5–92 / 2–24 | 5–100 / 3–19 |

此分层是历史样本的描述，不是统计显著性标准，也不是模型的固有随机性等级。低波动组仍存在路径变化；高波动组的一次 baseline 使用子代理，主轨迹未展开其内部调用。新一轮应与历史同三题比较，失败尝试的开销单独计入。

CI 默认使用 OpenCode `1.18.4`、`custom-openai/glm-5.2` 和本地 Embedding 模型 `local/potion-code-16m-v2`，每个任务、每个 profile 独立运行 5 次。请在仓库的 Actions secret 中配置 `GLM_API_KEY`，用于 Agent 执行和评审。两个模型沿用同一个 secret 名称；其中的百炼业务空间 API Key 需要具有所选模型的调用权限。上文的 Claude Code 配置对应已发布的本地测试协议。

完整运行包含 20 题 × 2 个 profile × 5 次 = 200 个独立 trial。CI 通过 `--max-retries 2` 为异常失败（包括 Agent 超时）的 trial 最多额外重试 2 次；API 使用额度耗尽不重试。成功的 trial 和低分答案不重跑，重试次数不计入每组 5 次的样本数。本地运行默认不重试，可显式传入 `--max-retries` 开启。

每次失败的日志和轨迹保存在 Harbor job 的 `.retry-history/` 下，并随原始证据上传；Job Summary 展示重试次数和最终错误数。报告中的 token、工具调用、耗时及费用仅统计每个 trial 最终成功的那次执行，不包含失败尝试的额外开销；这些开销可在归档证据中查看。用尽重试次数仍失败时，该任务保持失败。每个任务 job 的总时限为 6 小时，包含准备和重试时间。

OpenCode 的两个 profile、子代理和评审统一使用所选模型，并请求以下参数，常量位于 `zg_bench/settings.py`：

| 参数 | Qwen3.8 Max / GLM-5.2 执行与评审请求值 |
| --- | --- |
| `temperature` | `0` |
| `seed` | `42` |
| `enable_thinking` | `true` |
| `reasoning_effort` | `"high"` |
| `max_tokens` | `32000` |
| `response_format` | 不传入 |

所有 trial 和重试使用同一个 seed。custom-openai 下的 Qwen3.8 Max、GLM 配置，以及 DashScope 下的 GLM 配置均设置 `reasoningEffort = "high"`，OpenAI-compatible SDK 会将其映射为 HTTP 请求中的 `reasoning_effort`。执行模型显式声明 `limit.output`，评审请求的 `max_tokens` 使用同一个 `BENCHMARK_MAX_OUTPUT_TOKENS` 常量。

[百炼 Chat Completions 文档](https://help.aliyun.com/zh/model-studio/qwen-api-via-openai-chat-completions) 说明了模型间的参数差异：Qwen3.8 Max 在思考模式下会将低于 `0.6` 的温度自动调整为 `0.6`，并将 `reasoning_effort="high"` 映射为 `xhigh`。因此此配置下 Qwen 的**实际温度不是零**。此外，`max_tokens=32000` 对 Qwen 只限制最终回答，不包含思考；对未设置 `thinking_budget` 的 GLM-5.2 则限制思考与回答的总和。本 benchmark 保留相同请求字段，但两个模型的实际采样设置和 token 预算并不完全相同。

Qwen3.8 Max 默认开启 `preserve_thinking`。OpenCode 模型配置设置 `interleaved: {"field": "reasoning_content"}`，将历史思考内容通过独立的 `reasoning_content` 字段回传，不拼入回答正文。参见[深度思考用法](https://help.aliyun.com/zh/model-studio/deep-thinking)和[Qwen3.8 Max 模型信息](https://help.aliyun.com/zh/model-studio/qwen3-8-max)。原有独立的 Qwen3.7 配置仍保持关闭 thinking。

执行和评审请求均不传入 `response_format`。评审提示词仍要求 JSON，并保留严格的评分解析与格式错误重试；报告元数据记录 `response_format = null`，表示请求未强制响应格式。

OpenCode 配置同时声明模型支持 temperature，并为所有内置 agent（包括子代理、上下文压缩及标题/摘要生成）设置温度，通过各 agent 的 provider options 传递 seed。评审报告记录温度、seed、thinking、推理强度、输出上限及响应格式；聚合时拒绝混用不一致的评审配置，也拒绝将新报告与缺少新增配置字段的旧报告混用。配置兼容的旧报告仍可彼此聚合。早期报告使用关闭 thinking 的评审模型，其分数不应直接混入新的评审口径。这些设置用于降低采样波动，响应能否完全一致仍取决于模型服务。

两个 OpenCode profile 在全局及全部内置 agent（包含子代理）统一禁止 `websearch` 和 `webfetch`。即使 Harbor 跳过交互式权限确认，这两个工具也不会出现在发给模型的工具列表中。本地仓库搜索、文件读取和 zvec-grep MCP 仍可使用。这是网页工具限制，并非容器网络隔离；shell 命令及安装、模型连接仍可访问网络。

CI 还会使用锁定版本的 OpenCode 连接本地模拟服务，验证连续工具调用请求中的采样参数、开启 Qwen3.8 Max / GLM thinking、实际 HTTP 字段 `reasoning_effort = "high"`、网页工具限制，以及 Qwen 历史 `reasoning_content` 字段的回传，不需要模型凭证。

每个任务在同一个 runner 上运行 Baseline 和 zvec-grep，对配对结果进行评审，并将 Harbor 运行证据和独立任务报告上传为 artifacts。全部任务完成后生成聚合报告；部分任务失败时，Summary 仍展示已完成任务的报告，不将不完整结果作为完整 benchmark 聚合。不同 GitHub run attempt 的报告保持隔离，自动 trial 重试发生在同一 attempt 内。

## 本地配置

Harbor 使用 Docker 运行锁定的任务环境。为了得到可比较的结果，请保持主机平台、Claude Code 版本和模型服务配置一致。

安装以下前置依赖：

- [uv](https://docs.astral.sh/uv/)
- Docker Engine 或 Docker Desktop，并支持 Docker Compose v2
- Node.js 22 或更新版本以及 npm

验证 Docker Compose、安装锁定的依赖，并导出 Claude 与 Embedding 凭证：

```sh
docker compose version
npm ci

cd benchmarks/swe-qa-bench
uv sync --frozen
source .venv/bin/activate
export ANTHROPIC_API_KEY="your-anthropic-api-key"
export ZVEC_GREP_API_KEY="your-qwen-embedding-api-key"
export ZVEC_GREP_EMBEDDING_ENDPOINT="https://llm-67x4s810wr6kl2i4.cn-beijing.maas.aliyuncs.com/compatible-mode/v1/embeddings"
```

先验证锁定资源，再运行 profile-aware 预检：

```sh
python -m zg_bench.swe_qa validate \
  --selection zg_bench/swe_qa/data/selection.json \
  --references zg_bench/swe_qa/data/references.json \
  --dataset datasets
```

```sh
zg-bench doctor \
  --agent claude-code \
  --model claude-opus-5 \
  --profile all \
  --embedding-model qwen/qwen3.7-text-embedding \
  --embedding-endpoint "$ZVEC_GREP_EMBEDDING_ENDPOINT" \
  --zvec-grep-package ../..
```

本地 package 路径依赖上文在代码仓库根目录运行的 `npm ci`。

## 本地 smoke test 和 dry run

使用以下命令查看锁定的任务选择：

```sh
zg-bench list tasks swe-qa-bench --tier smoke
zg-bench list tasks swe-qa-bench --tier full
```

首先 dry run 5 个任务、每个 profile 三次 trial 的配置：

```sh
zg-bench run swe-qa-bench \
  --tier smoke \
  --agent claude-code \
  --model claude-opus-5 \
  --profile all \
  --n-attempts 3 \
  --embedding-model qwen/qwen3.7-text-embedding \
  --embedding-endpoint "$ZVEC_GREP_EMBEDDING_ENDPOINT" \
  --zvec-grep-package ../.. \
  --dry-run
```

移除 `--dry-run` 即可执行 5 题 smoke run；将 `--tier smoke` 改为
`--tier full` 可运行发布测试对应的 20 题配对 Agent 协议。Runner 会为
Baseline 和 zvec-grep 同时固定 Claude Code `2.1.212`、Claude Opus 5、high
推理强度和每个 profile USD 4.00 预算。Harbor 轨迹和 verifier 输出写入
`runs/`；该命令不会自动重建发布结果中的 LLM Judge 与聚合报告。

## 诊断失败的运行

当 trial 记录异常时，`zg-bench` 会输出结构化的 Agent 或 zvec-grep 配置错误，并以非零状态退出。使用以下命令检查保存的 run：

```sh
zg-bench diagnose --latest
zg-bench diagnose <job-name>
```
