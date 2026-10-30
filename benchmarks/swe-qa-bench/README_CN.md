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

Job Summary 单元格使用 `baseline / zvec-grep / change`。每个任务的 baseline 和 zvec-grep 数值是对应 profile 配置次数的 trial 平均值（CI 筛选前为十次）。

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

Aggregate 展示三层结果：**原始值（Raw）**、**得分截尾后、整题筛选前**、**MAD 整题筛选后**。每层同时列出得分、时间、input token 和工具调用，以及各 profile 样本数和最终题集覆盖率。逐题表保留全部已完成题目并标注整题筛选状态；排除原因列出触发的 profile、指标、离散度、median、MAD 和 modified Z。

- 按「模型 × 题目 × profile」分别处理原始成功样本。按 `(judge.total, trial_index)` 升序排列，一次性去掉首尾各一个 trial。缺少 `trial_index` 时使用从 1 开始的原始位置。同分按 trial 顺序处理：低分端去掉编号最小的一次，高分端去掉编号最大的一次，不根据时间或 token 挑选。
- 每组 10 次保留 8 次。这是固定数量截尾，不是固定百分比或离群阈值；即使所有得分相同，也会去掉两次不同的执行。例如 `[90, 87, 87, 56, 88]` 去掉 56 和 90 后保留 `[87, 87, 88]`。少于 3 次时不截尾，全部保留，标记为 `unavailable_insufficient_trials`，并继续展示实际样本数。
- 剔除某次 trial 时，其得分、时间、输入/输出 token、工具调用、成本及会话用量一起退出均值；不依据时间或 token 另外剔除单次 trial。原有按组间分差 ±10 分或 token 变化 ±100% 剔除整题的规则仍然移除。
- 基于截尾后的同批样本，计算每题每组的得分样本标准差，以及时间、input token 的变异系数 CV（样本标准差 / 均值）。对每项指标，取**当前报告各 profile 中的最大离散度**；再在题目之间计算这些最大值的 median，以及 `MAD = median(abs(最大离散度 - median))`。
- 任一可计算指标满足 `modified Z = 0.6745 × (最大离散度 - median) / MAD > 3.5`，便将该题从**所有 profile、所有最终指标**中同时排除，保持共同题集。只做一次上尾筛选，不剔除低离散度，也不在排除后重算参考。默认 CI 使用当轮 baseline / zvec-grep 两组；历史两轮三组的六组回放需要显式输入，不能暗中混入当前 CI。
- 不足 5 题时不做整题筛选；这是工程保护，并非官方统计样本阈值。某指标只要有任一题、任一 profile 缺少有效离散度（如无逐次证据、样本不足、资源均值为零），或跨题 MAD 为零，该指标整套筛选即不可用；其他可用指标仍可执行。报告记录不可用状态及关联题目，不将缺失值当成零离散度。
- 每层先计算每题每组均值，再对该层题目等权平均。前两层涵盖全部已完成题目，最终层仅使用共同保留题集。质量与资源指标口径一致；资源值为均值而非总和，变化率由展示的聚合值计算。
- 报告 schema 4 保留原始 `cases`、`trials` 及 profile 均值。`aggregate.raw` 对应第一层，`aggregate.trimmed` 对应第二层，`aggregate.profiles` / `comparison` 对应最终层。`aggregate.filter` 记录得分截尾；`aggregate.task_filter` 记录整题判定、参考统计量和三阶段样本数；`aggregate.task_statistics` 保留全部逐题均值与离散度。旧报告从原始 trial 重新计算；缺少逐次证据的历史数据明确标记为未筛选，样本数显示 `N/A`。

