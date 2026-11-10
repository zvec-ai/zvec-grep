<p align="right">
  <a href="./README.md">English</a> | 中文
</p>

# 基准评测

Retrieval-only 衡量不同产品版本的检索质量；e2e 衡量 Agent 接入 zvec-grep 后的回答质量和资源消耗。各套评测的配置、固定输入和评分细节由对应 README 说明。

## 贡献者：自行运行评测

| Benchmark | 范围 | 本地运行说明 |
| --- | --- | --- |
| Retrieval-only | SWE-QA20、BEIR、DuRetrieval、Quarry；hybrid、fts、vector 三种模式 | [SWE-QA20 runner](zg-retrieval/README.md#run-locally)、[扩展套件 runner](zg-retrieval/expansion/README.md) |
| SWE-QA e2e | 20 道软件工程问答；baseline 与 zvec-grep 两组 Agent | [配置与小规模运行](swe-qa-bench/README_CN.md#本地配置) |
| BrowseComp-Plus e2e | 固定文档库上的 100 道问答 | [配置与运行](browse-comp-plus/README_CN.md) |

开发阶段 CI 使用自己的 fork。在 **Actions → Retrieval-only → Run workflow** 选择 harness 版本，将 `candidate_ref` 设为要编译的 Rust 源码分支、tag 或 commit；保持 `embedding=local` 即可不配置模型 API Key。单版本报告和 artifacts 见 [Retrieval 指南](zg-retrieval/README.md#run-ci-and-read-the-result)。独立的 **Retrieval-only PR** 手动入口支持用同一份 harness 对比两个完整 commit SHA。

可选的 SWE-QA e2e 参见 [fork 运行指南](swe-qa-bench/docs/run-in-fork_CN.md)：配置模型凭证后，先运行 `repro-3` 或 `smoke`，再按需选择 `all-full`。Agent 执行和评审都会消耗模型额度；远端 Embedding 还需要单独的 key 和 endpoint，参见[语料规模与 Embedding 负载](zg-retrieval/README.md#corpus-size-and-remote-embedding-load)。

## 维护者：默认评测与额外触发

### 触发方式与结果

| 触发方式 | 执行内容 | 结果位置 |
| --- | --- | --- |
| PR 创建、重新打开或推送新提交，包含 fork PR | 固定 PR base/head 的 Retrieval-only；本地 Embedding | 自动更新的 PR 机器人评论、Actions summary 与 `retrieval-comparison` artifact |
| PR Conversation 新评论，单独一行 `@zg-bench` | 相同的 Retrieval 版本对比 | 同一条机器人评论，从排队状态更新为结果 |
| 手动启动 **ZG Bench**，指定 `pr_number` | Retrieval 对比；只有明确设置 `run_e2e: true` 才额外运行 e2e | 同一条机器人评论及运行 artifacts |
| 手动启动 **SWE-QA Bench** | 指定任务范围、模型与 Embedding runtime 的 e2e | 单题证据与聚合报告 |
| 修改相关 benchmark 源码或工作流 | 离线回归检查 | CI 日志，不调用模型 |

PR 自动评测无需维护者角色和模型密钥；同一个 PR 的新提交会取消上一次自动评测。版本对比展示 File Hit@1/5/10、MRR@10、nDCG@10 的 **base / head / delta**，并链接到两个固定 commit。`retrieval-base-*`、`retrieval-head-*` 保留各版本证据。

[Retrieval PR comment](../.github/workflows/retrieval-pr-comment.yml) 通过独立的 `workflow_run` 任务发布已完成的自动评测：每个 PR 创建一条英文结果评论，后续运行和重跑更新同一条，支持 fork PR。评论包含两个 commit、原运行链接，以及 12 行套件/模式的 base / head / delta；缺失或无效结果明确显示 incomplete。已取消的运行、base/head 已过期的结果会跳过。手动 comparison 不发布这条自动评论；`@zg-bench` 继续使用各次请求自己的评论。

评论触发和调用模型的手动评测要求仓库角色明确为 `admin` 或 `maintain`；执行任务会重新检查原触发者和重跑者当前的权限。评论命令不会开启 e2e。引用或代码块中的命令、编辑旧评论、普通 Issue 和代码行 review 评论不会启动任务。运行中 PR 发生变化时，两个固定 SHA 保持不变；需要新建命令评论才能测试当前版本。

### 部署与失败重跑

将评论入口与可复用工作流合入默认分支，并允许评论任务使用 `issues: write` 和 `pull-requests: write`。只有评论任务持有写权限 token。评测执行使用只读 token、受信任的 harness checkout 和单独构建的候选版本。本地 Retrieval 不接收模型凭证；可选 e2e 需要 `GLM_API_KEY`，授权使用这些凭证运行前应检查候选代码。

`@zg-bench` 是命令关键字，无需注册同名账号；回复者使用 `github-actions[bot]`。专属机器人身份需要配置 GitHub App，并调整评论任务使用的 token 和 bot login。

自动结果发布工作流同样需要合入默认分支，并允许 Issues/Pull requests 写权限。该任务只使用默认分支受信任的代码，先通过 GitHub API 校验来源工作流、run、PR 和 commit 身份，再读取 JSON 产物、验证指标表并自行渲染 Markdown。发布任务不会执行 PR 代码或 artifact 脚本，评测任务继续使用只读 token。

失败后使用 **Re-run failed jobs**，会更新原机器人评论。e2e 聚合会检查同一 run 各 attempt 的候选 commit、harness、评测配置和报告摘要，按题目选择最新有效结果并保留之前的成功题目；缺失题目仍显示 incomplete。聚合产物中的 `task-report-selection.json` 记录选中的 attempt 和被排除输入。缺少身份 manifest 的旧报告无法复用，因此引入该 manifest 之前启动的运行需要重新完整启动；artifact 过期时也需要重新运行缺失题目。

## Benchmark 开发者：原理与验证

### 协议与实现

Retrieval 使用同一份固定 harness、输入和本地模型配置比较两个产品版本，各版本分别新建索引。e2e 比较的是两组 Agent：使用标准工具，以及在相同配置上额外获得准备好的 zvec-grep 索引、工具和使用说明。两组应固定任务、Agent/模型参数、共享工具、资源限制和 trial 次数；索引准备耗时单独统计。

参考答案、报告和历史输出必须放在可搜索工作区之外。使用通用工具说明、盲评和重复 trial，不针对参考答案调整 prompt 或强制使用工具。缺失和无效证据不能当作零分或成功结果。

代码入口见 [Retrieval 结构](zg-retrieval/README.md#code-structure)与 [SWE-QA 结构](swe-qa-bench/README_CN.md#代码结构)：runner 采集证据，validator 检查身份、协议和覆盖范围，指标函数对有效记录评分，聚合器与评论渲染器输出结果。[评论工作流](../.github/workflows/zg-bench.yml) 调用可复用工作流执行评测，由 [bench-comment.mjs](../.github/scripts/bench-comment.mjs) 更新状态。

### 验证改动

从仓库根目录执行离线检查，再按需进行调用模型的实验：

```sh
node --test benchmarks/zg-retrieval/test/*.test.mjs .github/scripts/*comment.test.mjs
uv sync --project benchmarks/swe-qa-bench --frozen
uv run --project benchmarks/swe-qa-bench python -m unittest discover -s benchmarks/swe-qa-bench/tests -v
```

改动工作流后运行 `actionlint`；评分与报告改动可先用保存的证据[离线重放](zg-retrieval/README.md#offline-replay-and-comparisons)，工作流集成在 fork 上验证。失败处理、身份或重跑逻辑的变更应补充对应回归用例。具体某次运行的验证记录写在 PR 中，README 保留当前行为和复现步骤。
