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
zvec-grep values are profile means across its configured trials (five in CI).

| Metric | Change | Interpretation |
| --- | --- | --- |
| Judge | `zvec-grep - baseline` | Positive means higher judged quality. |
| `input_token`, `toolcall`, agent wall time | `(zvec-grep - baseline) / baseline` | Negative means lower resource usage. |

A zero baseline denominator produces `N/A` for that efficiency comparison.
Index setup time is retained separately from agent wall time.

New OpenCode trials use `collect_session_usage=true`. Before the environment
stops, the adapter exports the root session and every recursively linked
subagent session to `agent/session-usage.json`. Collection requires a complete
export; missing sessions or inconsistent totals fail the trial/report instead
of falling back to the root trajectory. Reports mark this scope as
`opencode-session-tree-v1`. JSON reports and session artifacts retain the
root/subagent breakdown; Markdown reports omit the descendants table. This
presentation change does not remove subagent usage from the reported totals.

- Input tokens include uncached input, cache reads, and cache writes. Output
  tokens include text output and reasoning; their components are retained.
- Tool calls include the root delegation and each child's own calls, including
  nested descendants. Session/message/tool identities prevent double counting.
- Agent wall time already includes awaited subagent execution. Child durations
  are not added; the measured session-export overhead is subtracted.
- The judge still scores only the root's final answer. Judge usage is separate.
- A complete session export covers persisted task sessions, not the provider's
  entire bill: background title/summary calls that are not stored in the
  session database cannot be counted. Missing provider costs remain unknown.

Historical artifacts without session exports remain readable as `legacy-root`:
their child token/tool usage is unknown and may be omitted. They are rejected
when mixed with session-tree trials or reports, including old reports without
an explicit scope. Older input/output totals also used a different token
breakdown, so resource deltas must not be compared across these scopes as if
their accounting were identical. Failed attempts remain archived separately
under `.retry-history/` and are not included in final-success trial means.

The Aggregate summary appears first, before the task details. Each workflow
run independently filters tasks using each profile's trial means:

- Judge difference: `zvec-grep - baseline`, measured in score points. A
  difference strictly outside `[-10, +10]` excludes the task; exactly `-10` or
  `+10` remains included. This threshold is a score difference, not a percentage.
- Input-token change: `(zvec-grep - baseline) / baseline * 100`. A change
  strictly outside `[-100%, +100%]` excludes the task; exactly `-100%` or
  `+100%` remains included. With nonnegative token counts, only an increase
  above `100%` can cross this threshold. A zero baseline with positive
  zvec-grep input is excluded because its percentage change is undefined;
  two zero inputs remain included.

A task that meets either exclusion condition is removed from every Aggregate
metric and the main results table. Tasks meeting both conditions are counted
once. Judge exclusions appear in a separate table with baseline and zvec-grep
mean scores, the score difference, and the exclusion reason; the report also
identifies tasks excluded by the input-token condition.

- Judge values are equal-weight means across the included tasks.
- Baseline and zvec-grep efficiency values are sums of the included per-task
  profile means. Their displayed change is calculated directly from those two
  aggregate values, not by averaging task-level percentage changes.
- A zero aggregate baseline denominator produces `N/A` for that efficiency
  comparison. If no tasks remain, the Aggregate values are `N/A`.
- JSON `cases` and raw artifacts retain every task, including excluded tasks
  and child usage.

This is a sensitivity filter, not evidence that excluded data is invalid. All
selected tasks still execute and must complete their required trials and judge
calls; filtering cannot turn an incomplete benchmark into a successful one.
For `all-full`, each round still runs 20 tasks × 2 profiles × 5 trials. An
otherwise complete run with no tasks left after filtering may still succeed.

## GitHub Actions

Developers can follow the [fork run guide](docs/run-in-fork.md) to configure
their API key, Bailian endpoint, Rust candidate ref, and manual dispatch.

