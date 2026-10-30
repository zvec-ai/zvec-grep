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

模型与请求参数在 `benchmark.toml` 中配置：开启思考，reasoning effort 为 `high`，请求温度 `0`、seed `42`，输出上限 20,000 Token，上下文上限 1,000,000 Token。`max_output_tokens` 作为每次模型响应的 `max_tokens` 发送；在 Qwen 上限制答案输出，不是整个多轮 trial 的思考或累计 Token 预算。服务端对参数的实际处理依模型而定。关闭自动压缩与子代理委派，确保 trial 用量属于所记录的会话。

当前配置为 `trials_per_case = 1`、`infrastructure_retries = 0`：每题一组 baseline/zvec-grep 配对，harness 不自动重试基础设施失败。因此 `iteration` 共 2 组配对、4 次 Agent 运行，`smoke` 共 1 组配对、2 次 Agent 运行，`ci` 共 5 组配对、10 次 Agent 运行及 10 次判分。这组迭代参数与上文历史 study 的每题 3 次试验不同。

修改基础设施时先使用固定的 628、241 两题；两组配对、4 次判分与原始证据汇总完整成功后，再使用日常 5 题 CI 集合：

```sh
zg-bench run --suite iteration
```

运行原来的固定 5 题日常 CI 子集：

```sh
zg-bench run --suite ci
```

[选题依据](./suites/CI_SELECTION.md)与[审计元数据](./suites/ci-selection.json)记录了未改变的种子、
排序哈希、逐题检索链和标注证据文件。日常集合恢复为原来的 **628、700、966、827、241**，
共 10 份答案和 10 份评审，覆盖 32 份不同的标注证据文档。这是跨文档、跨主题检索，
不衡量代码库跨模块导航；文档数量也不等于必读文件数或推理跳数。
GitHub Actions 选择 `suite=ci`、`parallel_cases=5`；5 题共用一次准备、一份索引和一个 daemon。

