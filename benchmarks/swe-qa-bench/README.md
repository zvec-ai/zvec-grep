<p align="right">
  English | <a href="./README_CN.md">中文</a>
</p>

# SWE-QA benchmark

This benchmark measures how `zvec-grep` affects an agent's ability to answer
repository-level software-engineering questions. The canonical comparison uses
the same OpenCode agent, model, task prompt, repository commit, environment, and
limits for both profiles:

- **Baseline:** OpenCode uses its standard tools.
- **zvec-grep:** the same agent receives a prepared repository index and uses
  zvec-grep through MCP.

Index construction is measured separately and is not included in agent wall
time.

## Benchmark definition

The [`SWE-QA Bench`](../../.github/workflows/swe-qa-bench.yml) workflow runs a
pinned 20-task subset of
[`peng-weihan/SWE-QA-Bench`](https://github.com/peng-weihan/SWE-QA-Bench).
The benchmark inputs are locked in this directory:

- [`selection.json`](zg_bench/swe_qa/data/selection.json) records task IDs,
  task slugs, repository commits, asset hashes, and CI scope membership.
- [`references.json`](zg_bench/swe_qa/data/references.json) contains the
  isolated references used by the independent judge; agents cannot access it.
- [`datasets/`](datasets/) contains the
  pinned Harbor task environments, prompts, and verifiers.
- [`swe-qa-bench.yaml`](suites/swe-qa-bench.yaml) exposes the
  local dataset to the benchmark runner.

The workflow validates the locked selection, repository commits, hashes, and
reference isolation before starting model-backed jobs.

## CI scopes

- Same-repository pull requests and pushes to `main` run the three cases shown
  in the root README: `pylint:10`, `matplotlib:37`, and `django:32`.
- Fork and Dependabot pull requests run locked-asset validation, unit tests, and
  a dry-run preflight only, without model credentials.
- Manual `workflow_dispatch` with `scope=smoke` runs the same three cases.
- Manual `workflow_dispatch` with `scope=all-full` runs all 20 pinned tasks.

From the repository root, maintainers can trigger the manual scopes with the
GitHub CLI:

```sh
gh workflow run swe-qa-bench.yml -f scope=smoke
gh workflow run swe-qa-bench.yml -f scope=all-full
```

`all-full` is a workflow scope, not a `zg-bench --tier full` value. The local
suite stores its complete 20-task selection in the `ci` tier, and the workflow
passes the tasks selected for each scope explicitly.

Each selected task runs three baseline trials and three zvec-grep trials on the
same runner. All six answers are judged independently. The workflow is
report-only: numeric results do not gate review or merging, but every expected
profile run and judge call must complete successfully.

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

CI runs on Ubuntu 24.04 with Python 3.12 and Node.js 24. Local harness runs also
support Docker on macOS, although comparable benchmark results should use a
consistent Linux x86-64 environment.

Install these prerequisites:

- [uv](https://docs.astral.sh/uv/)
- Docker Engine or Docker Desktop with Docker Compose v2
- Node.js 22 or newer and npm

Verify Docker Compose, install the locked dependencies, and export the model
credential used by the workflow:

```sh
docker compose version
npm ci

cd benchmarks/swe-qa-bench
uv sync --frozen
source .venv/bin/activate
export GLM_API_KEY="your-api-key"
```

Run the same profile-aware preflight used for the SWE-QA configuration:

```sh
zg-bench doctor \
  --agent opencode \
  --model custom-openai/glm-5.2 \
  --profile all \
  --embedding-model local/potion-code-16m-v2 \
  --zvec-grep-package ../..
```

The local package path requires the repository-root `npm ci` shown above.

## Local smoke and dry run

Inspect the locked task selections with:

```sh
zg-bench list tasks swe-qa-bench --tier smoke
zg-bench list tasks swe-qa-bench --tier ci
```

Start with a dry run of the three-task, three-trial-per-profile configuration:

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

Remove `--dry-run` to execute the paired Harbor run. The local runner writes
trajectories and evaluator output to `runs/`; the GitHub Actions workflow is the
canonical path for collecting pairs, independently judging every answer, and
producing the aggregate report.

## Diagnose a failed run

When a trial records an exception, `zg-bench` prints the structured agent or
zvec-grep setup error and exits non-zero. Inspect a saved run with:

```sh
zg-bench diagnose --latest
zg-bench diagnose <job-name>
```