The [SWE-QA Bench workflow](../../.github/workflows/swe-qa-bench.yml) runs through
manual `workflow_dispatch` or an explicit `run_e2e: true` dispatch of
[ZG Bench](../../.github/workflows/zg-bench.yml). Pushes, automatic PR checks and
`@zg-bench` comments do not enable e2e.
Core maintainers are users with the repository's exact `admin` or `maintain`
role. The first step of every job checks both `github.actor` and
`github.triggering_actor` against their current roles, before checkout or
model-secret use. This also checks individual job reruns; missing permissions
or a failed permission lookup stop execution. GitHub users with write access
may still see and use the dispatch/rerun controls, but the workflow rejects
unauthorized benchmark execution.

The separate [SWE-QA Offline workflow](../../.github/workflows/swe-qa-offline.yml)
runs path-scoped Python and shared Node regression tests on pull requests and
`main` pushes. It does not read model credentials, build a candidate, or run
Harbor/model trials.

`workflow_dispatch` defaults to `repro-3` (3 tasks); `all-full` (20 tasks) and
`smoke` (5 tasks) remain available. `candidate_ref` defaults to `main` and
selects the Rust source branch, tag, or commit to build as the candidate npm
package; the harness checkout and candidate source are separate. Its `model` input defaults to
`glm-5.2`; select `qwen3.8-max` to run the same protocol with Qwen. The selected
model is used for both execution and judging. The independent `embedding` input
defaults to `local`, using `local/potion-code-16m-v2`; select `remote` to use
`qwen/qwen3.7-text-embedding`, matching the Retrieval-only workflow.

