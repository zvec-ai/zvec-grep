<p align="right">
  <a href="./README.md">English</a> | 中文
</p>

# SWE-QA benchmark

此 benchmark 用于衡量 `zvec-grep` 对 Agent 回答代码仓库级软件工程问题的能力有何影响。标准对照中，两个 profile 使用相同的 OpenCode Agent、模型、任务 prompt、代码仓库 commit、环境和限制：

- **Baseline：** OpenCode 使用其标准工具。
- **zvec-grep：** 同一个 Agent 获得准备好的代码仓库索引，并通过 MCP 使用 zvec-grep。

索引构建单独测量，不计入 Agent 执行耗时。

## Benchmark 定义

[`SWE-QA Bench`](../../.github/workflows/swe-qa-bench.yml) workflow 运行 [`peng-weihan/SWE-QA-Bench`](https://github.com/peng-weihan/SWE-QA-Bench) 中固定的 20 个任务子集。Benchmark 输入锁定在此目录中：

- [`selection.json`](zg_bench/swe_qa/data/selection.json) 记录任务 ID、任务 slug、代码仓库 commit、资源哈希和 CI scope 成员关系。
- [`references.json`](zg_bench/swe_qa/data/references.json) 包含独立评审器使用的隔离参考答案，Agent 无法访问该文件。
- [`datasets/`](datasets/) 包含锁定的 Harbor 任务环境、prompt 和 verifier。
- [`swe-qa-bench.yaml`](suites/swe-qa-bench.yaml) 将本地数据集提供给 benchmark runner。

Workflow 会在启动需要模型的 job 前，验证锁定的任务选择、代码仓库 commit、哈希以及参考答案的隔离状态。

## CI scope

- 同一代码仓库的 pull request 和向 `main` 的 push 会运行以下 5 个 smoke 任务：`reflex:6`、`pylint:9`、`matplotlib:37`、`streamlink:14` 和 `xarray:32`。
- 需要模型的 CI 在两个 profile 中均使用 `opencode` Agent 和 `custom-openai/glm-5.2` 模型。
- 来自 fork 和 Dependabot 的 pull request 只运行锁定资源验证、单元测试和 dry-run 预检，不使用模型凭证。
- 手动运行 `workflow_dispatch` 并设置 `scope=smoke` 时，会运行相同的 5 个 smoke 任务。
- 手动运行 `workflow_dispatch` 并设置 `scope=all-full` 时，会运行全部 20 个锁定任务。

维护者可以在代码仓库根目录使用 GitHub CLI 触发手动 scope：

```sh
gh workflow run swe-qa-bench.yml -f scope=smoke
gh workflow run swe-qa-bench.yml -f scope=all-full
```

`all-full` 是 workflow scope，不是 `zg-bench --tier full` 的值。本地套件将完整的 20 个任务保存在 `ci` tier 中，workflow 会显式传入各 scope 选中的任务。

每个选中的任务会在同一 runner 上运行 3 次 baseline trial 和 3 次 zvec-grep trial。全部 6 个回答均单独评审。Workflow 仅生成报告：数值结果不会阻止 review 或 merge，但所有预期的 profile run 和评审调用都必须成功完成。

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

CI 在 Ubuntu 24.04、Python 3.12 和 Node.js 24 上运行。本地 harness 也支持在 macOS 上使用 Docker，但可比较的 benchmark 结果应使用一致的 Linux x86-64 环境。

安装以下前置依赖：

- [uv](https://docs.astral.sh/uv/)
- Docker Engine 或 Docker Desktop，并支持 Docker Compose v2
- Node.js 22 或更新版本以及 npm

验证 Docker Compose、安装锁定的依赖，并导出 workflow 使用的模型凭证：

```sh
docker compose version
npm ci

cd benchmarks/swe-qa-bench
uv sync --frozen
source .venv/bin/activate
export GLM_API_KEY="your-api-key"
```

运行 SWE-QA 配置所使用的同一套 profile-aware 预检：

```sh
zg-bench doctor \
  --agent opencode \
  --model custom-openai/glm-5.2 \
  --profile all \
  --embedding-model local/potion-code-16m-v2 \
  --zvec-grep-package ../..
```

本地 package 路径依赖上文在代码仓库根目录运行的 `npm ci`。

## 本地 smoke test 和 dry run

使用以下命令查看锁定的任务选择：

```sh
zg-bench list tasks swe-qa-bench --tier smoke
zg-bench list tasks swe-qa-bench --tier ci
```

首先 dry run 5 个任务、每个 profile 三次 trial 的配置：

```sh
zg-bench run swe-qa-bench \
  --tier smoke \
  --agent opencode \
  --model custom-openai/glm-5.2 \
  --profile all \
  --n-attempts 3 \
  --embedding-model local/potion-code-16m-v2 \
  --zvec-grep-package ../.. \
  --dry-run
```

移除 `--dry-run` 即可执行配对 Harbor run。本地 runner 会将轨迹和评审器输出写入 `runs/`；GitHub Actions workflow 是收集配对结果、独立评审每个回答并生成聚合报告的标准路径。

## 诊断失败的运行

当 trial 记录异常时，`zg-bench` 会输出结构化的 Agent 或 zvec-grep 配置错误，并以非零状态退出。使用以下命令检查保存的 run：

```sh
zg-bench diagnose --latest
zg-bench diagnose <job-name>
```