按用户要求，因十题运行过慢恢复原五题，未根据得分或耗时替换单题；长尾题 827 仍保留。
[最新十题首轮](https://github.com/zvec-ai/zvec-grep/actions/runs/40648543751/attempts/1)
保留了 17 份完成答案和 14 份评审（7 个完整配对，双方 7/7），另有两次无输出超时及一次
provider 输入长度超限。3 次失败累计报告 166,532,123 tokens（含缓存输入）、18,972.83 秒，
两个未结束步骤的额外用量未知。[恢复轮](https://github.com/zvec-ai/zvec-grep/actions/runs/40648543751/attempts/2)
随本次缩减范围而取消；十题未完成验收，已有证据与成本单独保留，不并入下文成功五题实测。
缩减题数仍不能保证每题模型耗时。

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
3. 先选择默认的 `iteration`（固定 628、241 两题，共 4 次 Agent 答题、4 次判分）。
   两组配对、全部判分和原始证据汇总完整成功后，再使用较大的集合；小样本只验证 CI，不能代表整体质量。
   另保留 `ci`（5 题、10 次 Agent 运行、10 次判分）和 `smoke`
   （1 题、1 次配对试验，共 2 次 Agent 运行，另含工具使用审计）。
   `parallel_cases` 可选 1、2（默认）、5，按服务端限流调整。

无需合并即可运行分支版本：

```sh
gh workflow run zg-bench.yml --repo zvec-ai/zvec-grep \
  --ref dev/browse-comp-plus-ci \
  -f benchmark=browse-comp-plus \
  -f candidate_ref=main -f suite=iteration -f parallel_cases=2
```

BrowseComp-Plus 路由无需 `pr_number`；既有 retrieval 路由仍校验该参数。
两个路由均保留手动触发与 maintain/admin 权限检查。

`prepared_run_id` 留空时，自动查找同分支此前手动运行中完全匹配的准备产物。
也可填入指定运行的数字 ID，并使用相同候选 commit；显式指定的快照缺失、过期或不兼容时直接失败，不重新构建。

| Actions 设置 | 用途 |
| --- | --- |
| Secret `QWEN_API_KEY` | 可调用 `qwen3.8-max` 的 API key；未配置时沿用已有 `GLM_API_KEY` secret。 |
| Variable `BROWSE_COMP_MODEL_BASE_URL` | 必填，模型的 OpenAI 兼容 base URL，百炼地址以 `/compatible-mode/v1` 结尾；未配置时使用 `SWE_QA_MODEL_BASE_URL`。 |
| Secret `HF_TOKEN`（可选） | Hugging Face 下载认证。 |

工作流固定 OpenCode `1.18.4`，执行与评审均使用 `custom-openai/qwen3.8-max`，embedding 使用本地 `local/potion-multilingual-128m`，保留锁定数据集，每题执行 1 次配对试验。建索引前通过一次小型模型调用检查访问权限。
所选 key 通过 `OPENAI_API_KEY` 提供给每个调用模型的步骤，无需 Codex 登录或账号 `auth.json`。
[OpenCode 配置](https://opencode.ai/docs/config/)通过环境变量引用 key；临时 home 与 profile 不进入缓存或上传产物。

**耗时优化：** 准备、索引恢复和所有题目执行放在同一个 job。命中快照时只下载、校验、
解压一次；冷启动直接使用本机刚构建的索引。主进程统一启动并预热一个 zg daemon，
按 `parallel_cases` 并发执行题目子进程。各题独立使用 OpenCode/profile/judge home、
会话、临时目录、结果和证据，共享已有语料及索引；这是会话与进程隔离，不是安全沙箱。
同题 A/B 仍按计划顺序执行，一题失败不取消其他题。所有子进程退出后才关闭共享 daemon。
每题结束后立即在本机保存独立校验封存的证据，执行结束或取消时在 runner 仍可用的情况下
上传 bundle；失败重跑复用其中已完成的答案和匹配评审。最终汇总不额外调用模型，
共享启动和预热耗时只计算一次，各题原始元数据保留在 `shards/`。

| 阶段 | 复用或并行方式 |
| --- | --- |
| Rust 包 | 按候选 commit 精确缓存。 |
| 源数据 | 下载校验完成后立即缓存，索引构建失败后无需重新下载语料。 |
| 题目解密 | 从 Parquet 仅读取问题、参考答案及 gold/evidence 文档 ID，跳过未使用的正文；保留全部题目与评分输入。 |
| 语料、索引和本地模型 | 保留 7 天的 v3 runtime 快照仅包含一份 Markdown 语料（`workspaces/b`）、索引、Potion 模型、解密题目及状态清单，不传输原始 Parquet 和重复的 baseline 语料。候选版本、数据、embedding 配置、准备脚本、系统架构和目录必须一致。 |
| runtime 产物完全匹配 | 执行 job 只下载、校验、解压一次 runtime 快照，通过 `copy2` 恢复一次独立的 baseline 语料，不使用硬链接，保留纳秒时间戳；生成新的固定评测计划，无需重建索引。 |
| 题目执行 | 同一 runner 内最多 5 个独立题目子进程，共享已有索引和主进程管理的 daemon；无需逐题重复下载、解压或预热。 |
| 失败重跑 | “Re-run failed jobs” 先恢复并校验原始计划与题目字节，不重新随机化 A/B 顺序，不覆盖计划 artifact；在同一次 workflow、同一计划内复用已完成 trial 及匹配评审，只补失败或缺失项。此前 bundle 的证据继续参与汇总，并保留全部历史尝试。 |

**冷启动成本：** 所有 suite 都准备完整的 100,195 篇语料。准备标识变化且无兼容快照，或产物过期后，
需在本地构建 Potion 索引。[历史实测运行](https://github.com/zvec-ai/zvec-grep/actions/runs/40438698350)
索引全部文档耗时 112 分 32 秒、无失败，压缩的**完整准备快照**为 15.9 GiB。
[runtime 转换实测](https://github.com/zvec-ai/zvec-grep/actions/runs/40501615950)
直接复用该索引：新快照为 11.47 GiB，相比旧包的 15.86 GiB 缩减 27.68%。
一次性转换及准备共 12 分 40 秒；原 matrix 版本[两题 E2E 已通过](https://github.com/zvec-ai/zvec-grep/actions/runs/40519967945)。后续精确命中 v3 时跳过转换。
准备与执行合并后的 job 限时 6 小时；与 SWE-QA Bench 一致，concurrency 按 run ID 和 attempt
独立，不同手动运行不会互相阻塞或取消。runner 会释放 SDK 空间，并检查至少 50 GiB
可用磁盘。runtime 快照使用 artifact，与仓库的 10 GB Actions cache 配额分开；仅源数据和
候选包进入 Actions cache。命中 runtime 快照时只下载原产物一次，不重复上传。五题总下载量从五份 57.35 GiB
降至一份 11.47 GiB；减少 80% 传输量不代表整体耗时减少 80%。每轮仍需一次下载、
解压和 baseline 复制；普通托管 runner 不会跨 workflow 保留已恢复文件。
Embedding 使用 runner CPU；`smoke` 同样产生 Agent 和评审 API 费用。

**最新恢复五题实测（2026-10-28）：** [运行 40681729330](https://github.com/zvec-ai/zvec-grep/actions/runs/40681729330)
使用 harness `0acd5b85`、candidate `4e557cd9`，答题和评审均为 OpenCode 1.18.4 +
`custom-openai/qwen3.8-max`。10 份答案、10 份匹配评审完整；下载后的每题与汇总 checksum、
selected/attempt 字节、评审输入哈希、同题 A/B 顺序和完整性检查全部通过。
所有 trial 仅有 `attempt-001`，无失败或未完成尝试、无未知 Agent/评审用量；
628 的 baseline 错误答案原样保留，未因答错重跑。

| 本轮五题均值或结果 | Baseline | zvec-grep | zg 变化 |
| --- | ---: | ---: | ---: |
| 正确率 | 4/5（80%） | 5/5（100%） | +20 个百分点 |
| Agent 总 tokens，含缓存输入 | 2,600,356.8 | 2,470,849.4 | −4.98% |
| Agent 耗时，不含准备与评审 | 892.81 秒 | 801.31 秒 | −10.25% |

Workflow 共 **1 小时 17 分 13 秒**。共享执行 job 共 1 小时 12 分 12 秒，包含一次下载
11 分 26 秒、一次恢复 3 分 01 秒、恢复后索引校验 14 秒、答题及评审 54 分 21 秒。
无模型凭据的候选 job 共 4 分 36 秒，聚合 16 秒。共享 daemon 启动与预热 4.18 秒，
包含在执行阶段且只计一次；加上 5 题各自 profile 准备后 runtime 准备共 5.41 秒，
6 个 sessions 不代表 6 次索引预热。10 次评审另用 29,783 tokens、累计 88.41 秒；
模型预检、runner 准备与评审均不计入 Agent 均值。

只有 241、700 两题 zg 更快，其余三题更慢。4/5 题实际调用 zg，共 4 次调用，2 次成功、
2 次失败；即使 zg 侧答案全部正确，工具错误仍保留。这是固定五题、每题一次的小样本，
不能推断总体收益或 workflow 加速幅度；下载波动和模型长尾仍明显。tokens 含缓存输入，
不等同于未缓存计费量。此前未完成十题的完成、失败与未知成本在上文单列，不计入本轮均值。

**单 job CI 实测（2026-10-27）：** 先完成[两题 iteration](https://github.com/zvec-ai/zvec-grep/actions/runs/40539457579)，
再通过[当时的五题 CI](https://github.com/zvec-ai/zvec-grep/actions/runs/40552240366)，
两轮均使用 harness `0bb4e773`、candidate `4e557cd9`。五题共完成 10 份答案和 10 份评审；
下载后的每题与汇总 checksum、selected/attempt 结果字节、评审输入哈希和完整性检查均通过。
每个 trial 仅有 `attempt-001`，无失败或未完成的 Agent 尝试，无未知 Agent/评审用量，
直接复用完整的 100,195 篇索引。

| 五题结果，每题一次 | Baseline | zvec-grep |
| --- | ---: | ---: |
| Score | 5/5（100%） | 5/5（100%） |
| 平均 Agent 总 tokens，含缓存输入 | 1,323,608 | 1,266,916 |
| 平均 Agent 耗时，不含准备与评审 | 733.62 秒 | 495.54 秒 |

Workflow 总耗时 **49 分 25 秒**：候选校验与构建 49 秒，快照下载一次 3 分 11 秒，
恢复一次 2 分 51 秒，恢复后索引校验 13 秒，题目执行及评审 39 分 15 秒，汇总 16 秒。
共享 daemon 启动与预热共 4.77 秒，包含在执行阶段且仅计算一次。10 次评审共用
29,761 total tokens、累计 91.16 秒，与 Agent 指标分开；预检和 runner 准备也不计入
Agent 均值。两题运行下载耗时 24 分 56 秒，本轮为 3 分 11 秒，传输波动明显，
不能把两轮总耗时差直接归因于共享索引。

这五题用于验证 CI，不代表整体性能，也不与上文历史 Codex 研究直接比较。平均 Agent
tokens 减少 4.28%、耗时减少 32.45%，主要由 case 700 拉动，其余四题 zg 耗时均增加。
只有三题实际调用 zg：一次成功，两次因模型把已提供的目录 `browse-comp-plus`
误写为 `browsecomp-plus` 而失败。这些完成答案与工具错误均保留在证据中，未为改善结果重跑。

小型计划 artifact 保留 7 天；失败 job 应在准备快照仍有效时重跑，过期后启动新运行。
旧 v2 快照只通过 `42102cd9` 冻结的准备代码哈希计算精确 legacy key，并要求候选版本、数据、
索引配置及目录一致。准备阶段校验兼容的完整快照后，一次转换为 v3，无需重建索引，
随后执行 job 复用 runtime 产物；不匹配的旧快照不能作为回退。

候选程序在不含模型凭据的独立 job 中构建。请只选择可信候选代码，该程序随后会在评测
runner 上执行。产物分别记录评测脚本与候选程序的 commit SHA。

每个 profile 每题运行 1 次。harness 不自动重试基础设施失败（`infrastructure_retries = 0`），
包括临时 HTTP 429/5xx 及服务端标记可重试的错误。手动重跑失败 job 时，只有 workflow run ID、
候选 commit、harness commit、配置及计划完全一致，才恢复原始证据。已完成的答案即使答错也复用，
已有评审仅在输入哈希匹配时复用，只执行失败或缺失的 trial 和评审，不新增自动重试、不混用不同计划。
证据必须包含 `evidence-manifest.json`；缺少该完整性清单的旧产物不能续跑。
历史失败 attempt、成本及来源仍保留在原始证据中。job summary 分别列出
成功试验、有效配对试验、额外重试、失败 attempt、最终失败和未完成试验。配对指标仅使用
最终完成的 attempt；失败 attempt 的开销保留在原始证据中。两个 profile 使用相同的模型参数、
锁定样本及配对评分方式。相同任务模板提供各自的完整绝对工作路径，要求原样复制路径参数，
以减少目录拼写错误；工具失败仍保留在报告与原始证据中。

结果展示在 Actions job summary，产物保留策略与 SWE-QA Bench 对齐：

| 产物 | 内容 | 保留时间 |
| --- | --- | --- |
| 各题 / 汇总证据 | JSON/Markdown/CSV 报告、运行轨迹、评审结果、候选元数据及 `evidence-manifest.json` | 30 天 |
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