The `repro-3` scope runs 3 tasks × 2 profiles × 5 trials = 30 trials. Its fixed
tasks were selected from [run 35206585943](https://github.com/Cuiyus/zvec-grep/actions/runs/35206585943)
for small reproduction and iteration runs. Each range below covers the five
final successful trials in that historical run:

| Relative variability | Task | Baseline score / tool calls | zvec-grep score / tool calls |
|---|---|---|---|
| Low | `reflex:6` | 87–96 / 7–12 | 84–92 / 3–5 |
| Medium | `requests:16` | 68–97 / 2–6 | 75–99 / 7–10 |
| High | `conan:39` | 5–92 / 2–24 | 5–100 / 3–19 |

These are descriptive historical strata based on investigation paths and score
variation, not statistical significance levels or intrinsic model properties.
The low group still has differing paths; one high-group baseline trial delegates
to a subagent whose internal calls are absent from the main trace. Compare new
trials against the same three historical tasks, and account for failed-attempt
overhead separately.

CI uses OpenCode `1.18.4` with `custom-openai/glm-5.2` and local embedding by
default, with five trials per task and profile. Configure the repository's
`GLM_API_KEY` Actions secret for agent
execution and judging. The existing secret name is retained for both models;
its Bailian business-space API key must have access to the selected model.
Remote embedding additionally requires the `QWEN_EMBEDDING_API_KEY` Actions
secret and `QWEN_EMBEDDING_ENDPOINT` repository variable. Source content is sent
to that remote embedding endpoint only when `embedding=remote`.
The Claude Code configuration above describes the published local protocol.
CI reads the fixed trial count, failure retry limit, and local/remote embedding
model mapping from [`ci-config.json`](ci-config.json). The workflow's execution
model, embedding runtime, scope, and Rust source are manual run inputs;
credentials and endpoints are runtime settings.

The SWE-QA suite explicitly configures the versioned
[`index.ignore`](zg_bench/swe_qa/data/index.ignore) file. It contains only three
fixed benchmark fixtures that have text suffixes but contain a PNG or malformed
UTF-16/32 data. The ignore-file digest is part of the local index-seed identity;
any other indexing failure remains fatal and visible in the task result. This
setting belongs to the suite and is not inherited by other runner suites.

The full run contains 20 tasks × 2 profiles × 5 trials = 200 independent
trials. CI passes `--max-retries 2`: an exception, including an agent timeout,
can trigger at most two additional attempts of the same trial. API usage-limit
errors are not retried. Successful trials and low scores are not retried, and
retry attempts do not increase the five-trial sample count. Local runs default
to no retries unless `--max-retries` is supplied.

Failed attempts are preserved under each Harbor job's `.retry-history/` and
uploaded with the raw evidence. Job summaries show retry counts and remaining
errors. Report token, tool-call, time, and cost metrics describe the final
successful attempt of each trial; they exclude failed-attempt overhead, which
remains in the archived evidence. Exhausted retries still fail the task.
Each task job has a six-hour ceiling, including setup and retries.

Both OpenCode profiles, their delegated agents, and the judge use the selected
model with the following requested settings, shared in `zg_bench/settings.py`:

| Parameter | Qwen3.8 Max / GLM-5.2 execution and judging |
| --- | --- |
| `temperature` | `0` |
| `seed` | `42` |
| `enable_thinking` | `true` |
| `reasoning_effort` | `"high"` |
| `max_tokens` | `32000` |
| `response_format` | Omitted |

The same seed is used for every trial and retry. The custom-openai Qwen3.8 Max
and GLM configurations and the DashScope GLM configuration set `reasoningEffort = "high"`; the
OpenAI-compatible SDK maps this to the HTTP field `reasoning_effort`.
Execution sets the model's `limit.output` explicitly, and the judge uses the
same `BENCHMARK_MAX_OUTPUT_TOKENS` constant for `max_tokens`.

The [Bailian Chat Completions reference](https://help.aliyun.com/zh/model-studio/qwen-api-via-openai-chat-completions)
documents model-specific behavior: Qwen3.8 Max raises a requested temperature
below `0.6` to `0.6` in thinking mode, and maps `high` reasoning effort to
`xhigh`. These requests therefore do **not** give Qwen an effective temperature
of zero. Also, `max_tokens=32000` limits Qwen's final answer, excluding thinking;
for GLM-5.2 without `thinking_budget`, it limits thinking and the answer together.
The benchmark retains the same requested fields across these models, but their
effective sampling settings and token budgets are not identical.

Qwen3.8 Max (custom-openai) and GLM-5.2 (custom-openai / DashScope) both
explicitly set `interleaved: {"field": "reasoning_content"}` to return historical
reasoning in a separate field without appending it to answer `content`. This
is an OpenCode message-mapping setting, not the switch that enables thinking.
Bailian defaults to Qwen `preserve_thinking=true` and GLM `clear_thinking=false`;
both use the reasoning history supplied by the client. Qwen requires complete
reasoning replay; omitting it is not a valid token-saving optimization. See the
[Bailian Chat reference](https://help.aliyun.com/zh/model-studio/qwen-api-via-openai-chat-completions)
and [GLM documentation](https://help.aliyun.com/zh/model-studio/glm).
The older, separate Qwen3.7 configuration still keeps thinking disabled.

**Interpreting historical configurations:** an absent `interleaved` field in
older GLM configurations does not mean reasoning history was omitted. With the
verified OpenCode `1.18.4`, `ai 6.0.168`, and `@ai-sdk/openai-compatible 2.0.41`,
both the default SDK path and the explicit mapping serialize nonempty reasoning
as `reasoning_content`. Empty reasoning may differ only by an omitted field
versus an empty string. The shared explicit setting makes the contract clear;
it does not justify rewriting historical reports or concluding that older GLM
runs omitted reasoning. This verifies client behavior, not every historical
HTTP request. Equivalent replay behavior does not imply equal reasoning volume,
effective budgets, or cache costs across models.

Both execution and judging omit `response_format`. The judge's prompt still
requires JSON, with strict score parsing and retries on invalid responses.
Its report metadata records `response_format = null` to indicate that the
request does not force a response format.
OpenCode declares the model's temperature capability and
passes the seed through every built-in agent's provider options, including
subagents, compaction, and title/summary generation. Judge reports record
temperature, seed, thinking, reasoning effort, output limit, and response
format. Aggregation rejects mismatched judge settings and a mix of new reports
with legacy reports missing the new settings; compatible legacy reports can
still be aggregated together. Earlier reports used a judge with thinking
disabled, so their scores should not be mixed with the new judging protocol.
These settings reduce sampling variance; identical responses still depend on
the model service.

Both OpenCode profiles deny `websearch` and `webfetch` globally and for every
built-in agent, including delegated agents. These tools are excluded from model
requests even when Harbor skips interactive permission prompts. Local repository
search, file reads, and the zvec-grep MCP remain available. This is a web-tool
restriction, not container network isolation; shell commands and setup/model
connections still have network access.

CI checks the pinned OpenCode binary against a local fake provider, inspecting
actual HTTP requests for sampling parameters, enabled Qwen3.8 Max / GLM thinking,
`reasoning_effort = "high"`, and web-tool restrictions. Reasoning checks cover
both models' tool and subagent continuations, multiple SSE reasoning chunks,
empty reasoning, and the legacy path without an explicit mapping, verifying
complete `reasoning_content` replay without mixing it into answer content.
These tests need no model credentials and do not measure live Bailian quality
or billing effects.

Each task runs Baseline and zvec-grep on the same runner, judges the paired
results, and uploads Harbor evidence and an independent task report as
artifacts. The final Aggregate is also generated when some tasks fail. It uses
only completed task reports: failed tasks do not enter any mean, total, filter,
or sample count, and are listed separately as missing. The workflow remains
failed so the missing coverage stays visible. **Re-run failed jobs** can reuse
successful task reports from earlier attempts of the same GitHub run. Each task
artifact includes a `task-report-manifest.json` binding its checksum to the
candidate commit/repository, harness revision, protocol hash, scope, models,
endpoint hashes and trial configuration. Aggregation validates those identities
and report evidence before selecting the newest valid report per task, then
recomputes the aggregate without new model calls. It never merges individual
trials from different attempts. The aggregate includes
`task-report-selection.json` with selected attempts and rejected inputs. Reports
without a compatible manifest or expired artifacts cannot be reused; start a
new full run or rerun missing tasks. Automatic trial retries remain within one
attempt.

## Code structure

The Python package follows the same responsibility boundaries as the
Retrieval-only benchmark: `core`, `engines`, `metrics`, and `reports`.
SWE-QA-specific inputs and Harbor integration remain explicit:

```text
swe-qa-bench/
  zg_bench/
    cli.py                 Stable zg-bench entrypoint
    runner.py              Suite/profile execution and setup caches
    retries.py             Failed-trial retry policy
    ci/                    Artifact checks called by workflows
    core/                  Shared errors, JSON I/O, model requests and report protocol
    engines/
      registry.py          Supported agent/model catalog and credentials
      judge.py             Model requests, score parsing and judge retries
      opencode/config.py   OpenCode model/provider configuration
    metrics/               Numeric checks, usage accounting and comparisons
    reports/               Report validation, aggregation and Markdown output
      retries.py           Trial retry summary rendering
    agents/                Stable Harbor adapter import paths and session export
    swe_qa/                Suite validation, pair collection and judge orchestration
      cli.py               Stable python -m zg_bench.swe_qa entrypoint
      selection.py         Locked CI scope selection and matrix output
      data/                Frozen selection and isolated reference answers
    settings.py            Version, endpoint and request constants
  suites/                  Runner suite configuration
  datasets/                Frozen Harbor tasks and environments
  tests/                   CLI, accounting, report and provider-contract tests
```

The suite adapter collects and validates execution evidence; the judge engine
scores final answers; metric functions operate on normalized records; reports
validate and aggregate those records before rendering. Metrics do not call a
model or depend on an agent engine, and Markdown formatting does not perform
model requests. The runner coordinates Harbor execution without owning report
formulas.

To add an execution model or agent, extend the engine registry and its provider
or Harbor adapter tests. Models used for both execution and self-judging share
one request specification in `core/protocol.py`; provider, credential and
endpoint resolution is shared through `engines/registry.py`. To add a CI task
scope, update `swe_qa/selection.py`.
To add a metric, define its calculation and accounting checks in `metrics/`,
then update report validation and rendering.
A new suite owns its input validation and evidence conversion rather than
adding dataset branches to shared metrics. Keep both CLI interfaces, Harbor
adapter import paths and frozen asset paths stable.

This aligns directory responsibilities with Retrieval-only; each benchmark
keeps its own Python or JavaScript implementation and statistical protocol.
Both workflows build Rust candidates through
[`rust-candidate-package`](../../.github/actions/rust-candidate-package/action.yml)
and verify manifests with the shared
[`rust-package-cache.mjs`](../shared/rust-package-cache.mjs).
Moving code between modules does not change task selection, trial counts,
usage scope or Aggregate exclusion rules.

## Local setup

The local Claude Code recipe below documents the historical published protocol.
The current GitHub Actions comparison builds and tests only the selected Rust
candidate described above.

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
