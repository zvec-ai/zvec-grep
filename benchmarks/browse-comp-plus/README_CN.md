<p align="right">
  <a href="./README.md">English</a> | 中文
</p>

# BrowseComp-Plus

[BrowseComp-Plus](https://github.com/texttron/BrowseComp-Plus) 用于评估深度研究 Agent：题目通常需要在多篇文档中定位并串联线索，才能得出答案。与实时网页搜索不同，它使用约 10 万篇经人工核验的固定语料，使不同检索器能够在可控、可复现的条件下进行比较。

当前 harness 使用 **OpenCode 1.18.4 + `custom-openai/qwen3.8-max`** 在该语料库上进行配对评测，执行和盲测评审使用同一模型。检索使用本地 **`local/potion-multilingual-128m`** embedding 模型，无需远端 embedding 服务或 API key。

整体原则与原论文一致，但在语料处理和评测流程上略有调整，以更贴近用户实际使用通用 Agent 的场景。

每个问题均通过相互独立的配对 trial 进行评测；每次评测都使用相同的模型、prompt、语料库、OpenCode 配置和限制：

- **Baseline：** OpenCode 使用本地工具，关闭网页搜索、网页抓取和子代理。
- **zvec-grep：** 保持相同的 OpenCode 配置，仅通过 `zg --install --target opencode` 增加 zvec-grep MCP 工具和使用指引。

Benchmark 记录回答质量、Token 用量、Agent 执行耗时、工具调用次数和完整的 OpenCode 轨迹。

## 已发布结果（历史 Codex 协议）

[已发布完整报告](./LATEST_REPORT.md)及下列数字来自 Codex `0.147.0` + `gpt-5.6-sol`，embedding 使用 `qwen/qwen3.7-text-embedding`，**并非当前 OpenCode/Qwen/Potion 配置的结果**。Agent、embedding 模型与工具协议已变化，新运行应作为独立研究，不能直接将这些数字作为回归基线。

以下是历史 study 的摘要：在 300 组配对 trial 中，zvec-grep 在保持回答质量的同时，将平均 Input Token 减少 **37.56%**、工具调用次数减少 **43.52%**、Agent 执行时间减少 **38.58%**。

### 评测配置

Study 选取 100 个 case，以兼顾覆盖范围、运行时间和成本。样本并非随机抽取，而是按照锁定的 Hugging Face `test` split 原始顺序选择。我们没有在这部分数据中发现明显的顺序偏置。采用公开且固定的顺序，也能尽量减少人为选择空间，避免挑选更有利于 zvec-grep 的 case。

对于原始数据集中经复核确认存在缺陷的 case，例如题目线索相互矛盾，或现有语料无法支持明确答案，我们会将其排除。所有排除项均记录在 [`suites/study.txt`](./suites/study.txt) 中。

| 配置项 | 值 |
| --- | --- |
| 评测规模 | 100 个 case · 300 组配对 trial |
| Agent | `gpt-5.6-sol` · `high` reasoning |
| Embedding 模型 | `qwen/qwen3.7-text-embedding` |

### 主要结果

所有 300 组配对 trial 均纳入均值。变化表示 zvec-grep 相对 Baseline 的差异。

| 指标 | Baseline | zvec-grep | 变化 |
| --- | ---: | ---: | ---: |
| 回答准确率 | 98.67% | 99.00% | +0.33 pp |
| Input Token | 1.68M | 1.05M | **−37.56%** |
| 工具调用次数 | 25.42 | 14.36 | **−43.52%** |
| Agent 执行时间 | 259.4 秒 | 159.3 秒 | **−38.58%** |

zvec-grep 索引准备过程与 Agent 执行过程分开测量和报告。

### 结果分析

- **为什么能提供帮助：** 面对需要根据改写线索进行跨文档推理的问题，zvec-grep 可以在 Agent 大范围扫描语料库之前定位到正确实体，从而缩小有效搜索空间，降低发现候选答案的成本。
- **效果有多普遍：** zvec-grep 将 Input Token 平均减少了 37.6%；case 级中位降幅为 25.8%，100 个 case 中有 67 个的 Token 用量更低，同时回答质量保持稳定。其余 case 的运行规模通常较小，绝对差值也较小。平均降幅高于中位降幅，是因为少数 Baseline 高消耗长尾轨迹在使用 zvec-grep 后大幅缩短；而 25.8% 的中位降幅和 67% 的 case 占优比例也说明，收益并不只来自这些极端值。
- **收益较小的场景：** 如果精确搜索本就足够，或语义检索结果虽相关却不足以确定答案，Agent 仍可能使用 grep 重复验证。这些退化在不同 trial 间也不够稳定，说明这部分劣势更多来自额外检索开销和运行波动，而不是普遍且稳定的退化。因此，准确验证证据并在证据充分时及时停止仍然十分重要。

总体来看，当任务适合通过语义检索缩小大范围或模糊的搜索空间时，zvec-grep 最能发挥价值，并可避免部分高消耗的极端探索轨迹。如果任务本身适合用精确关键词直接定位，zvec-grep 的增量收益较小，整体表现通常更接近 Baseline。

BrowseComp-Plus 数据集中也存在少量瑕疵，例如个别 query 的线索与来源文档无法完全对应。这与真实世界中的信息环境相似：资料可能不完整，彼此之间也可能存在矛盾。如果现有语料无法支持一个可信的答案，我们会排除该 case；如果核心答案仍有充分的证据支撑，则仍会保留该 case。这类线索可能让 Agent 花费更多时间核对，并加大不同 trial 之间的波动。

## 前置条件

宿主环境需要 macOS 或 Linux，并安装 `uv`、Node.js 和 `zg`。安装固定版本的 OpenCode，配置模型 API key 及其业务空间对应的 OpenAI 兼容 base URL。Potion embedding 在本地运行，首次使用时下载模型文件。

```sh
cd benchmarks/browse-comp-plus
npm install --global opencode-ai@1.18.4
uv sync --frozen
source .venv/bin/activate

# 从环境变量或密钥管理器读取凭据。
export OPENAI_API_KEY="$QWEN_API_KEY"
export BROWSE_COMP_MODEL_BASE_URL='https://YOUR-WORKSPACE.cn-beijing.maas.aliyuncs.com/compatible-mode/v1'
zg-bench doctor
```

无需交互式登录。每个 profile 使用隔离的 OpenCode 配置与数据目录，模型凭据从 `OPENAI_API_KEY` 读取。

## 准备 benchmark

下载锁定版本的官方数据，将语料库中的每个 `text` 字段原样生成为 `<docid>.md`，并构建可复用索引：

```sh
zg-bench prepare
```

首次准备需要网络连接，并需要足够的磁盘空间来存放下载的数据、生成的语料库和索引。

后续运行会复用已经完成的下载、语料库和索引阶段。

## 运行

使用一个问题验证完整的配对流程：

```sh
zg-bench run --suite smoke
```

模型与请求参数在 `benchmark.toml` 中配置：开启思考，reasoning effort 为 `high`，请求温度 `0`、seed `42`，输出上限 32,000 Token，上下文上限 1,000,000 Token。服务端对参数的实际处理依模型而定。关闭自动压缩与子代理委派，确保 trial 用量属于所记录的会话。

运行固定随机选取的 5 个问题组成的 CI 子集：

```sh
zg-bench run --suite ci
```

运行固定的 Study 子集：

```sh
zg-bench run --suite study
```

运行锁定的官方数据集中的全部样例：

```sh
zg-bench run --suite full
```

## 评测与报告

使用盲测 OpenCode/Qwen 评审器评测最近一次运行，并生成最终报告：

```sh
zg-bench evaluate
```

仅对于 `smoke` 套件，评测还会审计 zvec-grep profile 的工具轨迹，并报告 zvec-grep 是否使用正确。该审计独立于对回答正确性的盲测评审。

如有需要，可以明确指定 run：

```sh
zg-bench evaluate <run-id>
```

重新生成最近一次运行的 Token、耗时、完成状态和配对样例报告：

```sh
zg-bench report
```

如有需要，可以明确指定 run：

```sh
zg-bench report <run-id>
```

删除所有 run 和生成的报告，同时保留下载的数据、workspace 和可复用索引：

```sh
zg-bench clean
```

## GitHub Actions（手动触发）

[BrowseComp-Plus workflow](../../.github/workflows/browse-comp-plus.yml)
仅供仓库 maintain/admin 角色手动触发，不会随 push、PR、评论或定时任务运行。

1. 配置下表中的 Actions 设置。
2. 合并前可从当前分支手动触发已注册的 **ZG Bench**，由其路由到 BrowseComp-Plus。
   BrowseComp-Plus 在默认分支注册后，可直接打开 **Actions → BrowseComp-Plus → Run workflow**。
   选择可信的 workflow 分支，将 `candidate_ref` 设为本仓库待评测的可信分支、tag 或 commit。
3. 选择 `ci`（默认：5 题、15 次配对试验，共 30 次 Agent 运行）或 `smoke`
   （1 题、3 次配对试验，共 6 次 Agent 运行，另含工具使用审计）。
   `parallel_cases` 可选 1、2、5（默认），按服务端限流调整。

无需合并即可运行分支版本：

```sh
gh workflow run zg-bench.yml --repo zvec-ai/zvec-grep \
  --ref dev/browse-comp-plus-ci \
  -f benchmark=browse-comp-plus \
  -f candidate_ref=main -f suite=ci -f parallel_cases=5
```

BrowseComp-Plus 路由无需 `pr_number`；既有 retrieval 路由仍校验该参数。
两个路由均保留手动触发与 maintain/admin 权限检查。

| Actions 设置 | 用途 |
| --- | --- |
| Secret `QWEN_API_KEY` | 可调用 `qwen3.8-max` 的 API key；未配置时沿用已有 `GLM_API_KEY` secret。 |
| Variable `BROWSE_COMP_MODEL_BASE_URL` | 必填，模型的 OpenAI 兼容 base URL，百炼地址以 `/compatible-mode/v1` 结尾；未配置时使用 `SWE_QA_MODEL_BASE_URL`。 |
| Secret `HF_TOKEN`（可选） | Hugging Face 下载认证。 |

工作流固定 OpenCode `1.18.4`，执行与评审均使用 `custom-openai/qwen3.8-max`，embedding 使用本地 `local/potion-multilingual-128m`，保留锁定数据集及每题 3 次试验。建索引前通过一次小型模型调用检查访问权限。
所选 key 通过 `OPENAI_API_KEY` 提供给每个调用模型的步骤，无需 Codex 登录或账号 `auth.json`。
[OpenCode 配置](https://opencode.ai/docs/config/)通过环境变量引用 key；临时 home 与 profile 不进入缓存或上传产物。

**耗时优化：** 准备阶段只执行一次，随后各题在独立 runner 上并行。每题仍在同一 runner
内顺序执行 3 次 A/B 配对试验；全套题目的随机 A/B 顺序统一生成。各题评审与其他题执行
重叠，最终报告从原始结果重算指标，不重复调用模型。题目间调度与本地串行执行不同，
各题原始元数据保留在 `shards/`。

| 阶段 | 复用或并行方式 |
| --- | --- |
| Rust 包 | 按候选 commit 精确缓存。 |
| 源数据 | 下载校验完成后立即缓存，索引构建失败后无需重新下载语料。 |
| 语料、索引和本地模型 | 完整快照包含 Potion 模型缓存，避免各题 worker 重复下载。仅在候选版本、数据、embedding 配置、准备脚本、系统架构和目录一致时复用，不回退复用其他候选版本的索引。 |
| 题目执行 | 最多 5 个独立 runner，不共享 daemon 或可写索引。 |
| 失败重跑 | “Re-run failed jobs” 保留成功题目的产物，汇总仅接受当前计划下各题最新 attempt。 |

**冷启动成本：** 两个 suite 都准备完整的 100,195 篇语料。新候选版本或缓存被淘汰后需在本地
构建 Potion 索引；尚未实测其冷构建耗时、索引大小或 Actions 端到端提速。
准备阶段和各题均限时 6 小时；与 SWE-QA Bench 一致，concurrency 按 run ID 和 attempt
独立，不同手动运行不会互相阻塞或取消。runner 会释放 SDK 空间，并检查至少 35 GiB
可用磁盘。压缩快照保留纳秒时间戳以避免误触发重建，尽力写入缓存，
同时作为有效期 1 天的 artifact 供各题下载。需计入数 GB 的传输/存储与仓库缓存配额；
Embedding 使用 runner CPU；`smoke` 同样产生 Agent 和评审 API 费用。

小型计划 artifact 保留 7 天；失败 job 应在准备快照的 1 天有效期内重跑，过期后启动新运行。

候选程序在不含模型凭据的独立 job 中构建。请只选择可信候选代码，该程序随后会在评测
runner 上执行。产物分别记录评测脚本与候选程序的 commit SHA。

每个 profile 保持每题 3 次试验；基础设施失败最多额外重试 2 次，包括临时 HTTP 429/5xx
及服务端标记可重试的错误，额度耗尽不重试。成功试验和低分不重试。job summary 分别列出
成功试验、有效配对试验、额外重试、失败 attempt、最终失败和未完成试验。配对指标仅使用
最终完成的 attempt；失败 attempt 的开销保留在原始证据中。锁定样本与配对评测协议不变。

结果展示在 Actions job summary，产物保留策略与 SWE-QA Bench 对齐：

| 产物 | 内容 | 保留时间 |
| --- | --- | --- |
| 各题 / 汇总证据 | JSON/Markdown/CSV 报告、运行轨迹、评审结果及候选元数据 | 30 天 |
| `task-report` / `aggregate-report` | 从通过 secret 扫描的证据中提取的精简报告和元数据 | 90 天 |
| 准备阶段诊断 | 准备日志及候选元数据 | 7 天 |

失败、取消或超时后，只要 runner 仍可执行，就会尝试导出并上传证据。部分试验失败时仍
评审已完成的配对，原失败状态保留；取消运行后不再启动评审。缺少凭据、运行失败、试验或评审
不完整都会使 workflow 失败；准确率与效率仅报告，不设置回归阈值。导出排除 profile、运行时
凭据与语料文件，检测到配置的 secret 值时阻止上传。

本地可运行不调用模型、不下载数据的 CI 集成检查：

```sh
uv run --frozen python -m unittest discover -s tests -v
```

设置 `OPENCODE_BENCHMARK_TEST_BINARY=opencode` 后，还会用离线协议测试验证已安装的 CLI。

## 产物

生成的数据保存在 `artifacts/` 下，不会提交到代码仓库。其中包括锁定的源数据快照、生成的语料库、可复用索引、各次运行隔离的 profile、原始尝试、评审器输入和报告。Gold data 和 manifest 始终位于 Agent workspace 之外。
