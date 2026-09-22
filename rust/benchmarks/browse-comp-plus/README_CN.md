<p align="right">
  <a href="./README.md">English</a> | 中文
</p>

# BrowseComp-Plus

[BrowseComp-Plus](https://github.com/texttron/BrowseComp-Plus) 用于评估深度研究 Agent：题目通常需要在多篇文档中定位并串联线索，才能得出答案。与实时网页搜索不同，它使用约 10 万篇经人工核验的固定语料，使不同检索器能够在可控、可复现的条件下进行比较。

本 benchmark 使用 Codex 在该语料库上进行原生配对评测。

整体原则与原论文一致，但在语料处理和评测流程上略有调整，以更贴近用户实际使用通用 Agent 的场景。

每个问题均通过相互独立的配对 trial 进行评测；每次评测都使用相同的模型、prompt、语料库、Codex 配置和限制：

- **Baseline：** Codex 使用其标准工具集。
- **zvec-grep：** 保持相同的 Codex 配置，仅通过 `zg --install` 增加 zvec-grep MCP 工具和使用指引。

Benchmark 记录回答质量、Token 用量、Agent 执行耗时、工具调用次数和完整的 Codex 轨迹。

## 评测结果

[最新完整报告](./LATEST_REPORT.md)提供了完整结果和复现信息。以下是本次 study 的摘要：在 300 组配对 trial 中，zvec-grep 在保持回答质量的同时，将平均 Input Token 减少 **37.56%**、工具调用次数减少 **43.52%**、Agent 执行时间减少 **38.58%**。

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

在此目录中安装 Python 环境并检查宿主环境：

```sh
cd benchmarks/browse-comp-plus
uv sync
source .venv/bin/activate
zg-bench doctor
```

宿主环境需要提供：

- 安装了 `uv` 的 macOS 或 Linux；
- 已安装并完成身份验证的 Codex CLI；
- 已安装 `zg`。

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

Codex 模型和 reasoning effort 在 `benchmark.toml` 中配置。Runner 会在创建 trial 前验证配置的模型。

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

使用盲测 Codex 评审器评测最近一次运行，并生成最终报告：

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

## 产物

生成的数据保存在 `artifacts/` 下，不会提交到代码仓库。其中包括锁定的源数据快照、生成的语料库、可复用索引、各次运行隔离的 profile、原始尝试、评审器输入和报告。Gold data 和 manifest 始终位于 Agent workspace 之外。
