<p align="right">
  English | <a href="./README_CN.md">中文</a>
</p>

# BrowseComp-Plus

[BrowseComp-Plus](https://github.com/texttron/BrowseComp-Plus) evaluates
deep-research agents on reasoning-intensive questions that require locating and
connecting evidence across multiple documents. Rather than searching the live
web, it uses a fixed corpus of roughly 100,000 human-verified documents,
enabling controlled and reproducible retriever comparisons.

The current harness runs paired evaluations with **OpenCode 1.18.4 and
`custom-openai/qwen3.8-max`** on that corpus. Both execution and blind judging
use this model. Retrieval uses the local **`local/potion-multilingual-128m`**
embedding model; no remote embedding service or embedding API key is needed.

It follows the original paper's core principles, with small differences in
corpus processing and evaluation protocol to better reflect real-world use of a
general-purpose agent.

Each query is evaluated in independent paired trials with the same model,
prompt, corpus, OpenCode settings, and limits:

- **Baseline:** OpenCode with local tools; web search, web fetch, and subagents
  are disabled.
- **zvec-grep:** the same OpenCode setup, with only the zvec-grep MCP tools and
  usage instructions added by `zg --install --target opencode`.

The benchmark records answer quality, token usage, wall-clock time, tool calls,
and complete OpenCode trajectories.

## Published results (historical Codex protocol)

The [published full report](./LATEST_REPORT.md) and figures below use Codex
`0.147.0` with `gpt-5.6-sol` and `qwen/qwen3.7-text-embedding` embeddings.
**They are not results for the current OpenCode/Qwen/Potion configuration.**
Changing the agent, embedding model, and tool protocol means new runs must be treated as a separate
study, not as a direct regression against these figures.

The sections below summarize that study: across 300 paired
trials, zvec-grep maintained answer quality while reducing average input tokens
by **37.56%**, tool calls by **43.52%**, and Agent time by **38.58%**.

### Study configuration

The study uses 100 cases to balance coverage, runtime, and cost. Cases are
selected in the pinned Hugging Face test-split order rather than sampled
randomly. We found no obvious ordering bias in this portion. Following a fixed
published order also minimizes discretion in selecting cases that might favor
zvec-grep.

Cases are excluded only when review confirms a defect in the original dataset,
such as inconsistent clues or insufficient corpus evidence to determine a
defensible answer. Each exclusion is documented in
[`suites/study.txt`](./suites/study.txt).

| Setting | Value |
| --- | --- |
| Evaluation scale | 100 cases · 300 paired trials |
| Agent | `gpt-5.6-sol` · `high` reasoning |
| Embedding model | `qwen/qwen3.7-text-embedding` |

### Primary results

All 300 paired trials are included in the averages. Changes show zvec-grep
relative to Baseline.

| Metric | Baseline | zvec-grep | Change |
| --- | ---: | ---: | ---: |
| Answer accuracy | 98.67% | 99.00% | +0.33 pp |
| Input tokens | 1.68M | 1.05M | **−37.56%** |
| Tool calls | 25.42 | 14.36 | **−43.52%** |
| Agent time | 259.4 s | 159.3 s | **−38.58%** |

zvec-grep index preparation is measured and reported separately from Agent
execution.

### What we observed

- **Why it helps.** On cross-document questions with paraphrased clues,
  zvec-grep can surface the correct entity before broad corpus scans. This
  narrows the effective search space and reduces candidate-discovery cost.
- **How often it helps.** zvec-grep reduced average input tokens by 37.6%; the
  median case saw a 25.8% reduction, and 67 of 100 cases used fewer tokens while
  maintaining answer quality. The remaining cases generally involved smaller
  runs and smaller absolute differences. The average reduction exceeds the
  median because a few expensive Baseline search tails were substantially
  shortened, while the 25.8% median and 67% case win rate show that the gains
  extended beyond those outliers.
- **Where gains narrow.** When exact search is already sufficient—or semantic
  results are relevant but not decisive—the Agent may repeat verification with
  grep. These regressions were also less consistent across trials, suggesting
  that much of the downside came from additional retrieval overhead and
  run-to-run variation rather than a broad, systematic regression. Precise
  verification and timely stopping therefore remain important.

Overall, zvec-grep provides the most value when semantic retrieval can narrow a
large or ambiguous search space, where it can prevent costly exploration tails.
When a task is already well served by exact keyword search, its incremental
benefit is smaller and results tend to remain closer to Baseline.

The BrowseComp-Plus dataset also contains minor imperfections; for example, some
query clues do not fully align with their source documents. This resembles
real-world information environments, where evidence may be incomplete or
conflicting. Cases without enough corpus evidence for a defensible answer are
excluded, while cases with sufficient evidence for the core answer are retained.
Reconciling such clues can require extra exploration and increase run-to-run
variation.

## Prerequisites

Use macOS or Linux with `uv`, Node.js, and `zg` installed. Install the pinned
OpenCode CLI, then configure a model API key and the matching OpenAI-compatible
base URL for your provider workspace. Potion embeddings run locally; the model
files download on first use.

```sh
cd benchmarks/browse-comp-plus
npm install --global opencode-ai@1.18.4
uv sync --frozen
source .venv/bin/activate

# Read credentials from your environment or secret manager.
export OPENAI_API_KEY="$QWEN_API_KEY"
export BROWSE_COMP_MODEL_BASE_URL='https://YOUR-WORKSPACE.cn-beijing.maas.aliyuncs.com/compatible-mode/v1'
zg-bench doctor
```

No interactive login is needed. Profiles use isolated OpenCode configuration
and data directories; provider credentials are read from `OPENAI_API_KEY`.

## Prepare the benchmark

Download the pinned official data, materialize every corpus `text` field
unchanged as `<docid>.md`, and build the reusable index:

```sh
zg-bench prepare
```

Initial preparation requires network access and sufficient disk space for the
downloaded data, materialized corpus, and index.

Subsequent runs reuse completed download, corpus, and index stages.

## Run

Verify the complete paired workflow on one query:

```sh
zg-bench run --suite smoke
```

Model and request settings are configured in `benchmark.toml`: thinking enabled,
`high` reasoning effort, requested temperature `0`, seed `42`, a 20,000-token
output limit, and a 1,000,000-token context limit. `max_output_tokens` is sent as
`max_tokens` for each model response; on Qwen it limits answer output, not the
total reasoning or token budget of a multi-turn trial. Provider-side interpretation
of these settings is model-specific. Automatic compaction and delegated
subagents are disabled so all trial usage belongs to the recorded session.

The current configuration uses `trials_per_case = 1` and
`infrastructure_retries = 0`: one baseline/zvec-grep pair per query, with no
automatic infrastructure-failure retries by the harness. Thus `smoke` runs one
pair / two agent runs and `ci` runs five pairs / ten agent runs. These iteration
settings differ from the historical three-trial study above.

Run the fixed random 5-query CI subset:

```sh
zg-bench run --suite ci
```

Run the fixed study subset:

```sh
zg-bench run --suite study
```

Run all cases in the pinned official dataset:

```sh
zg-bench run --suite full
```

## Evaluate and report

Evaluate the latest run with a blind OpenCode/Qwen judge and generate its final report:

```sh
zg-bench evaluate
```

For the `smoke` suite only, evaluation also audits the zvec-grep profile's tool
trace and reports whether zvec-grep was used correctly. This audit is separate
from the blind answer-correctness judgement.

Specify a run explicitly when needed:

```sh
zg-bench evaluate <run-id>
```

Regenerate the latest run's token, timing, completion, and paired-case report:

```sh
zg-bench report
```

Specify a run explicitly when needed:

```sh
zg-bench report <run-id>
```

Delete all runs and generated reports while preserving the downloaded data,
workspaces, and reusable index:

```sh
zg-bench clean
```

## GitHub Actions (manual)

The [BrowseComp-Plus workflow](../../.github/workflows/browse-comp-plus.yml)
is started manually by a repository maintainer or administrator. It does not
run on pushes, PRs, comments, or a schedule.

1. Configure the Actions settings below.
2. To test the new workflow before merging it, dispatch the already registered
   **ZG Bench** workflow from this branch; it routes to BrowseComp-Plus. After
   BrowseComp-Plus is registered on the default branch, use **Actions →
   BrowseComp-Plus → Run workflow** directly. Select a trusted workflow branch
   and set `candidate_ref` to the trusted branch, tag, or commit to benchmark
   in this repository.
3. Choose `ci` (default: 5 cases, 5 paired trials / 10 agent runs) or `smoke`
   (1 case, 1 paired trial / 2 agent runs, plus the usage audit). Set
   `parallel_cases` to 1, 2, or 5 (default) to fit provider rate limits.

Run the branch version without merging:

```sh
gh workflow run zg-bench.yml --repo zvec-ai/zvec-grep \
  --ref dev/browse-comp-plus-ci \
  -f benchmark=browse-comp-plus \
  -f candidate_ref=main -f suite=ci -f parallel_cases=5
```

The BrowseComp-Plus route does not require `pr_number`; the existing retrieval
route still validates it. Both routes retain the manual-trigger and
maintainer/admin checks.

Leave `prepared_run_id` empty to find an exact matching preparation artifact
from an earlier manual run on the same branch. To recover a specific preparation,
set it to that run's numeric ID and use the same candidate commit. A missing,
expired, or incompatible explicitly requested snapshot fails without rebuilding.

| Actions setting | Purpose |
| --- | --- |
| Secret `QWEN_API_KEY` | API key authorized to invoke `qwen3.8-max`. The existing `GLM_API_KEY` secret is used as a fallback when `QWEN_API_KEY` is absent. |
| Variable `BROWSE_COMP_MODEL_BASE_URL` | Required model OpenAI-compatible base URL, ending in `/compatible-mode/v1` for Bailian; falls back to `SWE_QA_MODEL_BASE_URL`. |
| Secret `HF_TOKEN` (optional) | Hugging Face download authentication. |

The workflow pins OpenCode `1.18.4` and uses `custom-openai/qwen3.8-max` for
both agent execution and judging, with local `local/potion-multilingual-128m`
embeddings, the pinned dataset, and one paired trial per case. A small model
preflight checks access before indexing. The selected
key is supplied as `OPENAI_API_KEY` to each model-calling step; no Codex login or
account `auth.json` is used. [OpenCode configuration](https://opencode.ai/docs/config/)
references the key through an environment variable. Temporary homes and profiles
are excluded from caches and evidence artifacts.

**Execution time:** preparation runs once, then cases run on independent runners.
Each case runs baseline and zvec-grep sequentially on the same runner in the
suite-wide randomized A/B order planned once. Judges run alongside the other
cases, and the final report recomputes metrics from raw results without more
model calls. This changes the cross-case scheduling from the local serial run;
the original metadata for every case is retained under `shards/`.

| Stage | Reuse / parallelism |
| --- | --- |
| Rust package | Cached for the exact candidate commit. |
| Source data | Cached immediately after download and verification, so a failed index build does not require downloading the corpus again. |
| Query decoding | Read only questions, reference answers, and gold/evidence document IDs from Parquet; skip unused document bodies while preserving every query and scoring input. |
| Corpus, index, and local model | A seven-day artifact includes the Potion model cache. Earlier runs and case workers reuse it only for the same candidate, dataset, embedding configuration, preparation code, OS/architecture, and workspace path. No fallback to another candidate's index. |
| Preparation on an exact artifact match | Download only the earlier run's small query artifact, verify its candidate and query digest, and create a fresh evaluation plan. Each case downloads and validates the complete snapshot; preparation avoids a redundant multi-GB transfer and extraction. |
| Case execution | Up to five independent runners; no shared daemon or mutable index. |
| Partial rerun | “Re-run failed jobs” keeps successful case artifacts; aggregation accepts only the current plan and latest attempt for each case. |

**Cold-run cost:** both suites prepare all 100,195 documents. A new preparation
identity or an expired artifact requires a local Potion index build. The first
[measured run](https://github.com/zvec-ai/zvec-grep/actions/runs/40438698350)
indexed all documents with no failures in 112 minutes 32 seconds and produced
a 15.9 GiB compressed preparation snapshot. End-to-end speedup has not yet been
measured. Preparation and each case allow six hours. As in SWE-QA Bench, concurrency is scoped to
the run ID and attempt, so separately requested runs do not block or cancel
one another. Each hosted runner frees SDK space and requires 50 GiB free.
The compressed snapshot preserves nanosecond timestamps to avoid reindexing.
It uses artifact storage because it exceeds the repository's 10 GB Actions
cache limit; only source data and candidate packages use Actions cache. A run
that finds a matching artifact shares the original with its case jobs
without uploading another copy. Allow for multi-GB artifact transfer/storage.
Embeddings use runner CPU; even `smoke` incurs agent and judge API costs.

The small plan artifact is retained for seven days. Retry failed jobs while
the prepared artifact still exists; after expiry, start a new run. Explicitly
restoring an older run-ID-named snapshot republishes it under its exact
preparation identity without rebuilding or recompressing the index.

The candidate is built in a separate job without model credentials. Only select
trusted candidate code, which subsequently executes on the benchmark runners.
The harness and candidate commit SHAs are recorded separately.

Each profile runs once per case. The harness does not automatically retry
infrastructure failures (`infrastructure_retries = 0`), including transient
HTTP 429/5xx errors and provider errors marked retryable. A manual failed-job
rerun repeats that case's pair; it does not resume individual successful trials.
Successful trials and low scores do not trigger retries. The job summary separates successful
trials, eligible paired trials, extra retries, failed attempts, final failures,
and unfinished trials. Paired metrics use the final completed attempts only;
failed-attempt overhead remains available in raw evidence. Both profiles use
the same model settings, pinned samples, and paired scoring. Both receive their exact
absolute working directory in the same task template, with instructions to copy
path arguments verbatim; this reduces directory-name typos that obscure tool
behavior. Tool failures remain visible in the report and raw evidence.

Results appear in the Actions job summary. Retention follows SWE-QA Bench:

| Artifact | Contents | Retention |
| --- | --- | --- |
| Case / aggregate evidence | JSON/Markdown/CSV reports, trajectories, judge results, and candidate metadata | 30 days |
| `task-report` / `aggregate-report` | Compact reports and metadata extracted from secret-scanned evidence | 90 days |
| Preparation diagnostics | Preparation logs and candidate metadata | 7 days |

Evidence export and upload are attempted even after failure, cancellation, or
timeout, while the runner remains available. Partial runs remain failed but
completed pairs are still judged; cancellation stops further judging.
Missing credentials, runtime failures, incomplete trials, or incomplete judging
fail the workflow; accuracy and efficiency are reported without a regression
threshold. Evidence excludes profiles, runtime credentials, and corpus files;
upload is blocked if configured secret values are detected.

Run the CI integration checks locally without model calls or dataset downloads:

```sh
uv run --frozen python -m unittest discover -s tests -v
```

Set `OPENCODE_BENCHMARK_TEST_BINARY=opencode` to also check the installed CLI
against the offline protocol fixtures.

## Artifacts

Generated data is stored under `artifacts/` and is not committed. It contains
the pinned source snapshots, materialized corpus, reusable index, run-local
isolated profiles, raw attempts, evaluator inputs, and reports. Gold data and
manifests remain outside the agent workspace.