[NIST 的 modified-Z 说明](https://www.itl.nist.gov/div898/handbook/eda/section3/eda35h.htm) 使用绝对值超过 3.5 标记候选离群值。本 benchmark 将其上尾用于任务离散度筛选，属于报告规则，不是 p 值，也不证明被排除执行无效。最终均值描述保留题集，原始与仅得分截尾后的结果继续展示。实际 Judge 调用、费用及完成性门槛保留全部原始 trial 和任务；两阶段筛选均不能掩盖超时、重试耗尽或缺失任务，也不会补跑替代样本。`all-full` 每个模型/embedding 配置运行 20 题 × 2 个 profile × 10 次 = 400 个 trial。

## GitHub Actions

开发者可参考 [fork 运行说明](docs/run-in-fork_CN.md) 配置 API Key、百炼 endpoint、Rust 候选 ref，并手动触发测试。

[SWE-QA Bench 工作流](../../.github/workflows/swe-qa-bench.yml) 支持通过 `workflow_dispatch` 手动运行，或在手动启动 [ZG Bench](../../.github/workflows/zg-bench.yml) 时明确设置 `run_e2e: true`。push、PR 自动检查和 `@zg-bench` 评论均不会开启 e2e。核心维护成员限定为仓库角色明确为 `admin` 或 `maintain` 的用户。每个 job 的第一步都会实时检查 `github.actor` 和 `github.triggering_actor` 两人的当前角色，通过后才 checkout 和使用模型密钥；单独重跑某个 job 也会重新检查。权限不足或权限查询失败时停止执行。GitHub 的 write 用户可能仍能点击手动运行或重跑按钮，但工作流会拒绝未授权的 benchmark 执行。

独立的 [SWE-QA Offline 工作流](../../.github/workflows/swe-qa-offline.yml) 会在相关路径的 PR 和 `main` push 上运行 Python 与共享 Node 回归测试。它不读取模型密钥、不构建 candidate，也不运行 Harbor 或模型 trial。

`workflow_dispatch` 默认选择 `repro-3`（3 题），也可选择 `all-full`（20 题）或 `smoke`（5 题）。`candidate_ref` 默认是 `main`，指定要单独检出并打包的 Rust 源码分支、标签或提交；benchmark harness 与候选源码分开。`model` 输入默认是 `glm-5.2`，也可选择 `qwen3.8-max`；所选模型同时用于执行和评审。独立的 `embedding` 输入默认是 `local`，对应 `local/potion-code-16m-v2`；选择 `remote` 时使用与 Retrieval-only 工作流一致的 `qwen/qwen3.7-text-embedding`。

`repro-3` 固定运行 3 题 × 2 个 profile × 10 次 = 60 个 trial，供小规模复现与迭代。任务按[运行 35206585943](https://github.com/Cuiyus/zvec-grep/actions/runs/35206585943) 中的调查路径和评分波动选取。下表范围均来自该轮最终成功的五次测试：

| 相对波动 | 任务 | Baseline 评分 / 工具次数 | zvec-grep 评分 / 工具次数 |
|---|---|---|---|
| 低 | `reflex:6` | 87–96 / 7–12 | 84–92 / 3–5 |
| 中 | `requests:16` | 68–97 / 2–6 | 75–99 / 7–10 |
| 高 | `conan:39` | 5–92 / 2–24 | 5–100 / 3–19 |

此分层是历史样本的描述，不是统计显著性标准，也不是模型的固有随机性等级。低波动组仍存在路径变化；高波动组的一次 baseline 使用子代理，主轨迹未展开其内部调用。新一轮应与历史同三题比较，失败尝试的开销单独计入。

CI 默认使用 OpenCode `1.18.4`、`custom-openai/glm-5.2` 和本地 Embedding，每个任务、每个 profile 独立运行 10 次。请在仓库的 Actions secret 中配置 `GLM_API_KEY`，用于 Agent 执行和评审。两个执行模型沿用同一个 secret 名称；其中的百炼业务空间 API Key 需要具有所选模型的调用权限。远端 Embedding 还需要 Actions secret `QWEN_EMBEDDING_API_KEY` 和仓库 variable `QWEN_EMBEDDING_ENDPOINT`；只有选择 `embedding=remote` 时才会将源码内容发送到该远端 Embedding endpoint。上文的 Claude Code 配置对应已发布的本地测试协议。

CI 的固定 trial 次数、失败重试上限及 local/remote Embedding 模型映射由版本化的 [`ci-config.json`](ci-config.json) 指定。执行模型、Embedding runtime、任务范围和 Rust 源码是手动运行的输入；密钥与 endpoint 属于运行环境配置。

SWE-QA suite 显式配置版本化的 [`index.ignore`](zg_bench/swe_qa/data/index.ignore)。其中只列出 3 个后缀为文本、实际内容为 PNG 或损坏 UTF-16/32 数据的固定 benchmark fixture。ignore 文件摘要会计入本地索引种子的身份；其他索引失败仍会使任务失败并出现在结果中。该设置属于 suite，其他 runner suite 不会继承。

完整运行包含 20 题 × 2 个 profile × 10 次 = 400 个独立 trial。CI 通过 `--max-retries 2` 为异常失败（包括 Agent 超时）的 trial 最多额外重试 2 次；API 使用额度耗尽不重试。成功的 trial 和低分答案不重跑，重试次数不计入每组 10 次的样本数。本地运行默认不重试，可显式传入 `--max-retries` 开启。

每次失败的日志和轨迹保存在 Harbor job 的 `.retry-history/` 下，并随原始证据上传；Job Summary 展示重试次数和最终错误数。报告中的 token、工具调用、耗时及费用仅统计每个 trial 最终成功的那次执行，不包含失败尝试的额外开销；这些开销可在归档证据中查看。用尽重试次数仍失败时，该任务保持失败。每个任务 job 的总时限为 6 小时，包含准备和重试时间。

即使部分任务最终失败，工作流仍会输出 Aggregate。Aggregate 只使用成功产出独立报告的任务；失败任务不进入任何均值、总量、筛选分母或样本数，并会作为缺失任务单独列出。工作流本身仍保持失败状态，以明确提示覆盖不完整。

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

每个任务在同一个 runner 上运行 Baseline 和 zvec-grep，对配对结果进行评审，并将 Harbor 运行证据和独立任务报告上传为 artifacts。全部任务完成后生成聚合报告；部分任务失败时，Summary 仍展示已完成任务的报告，不将不完整结果作为完整 benchmark 聚合。

使用 **Re-run failed jobs** 时，可以复用同一个 GitHub run 之前 attempt 已成功的任务报告。每题的 `task-report-manifest.json` 将报告摘要绑定到候选 commit/仓库、harness 版本、协议哈希、任务范围、模型、endpoint 摘要和 trial 配置。聚合器先校验身份和报告证据，再按题目选择最新有效结果，离线重新计算聚合指标，不调用模型，也不跨 attempt 拼接单个 trial。聚合产物中的 `task-report-selection.json` 记录选中的 attempt 和被排除输入。缺少兼容 manifest 的旧报告和已过期 artifact 无法复用，需要重新完整启动或重跑缺失题目。自动 trial 重试仍发生在同一 attempt 内。

## 代码结构

Python 包采用与 Retrieval-only benchmark 相同的职责划分：`core`、`engines`、`metrics` 和 `reports`。SWE-QA 的专用输入与 Harbor 集成保持独立：

```text
swe-qa-bench/
  zg_bench/
    cli.py                 稳定的 zg-bench 命令入口
    runner.py              Suite/profile 执行编排与安装缓存
    retries.py             失败 trial 的重试策略
    ci/                    由 workflow 调用的制品检查
    core/                  公共异常、JSON 读写、模型请求与报告协议
    engines/
      registry.py          Agent/模型支持目录与凭证路由
      judge.py             模型请求、评分解析与评审重试
      opencode/config.py   OpenCode 模型和 provider 配置
    metrics/               数值检查、用量核算与对比计算
    reports/               报告校验、聚合与 Markdown 输出
      retries.py           Trial 重试摘要渲染
    agents/                稳定的 Harbor 适配器导入路径与会话导出
    swe_qa/                Suite 校验、配对结果采集与评审编排
      cli.py               稳定的 python -m zg_bench.swe_qa 命令入口
      selection.py         锁定的 CI 范围选择与矩阵输出
      data/                锁定的任务选择与隔离参考答案
    settings.py            版本、endpoint 与请求常量
  suites/                  Runner 的 suite 配置
  datasets/                锁定的 Harbor 任务与环境
  tests/                   CLI、用量、报告与 provider 契约测试
```

Suite 适配层采集并校验执行证据，judge engine 对最终答案评分，metrics 基于规范化记录计算指标，reports 校验和聚合这些记录后再渲染。指标计算不调用模型，也不依赖具体 Agent engine；Markdown 渲染不发起模型请求。Runner 负责 Harbor 执行编排，不定义报告中的统计公式。

新增执行模型或 Agent 时，扩展 engine 注册及对应的 provider 或 Harbor 适配器测试。执行与评审共用的模型在 `core/protocol.py` 使用同一份请求规格，并通过 `engines/registry.py` 共用 provider、凭据和 endpoint 解析。新增 CI 任务范围时修改 `swe_qa/selection.py`。新增指标时，在 `metrics/` 中定义计算方式与用量检查，再更新报告校验和渲染。新增 suite 负责自身的输入校验和证据转换，不向公共指标代码添加数据集分支。两个 CLI、Harbor 适配器导入路径及锁定资源路径保持稳定。

这里统一的是与 Retrieval-only 的目录职责；两个 benchmark 分别保留自己的 Python 或 JavaScript 实现和统计协议。两套工作流共用 [`rust-candidate-package`](../../.github/actions/rust-candidate-package/action.yml) 构建 Rust 候选包，并用共享的 [`rust-package-cache.mjs`](../shared/rust-package-cache.mjs) 校验清单。模块迁移不改变任务选择、trial 次数、用量统计范围或 Aggregate 排除规则。

## 本地配置

以下 Claude Code 本地步骤记录已发布结果使用的历史协议；当前 GitHub Actions 对照测试仅构建并测试上文指定的 Rust 候选版本。

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
