# Symbol-anchored caller/callee probes

`caller-callee-symbols` is an opt-in E2E diagnostic suite of five self-authored
questions about pinned Requests, Matplotlib and Sympy repositories. The repository
snapshots and Harbor/judging protocol come from SWE-QA-Bench; the five questions
are authored here, not copied from its upstream question set. QA20 stays separate.

| Task | Source question |
|---|---|
| requests:cc1 | Production callers of `resolve_redirects`, with call-site lines |
| matplotlib:cc1 | The method invoking `_make_line` through a local alias |
| sympy:cc1 | Direct `ode_order` callers in `systems.py`, including comprehensions |
| matplotlib:cc2 | Direct calls and nested evaluation order in `_make_line` |
| sympy:cc2 | `_get_func_order` project callees versus Python built-ins |

These probes test answers about actual source relationships. A resolved-call graph
need not represent aliases, constructors or built-ins exhaustively, so answers must
be checked against source. No question names a required tool or forces tool use.
Zero-call trials remain valid. This small sample does not establish general
SWE-QA quality gains or caller/callee adoption on natural concept questions.

## Inputs and Gold

- `selection-symbols.json` fixes question hashes, task IDs and repository commits.
- `references-symbols.json` holds judge-only source facts. Candidate tool behavior
  and experimental results are excluded. [Source review](symbol-probes-gold.md)
  records an independent AST cross-check of the manual source inspection.
- `datasets-symbols/` contains only the instructions, pinned environments and
  verifier scripts. Reference answers and review notes are never uploaded there.
- `suites/caller-callee-symbols.yaml` fixes smoke and full membership, enables graph
  indexing, and uses an ignore file that excludes only the uploaded ignore file.

The answering agents see the same question for baseline and zvec-grep. The existing
agent/model sampling, budgets, verifier and independent judge protocol apply.
The zvec-grep profile builds with `--graph`; graph mode is part of the index seed
identity. It uses the candidate's default MCP tools and instructions. Baseline
keeps its native tools. This suite does not change tool descriptions or guidance.

## Validate and run

From the repository root:

```sh
uv sync --project benchmarks/swe-qa-bench --frozen
uv run --project benchmarks/swe-qa-bench python -m zg_bench.swe_qa validate-symbols \
  --selection benchmarks/swe-qa-bench/zg_bench/swe_qa/data/selection-symbols.json \
  --references benchmarks/swe-qa-bench/zg_bench/swe_qa/data/references-symbols.json \
  --dataset benchmarks/swe-qa-bench/datasets-symbols

uv run --project benchmarks/swe-qa-bench zg-bench run caller-callee-symbols \
  --tier smoke --profile all --agent opencode --model custom-openai/glm-5.2 \
  --embedding-model local/potion-code-16m-v2 --n-attempts 1 --max-retries 2 \
  --zvec-grep-package /path/to/candidate.tgz --jobs-dir /tmp/symbol-probes
```

Local runs use the existing model credential configuration. See
[run-in-fork.md](run-in-fork.md) for Actions secrets and endpoint configuration.
To select this suite in the existing manual workflow:

```sh
gh workflow run swe-qa-bench.yml --repo OWNER/zvec-grep --ref YOUR_BRANCH \
  -f candidate_ref=FULL_CANDIDATE_SHA -f task_set=caller-callee-symbols \
  -f scope=smoke -f model=glm-5.2 -f embedding=local
```

`smoke` selects requests:cc1 and matplotlib:cc2 (incoming and outgoing probes).
`all-full` selects all five. The QA20-specific `repro-3` scope is rejected for this
suite. Actions retains its existing five trials per task/profile: smoke is 20
trials; all-full is 50. Local `--n-attempts` remains explicit. Exceptions may retry
at most twice in CI; low scores, empty results and absent tool calls do not retry.

## Reports and evidence

The same collect → judge → aggregate pipeline processes both suites, selecting the
matching references and expected tasks. Report identity includes the task set and
the protocol hash covers these dataset files. Artifacts belong to a specific run;
reports from different task sets must not be merged.

The existing summary sensitivity filter still applies. Per-task reports and JSON
`cases` retain every completed task, including excluded aggregate rows. Inspect
all five cases when assessing these diagnostics; do not present their aggregate
as a QA20 result. Custom-provider costs remain unknown when not returned.

The paired evidence artifact retains `agent/trajectory.json`,
`agent/opencode-session-snapshot.json`, `agent/session-usage.json` and
`agent/zvec-grep-setup.json` per trial. Session snapshots contain root/recursive
tool names and statuses for checking callers/callees use; setup records graph mode,
index timing and seed identity. Index preparation is measured separately from
agent wall time. No model request proxy or experiment-specific guidance is needed.
