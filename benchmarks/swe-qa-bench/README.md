<p align="right">
  English | <a href="./README_CN.md">中文</a>
</p>

# SWE-QA benchmark

This benchmark measures how `zvec-grep` affects an agent's ability to answer
repository-level software-engineering questions. The published comparison uses
the same Claude Code agent, Claude Opus 5 model, task prompt, repository commit,
environment, and limits for both profiles:

- **Baseline:** Claude Code uses its standard tools.
- **zvec-grep:** the same agent receives a prepared repository index and uses
  zvec-grep through MCP.

Index construction is measured separately and is not included in agent wall
time.

## Benchmark definition

This benchmark uses a pinned 20-task subset of
[`peng-weihan/SWE-QA-Bench`](https://github.com/peng-weihan/SWE-QA-Bench).
The benchmark inputs are locked in this directory:

- [`selection.json`](zg_bench/swe_qa/data/selection.json) records task IDs,
  task slugs, repository commits, asset hashes, and runner tier membership.
- [`references.json`](zg_bench/swe_qa/data/references.json) contains the
  isolated references used by the independent judge; agents cannot access it.
- [`datasets/`](datasets/) contains the
  pinned Harbor task environments, prompts, and verifiers.
- [`swe-qa-bench.yaml`](suites/swe-qa-bench.yaml) exposes the
  local dataset to the benchmark runner.

The validation command below checks the locked selection, repository commits,
hashes, and reference isolation before model-backed runs.

## Published protocol

- **Coverage:** 20 retrieval-intensive tasks spanning What, Where, How, and
  Why, 8 intentions, and 11 repositories.
- **Agent:** Claude Code `2.1.212`.
- **Model:** Claude Opus 5 (`claude-opus-5`) at high reasoning effort.
- **Treatment embedding:** Qwen3.7 Text Embedding
  (`qwen/qwen3.7-text-embedding`).
- **Embedding endpoint:** the OpenAI-compatible Qwen endpoint shown in the
  local setup below.
- **Trials:** three independent runs per task and profile.
- **Budget:** USD 4.00 per task/profile run.

Baseline and zvec-grep run with identical settings. Index construction is
measured separately, and the reference answers remain isolated from both
profiles.

## Metrics and reporting

Job Summary cells use `baseline / zvec-grep / change`. Each task's baseline and
zvec-grep values are profile means across the three trials.

| Metric | Change | Interpretation |
| --- | --- | --- |
| Judge | `zvec-grep - baseline` | Positive means higher judged quality. |
| `input_token`, `toolcall`, agent wall time | `(zvec-grep - baseline) / baseline` | Negative means lower resource usage. |

A zero baseline denominator produces `N/A` for that efficiency comparison.
Index setup time is retained separately from agent wall time.

In the Aggregate row:

- Judge values are equal-weight means across tasks.
- Baseline and zvec-grep efficiency values are sums of the per-task profile
  means.
- The displayed efficiency change is the equal-weight mean of task-level
  changes, not a ratio of the aggregate sums.
- An `N/A` task is excluded only from the affected Aggregate metric.

## Local setup

Harbor runs the pinned task environments in Docker. Use the same host platform,
Claude Code version, and provider configuration for comparable results.

Install these prerequisites:

- [uv](https://docs.astral.sh/uv/)
- Docker Engine or Docker Desktop with Docker Compose v2
- Node.js 22 or newer and npm

Verify Docker Compose, install the locked dependencies, and export the Claude
and embedding credentials:

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

Validate the pinned assets, then run the profile-aware preflight:

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

The local package path requires the repository-root `npm ci` shown above.

## Local smoke and dry run

Inspect the locked task selections with:

```sh
zg-bench list tasks swe-qa-bench --tier smoke
zg-bench list tasks swe-qa-bench --tier full
```

Start with a dry run of the five-task, three-trial-per-profile configuration:

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

Remove `--dry-run` to execute the five-task smoke run. To run the published
20-task paired-agent protocol, change `--tier smoke` to `--tier full`. The
runner pins Claude Code `2.1.212`, Claude Opus 5, high reasoning effort, and the
USD 4.00 per-profile budget for both Baseline and zvec-grep. Harbor trajectories
and verifier output are written to `runs/`; the command does not automatically
recreate the published LLM Judge and aggregate report.

## Diagnose a failed run

When a trial records an exception, `zg-bench` prints the structured agent or
zvec-grep setup error and exits non-zero. Inspect a saved run with:

```sh
zg-bench diagnose --latest
zg-bench diagnose <job-name>
```
