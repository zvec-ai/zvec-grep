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

Job Summary 单元格使用 `baseline / zvec-grep / change`。每个任务的 baseline 和 zvec-grep 数值是对应 profile 三次 trial 的平均值。

| 指标 | 变化 | 含义 |
| --- | --- | --- |
| Judge | `zvec-grep - baseline` | 正值表示评审质量更高。 |
| `input_token`、`toolcall`、Agent 执行耗时 | `(zvec-grep - baseline) / baseline` | 负值表示资源用量更低。 |

如果 baseline 分母为零，对应的效率比较结果为 `N/A`。索引设置时间与 Agent 执行耗时分开统计。

在 Aggregate 行中：

- Judge 数值是所有任务的等权平均值。
- Baseline 和 zvec-grep 的效率数值是各任务 profile 平均值之和。
- 展示的效率变化是任务级变化的等权平均值，而不是聚合总和的比值。
- 结果为 `N/A` 的任务只会从受影响的 Aggregate 指标中排除。

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
