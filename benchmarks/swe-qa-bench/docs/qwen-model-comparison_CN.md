# Qwen 3.7 / 3.8 SWE-QA E2E

本轮比较四组：baseline × qwen3.7-max / qwen3.8-max，以及 zg × 两个模型。

| 配置 | 本轮值 |
| --- | --- |
| Runner | `zvec-retrieval-benchmark`，同一台机器串行执行 |
| Agent | OpenCode `1.18.4` |
| 任务 | 锁定的 SWE-QA20 全部 20 题 |
| 次数 | 每题、每模型、每 profile 1 次，共 80 个计划 trial |
| 失败重试 | 沿用协议：每个失败 trial 最多额外重试 2 次；报告保留重试记录 |
| zg 候选 | `97954e34cb55400d24692d400463e6df7bffb449` |
| Embedding | `local/potion-code-16m-v2` |
| 执行参数 | thinking=true，reasoning_effort=high，temperature=0，seed=42，输出上限 32000 |
| 共同评审 | `qwen3.8-max`；独立参考答案仅向评审器开放 |

两次 workflow dispatch 均选择 `all-full`、`local`、`n_attempts=1`、
`runner=zvec-retrieval-benchmark`、`judge_model=qwen3.8-max`，
只将 `model` 分别设为 `qwen3.7-max`、`qwen3.8-max`。

先验证 Docker、容器挂载和实际模型访问，再构建候选并运行题目。
每题保留 Harbor 轨迹、trial 用量、独立评分和绑定执行模型的 manifest，
各模型分别生成聚合报告。比较 Judge、input/output token、toolcall 和 Agent
执行耗时；索引构建耗时单独统计，失败或缺失结果不能按零分填充。

单次 trial 用于本轮探索，不能据此判断统计显著性。共同评审模型也是参测模型，
评分可能存在模型偏好，应结合答案与客观用量指标判断。
