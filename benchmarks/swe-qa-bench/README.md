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

The [SWE-QA Bench workflow](../../.github/workflows/swe-qa-bench.yml) runs only
through manual `workflow_dispatch`; pushes and pull requests do not trigger it.
Core maintainers are users with the repository's exact `admin` or `maintain`
role. The first step of every job checks both `github.actor` and
`github.triggering_actor` against their current roles, before checkout or
model-secret use. This also checks individual job reruns; missing permissions
or a failed permission lookup stop execution. GitHub users with write access
may still see and use the dispatch/rerun controls, but the workflow rejects
unauthorized benchmark execution.

`workflow_dispatch` defaults to `repro-3` (3 tasks); `all-full` (20 tasks) and
`smoke` (5 tasks) remain available. Its `model` input defaults to
`glm-5.2`; select `qwen3.8-max` to run the same protocol with Qwen. The selected
model is used for both execution and judging.

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

CI uses OpenCode `1.18.4` with `custom-openai/glm-5.2` by default, the local
`local/potion-code-16m-v2` embedding model, and five trials per task and
profile. Configure the repository's `GLM_API_KEY` Actions secret for agent
execution and judging. The existing secret name is retained for both models;
its Bailian business-space API key must have access to the selected model.
The Claude Code configuration above describes the published local protocol.

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

Qwen3.8 Max enables `preserve_thinking` by default. Its OpenCode model config
sets `interleaved: {"field": "reasoning_content"}` so historical reasoning is
returned in the separate `reasoning_content` field, rather than concatenated
into answer content. See the [thinking guide](https://help.aliyun.com/zh/model-studio/deep-thinking)
and [Qwen3.8 Max model information](https://help.aliyun.com/zh/model-studio/qwen3-8-max).
The older, separate Qwen3.7 configuration still keeps thinking disabled.

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

CI also checks the pinned OpenCode binary against a local fake provider to
verify the sampling parameters, enabled Qwen3.8 Max / GLM thinking, the actual
`reasoning_effort = "high"` request field, and web-tool restrictions in
consecutive tool-calling requests, including Qwen's historical
`reasoning_content` field, without using model credentials.

Each task runs Baseline and zvec-grep on the same runner, judges the paired
results, and uploads Harbor evidence and an independent task report as
artifacts. Complete runs also produce an aggregate report. If some tasks fail,
the summary still shows completed task reports without presenting a partial
set as the full benchmark. Reports from separate GitHub run attempts are kept
separate; automatic trial retries happen within one attempt.

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
