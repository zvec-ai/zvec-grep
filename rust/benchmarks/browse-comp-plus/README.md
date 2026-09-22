<p align="right">
  English | <a href="./README_CN.md">中文</a>
</p>

# BrowseComp-Plus

[BrowseComp-Plus](https://github.com/texttron/BrowseComp-Plus) evaluates
deep-research agents on reasoning-intensive questions that require locating and
connecting evidence across multiple documents. Rather than searching the live
web, it uses a fixed corpus of roughly 100,000 human-verified documents,
enabling controlled and reproducible retriever comparisons.

This benchmark runs a native paired evaluation of Codex on that corpus.

It follows the original paper's core principles, with small differences in
corpus processing and evaluation protocol to better reflect real-world use of a
general-purpose agent.

Each query is evaluated in independent paired trials with the same model,
prompt, corpus, Codex settings, and limits:

- **Baseline:** Codex with its standard set of tools.
- **zvec-grep:** the same Codex setup, with only the zvec-grep MCP tools and
  usage instructions added by `zg --install`.

The benchmark records answer quality, token usage, wall-clock time, tool calls,
and complete Codex trajectories.

## Results

The [latest full report](./LATEST_REPORT.md) contains the complete results and
reproduction details. The sections below summarize the study: across 300 paired
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

From this directory, install the Python environment and verify the host:

```sh
cd benchmarks/browse-comp-plus
uv sync
source .venv/bin/activate
zg-bench doctor
```

The host environment should provide:

- macOS or Linux with `uv`;
- an installed and authenticated Codex CLI;
- `zg` installed.

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

The Codex model and reasoning effort are configured in `benchmark.toml`. The
runner validates the configured model before creating trials.

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

Evaluate the latest run with a blind Codex judge and generate its final report:

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

## Artifacts

Generated data is stored under `artifacts/` and is not committed. It contains
the pinned source snapshots, materialized corpus, reusable index, run-local
isolated profiles, raw attempts, evaluator inputs, and reports. Gold data and
manifests remain outside the agent workspace.
