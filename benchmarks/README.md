<p align="right">
  English | <a href="./README_CN.md">中文</a>
</p>

# Benchmarks

Use Retrieval-only to measure search quality across product versions. Use e2e
benchmarks to measure how access to zvec-grep affects an agent's answers and
resource use. Setup, frozen inputs and scoring details live with each suite.

## Contributors: run a benchmark

| Benchmark | Scope | Local instructions |
| --- | --- | --- |
| Retrieval-only | SWE-QA20, BEIR, DuRetrieval and Quarry; hybrid, fts and vector | [SWE-QA20 runner](zg-retrieval/README.md#run-locally), [expansion runners](zg-retrieval/expansion/README.md) |
| SWE-QA e2e | 20 software-engineering questions; baseline vs zvec-grep agent profiles | [Setup and smoke runs](swe-qa-bench/README.md#local-setup) |
| BrowseComp-Plus e2e | 100 questions over a fixed document corpus | [Setup and execution](browse-comp-plus/README.md) |

For development CI, use your own fork. In **Actions → Retrieval-only → Run
workflow**, select the harness revision, set `candidate_ref` to the Rust source
branch/tag/commit, and leave `embedding=local` for a run without model API keys.
See the [Retrieval guide](zg-retrieval/README.md#run-ci-and-read-the-result) for
single-version results and artifacts. The separate **Retrieval-only PR** manual
dispatch compares two full commit SHAs under one harness.

For optional SWE-QA e2e, follow the [fork run guide](swe-qa-bench/docs/run-in-fork.md)
to configure model credentials and dispatch a small `repro-3` or `smoke` run
before choosing `all-full`. Execution and judging both consume model quota.
Remote embedding also requires its own key and endpoint; see
[corpus size and embedding load](zg-retrieval/README.md#corpus-size-and-remote-embedding-load).

## Maintainers: defaults and additional runs

### Triggers and results

| Trigger | Runs | Results |
| --- | --- | --- |
| PR opened, reopened or updated, including fork PRs | Retrieval-only on the exact PR base and head; local embedding | Updated PR bot comment, Actions summaries and `retrieval-comparison` artifact |
| New PR Conversation comment with `@zg-bench` on its own line | The same Retrieval version comparison | One bot comment, updated from queued status to results |
| Manual **ZG Bench** dispatch with `pr_number` | Retrieval comparison; e2e only with explicit `run_e2e: true` | The same bot comment and run artifacts |
| Manual **SWE-QA Bench** dispatch | Selected e2e scope, model and embedding runtime | Task evidence and aggregate report |
| Relevant benchmark source/workflow changes | Offline benchmark regression checks | CI logs; no model calls |

Automatic PR checks require no maintainer role or model secrets. A new push
cancels the previous automatic run for that PR. All version comparisons show
**base / head / delta** for File Hit@1/5/10, MRR@10 and nDCG@10, with links to both
commits. `retrieval-base-*` and `retrieval-head-*` retain per-version evidence.

[Retrieval PR comment](../.github/workflows/retrieval-pr-comment.yml) publishes
the completed automatic run through a separate `workflow_run` job. It creates
one English result comment per PR and updates it on later runs and reruns,
including for fork contributions. It links both commits and the source run,
shows all twelve suite/mode rows with base / head / delta, and marks missing or
invalid results as incomplete. Cancelled runs and results for outdated base/head
commits are skipped. Manual comparison dispatches do not post this automatic
comment; `@zg-bench` retains its own per-request comment.

Comment and model-backed manual runs require the exact repository `admin` or
`maintain` role. Jobs recheck the current roles of both the original actor and
rerunner. Comment commands never enable e2e. Quoted or fenced commands, edited
comments, ordinary Issues and inline review comments do not trigger a run.
Both SHAs stay pinned if the PR changes during execution; submit a new command
to test the current versions.

### Deployment and retries

Merge the comment entry point and reusable workflows into the default branch,
and allow its comment jobs to use `issues: write` and `pull-requests: write`.
Only those jobs have write tokens. Benchmark execution uses read-only tokens,
a trusted harness checkout and a separately built candidate. Local Retrieval
receives no model credentials. Optional e2e needs `GLM_API_KEY`; review candidate
code before authorizing it to run with those credentials.

The automatic result publisher also needs the default-branch workflow and
Issues/Pull requests write permissions. Its trusted default-branch code checks
the source workflow, run, PR and commit identities through GitHub's API before
publishing. It reads only JSON artifacts, validates the expected metric table,
and renders its own Markdown. PR code and artifact scripts are never executed
by this job; the read-only benchmark jobs receive no write token.

`@zg-bench` is a command keyword, so it needs no registered account. Replies use
`github-actions[bot]`. A dedicated bot identity requires a GitHub App and changes
to the comment jobs' token and login configuration.

Use **Re-run failed jobs** to retry a failed run and update its original bot
comment. For e2e, aggregation validates task reports from all attempts of that
run against the candidate, harness, evaluation configuration and report checksum,
then selects the latest valid result per task. Earlier successes are retained;
missing tasks remain incomplete. The aggregate's `task-report-selection.json`
records chosen attempts and rejected inputs. Reports without the identity
manifest cannot be reused; runs started before that manifest was introduced
need a new full run. Expired artifacts also require rerunning the missing tasks.

## Benchmark developers: protocol and validation

### Protocol and implementation

Retrieval compares two product versions using the same frozen harness, inputs
and local model configuration, with a fresh index for each version. e2e instead
compares two agent profiles: standard tools, and the same setup with a prepared
zvec-grep index, tools and usage guidance. Keep task inputs, agent/model settings,
shared tools, limits and trial counts fixed between profiles. Report index
preparation separately from agent execution.

Keep reference answers, reports and prior outputs outside the searchable
workspace. Use general-purpose tool guidance, blind judging and repeated trials;
do not tailor prompts to reference answers or force tool use. Missing or invalid
evidence must not become zero scores or successful results.

Start with the [Retrieval code map](zg-retrieval/README.md#code-structure) or
[SWE-QA code map](swe-qa-bench/README.md#code-structure). Runners capture evidence;
validators check identities, protocol and coverage; metric functions score
validated records; aggregators and comment rendering publish the results.
The [comment workflow](../.github/workflows/zg-bench.yml) delegates execution to
reusable workflows and updates status through [bench-comment.mjs](../.github/scripts/bench-comment.mjs).

### Validate changes

From the repository root, run offline checks before a model-backed experiment:

```sh
node --test benchmarks/zg-retrieval/test/*.test.mjs .github/scripts/*comment.test.mjs
uv sync --project benchmarks/swe-qa-bench --frozen
uv run --project benchmarks/swe-qa-bench python -m unittest discover -s benchmarks/swe-qa-bench/tests -v
```

Lint changed workflows with `actionlint`. Use [offline replay](zg-retrieval/README.md#offline-replay-and-comparisons)
to check scoring/report changes against saved evidence, and use a fork run to
check workflow integration. Add a focused regression for changed failure,
identity or retry behavior. Record run-specific validation evidence in the PR;
keep these READMEs focused on current behavior and reproduction instructions.
