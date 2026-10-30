# Retrieval-only: three ZG retrieval modes on 20 SWE-QA questions

Updated September 21, 2026. See the [benchmark README](./README.md) for commands and the [Retrieval-only workflow](../../.github/workflows/retrieval-only.yml) for execution.

## Execution

The workflow accepts manual `workflow_dispatch` events only. GitHub's workflow-ref selector chooses the benchmark harness (`main` for the latest merged harness). Its single `candidate_ref` parameter independently selects a branch, tag or commit containing the repository's `rust/` workspace and defaults to `main`. Every run tests **zg-hybrid, zg-fts and zg-vector** through the Rust public MCP endpoint. Push, pull-request and schedule events cannot trigger this benchmark.

At the start of a run, the workflow freezes the selected harness ref to an immutable commit. It checks out `candidate_ref` into a separate directory, resolves that source to another immutable commit, and builds from `candidate/rust/`. A native npm tarball cached under operating system, architecture and exact candidate commit bypasses compilation on repeat runs. If that package cache misses, a Cargo cache restores registry data and `rust/target` build objects before `npm run pack:local`. Node.js version is not part of candidate selection, cache identity or result identity.

Both the original actor and current re-run actor must have maintain or admin permission. Every job repeats that check, including partial re-runs; failed permission lookups deny execution. The checked-in action rejects write, triage and read roles. This check is not a permission boundary against contributors who can modify workflows or the local action on another branch; that requires repository- or organization-level execution policies.

The final **Retrieval results** job publishes the single overview table with three rows. Repository jobs upload evidence without separate main tables. Missing evidence, invalid experiments, product failures and successful runs have distinct statuses.

## Dataset and protocol

The 20 unchanged questions come from [Actions run 35206585943](https://github.com/Cuiyus/zvec-grep/actions/runs/35206585943), using SWE-QA-Bench revision `c13deac7a0d99b0ca2e593e004c4739475785b08`. The [source lock](./data/source.lock.json) freezes original questions, UTF-8 hashes, 11 repository commits and provenance fingerprints. There are five questions each in the what, where, how and why categories. No answer agent, query rewriting, subqueries or LLM judge participate.

Install the packed native candidate outside the corpus and connect through its product-generated stdio MCP configuration. Each repository gets a fresh code index and one MCP session; repository indexes are never cached. Questions, labels and reports are excluded from the corpus. Corpus and model inventories plus public Rust index-status aggregates are verified before and after retrieval. Model fingerprints exclude only the runtime completion marker `.zvec-grep-artifacts-<24hex>.complete`, which contains machine-specific timestamps; weights, tokenizer, configuration and other files remain hashed.

| Arm | Search argument | Retrieval routes |
| --- | --- | --- |
| zg-hybrid | `query: originalQuestion` | Native FTS and vector retrieval |
| zg-fts | `fts: [originalQuestion]` | Native FTS retrieval |
| zg-vector | `vector: [originalQuestion]` | Native vector retrieval |

FTS/vector requests omit the primary `query` and the other route. All requests use `limit: 10`, `autoUpdate: false`, `freshness: eventual` and `preferSymbol: false`. The harness sends no `preview` field so the default bounded `short` presentation is measured as returned. Disabling symbol preference prevents vector-only requests from adding a symbol-oriented FTS route. Public `matchedBy` evidence is checked against the requested mode. Native file aggregation, fusion and ranking remain product behavior under test; the harness adds no reranker.

Modes run in fixed order **hybrid → fts → vector**. Within each mode, each original question runs five consecutive times. All five results contribute quality and ranking-stability observations; only the fifth supplies output size. This gives **20 × 3 × 5 = 300 MCP calls** and **60 question/mode quality summaries**. Repetitions are not independent questions.

The protocol fixes the same model, corpus policy and index across modes. Shared runtime and model caches, together with fixed order, affect latency: later modes can reuse work initialized by earlier modes. Timings are observations under this protocol, not an unbiased speed comparison or cold-start benchmark.

## Metrics

| Metric | Definition and denominator |
| --- | --- |
| File Hit@1 | Mean of five binary rank-1 observations per question/mode, then equally across questions |
| File Hit@5 | Mean of five binary Top-5 observations per question/mode, then equally across questions |
| File Hit@10 | Mean of five binary Top-10 observations per question/mode, then equally across questions |
| File MRR@10 | Mean of five 1/r observations per question/mode; Top-10 misses contribute zero |
| nDCG@10 | Mean of five first-target binary discounted-gain observations per question/mode, then repository macro across 11 repositories |
| Stable Top 10 | Count of question/mode cases where all five ordered public result locations match |
| Mean output (KiB) | Mean UTF-8 bytes of successful fifth-call response text divided by 1024; normally 20 samples per mode |
| Avg RT / P50 RT (ms) | Mean and median duration of all successful MCP search calls; normally 100 samples per mode; excludes indexing |

All five quality metrics share [39 frozen relevant-file targets](./gold/files-v1.json), deduplicated from accepted annotation paths. Bridge-only paths earn no credit. Matching normalizes separators and accepts exact paths or directory-boundary suffixes without case folding. Preserve native ranks: repeated file chunks consume positions and are not collapsed or renumbered. Response length and visible declarations do not affect relevance.

For each question q, a target contributes only at its first matching native rank. Let g_i be 1 when at least one target first appears at rank i, otherwise 0, and T_q be the labeled target set:

```text
DCG@10(q)  = sum(g_i / log2(i + 1), i = 1..10)
IDCG@10(q) = sum(1 / log2(i + 1), i = 1..min(10, |T_q|))
nDCG@10(q) = DCG@10(q) / IDCG@10(q)
```

Unretrieved targets remain in the ideal-gain denominator; later chunks from an already matched file add no gain. Each question/mode first averages its five call-level scores. Hit/MRR then weight questions equally, while nDCG weights repositories equally after their within-repository means. Stability compares ranked public paths, ranges and match routes; equal quality scores alone do not imply stable retrieval. The JavaScript nDCG implementation is checked against byte-identical pinned Python functions in [`test/fixtures/ndcg-reference/`](./test/fixtures/ndcg-reference/), with their MIT license attribution preserved. This reference runs only in unit tests.

## Integrity and labels

Each question/mode/repetition has its own raw response and hash. Offline aggregation reparses public responses and verifies frozen identities, original questions, exclusive route arguments, omitted preview override, repetition count/order, corpus/model inventories and public index status. Three-mode coverage is mandatory even for an explicitly selected local subset. Saved derived scores are not trusted as scoring inputs.

Installation, indexing and product-call failures fail operational integrity. Product-call failures with valid evidence retain zero credit in the denominator. Failed calls are excluded from output and latency measurements. An isolated public-response format failure is excluded from diagnostic partial scores and listed by task ID, mode and reason; coverage shows the smaller denominator, and CI still fails. Protocol, identity, missing-call and other integrity errors withhold all aggregates. Quality values have no arbitrary pass threshold.

Labels came from AI-assisted source review and remain partial positives, without independent blind human validation. Original source anchors remain provenance and do not score relevance. A file hit does not establish sufficient answer evidence or complete recall. One Hit change is five percentage points on this suite; these public development questions support regression diagnosis rather than broad superiority claims. See the [metric validity review](./zg-retrieval-metric-review.md).

## Artifacts and compatibility

- `retrieval-results`: the single `summary.md` and `summary.json`.
- `retrieval-zg-report`: `report.json`, `report.md` and per-call `scores.jsonl`.
- `retrieval-data-<owner>__<repo>`: raw requests/responses, installation evidence, corpus/model inventories and public index status.

The overview uses schema 5 and fixed `zg-hybrid`, `zg-fts`, `zg-vector` rows. It identifies the frozen harness commit and selected candidate ref/commit and lists failed tasks and their reasons. ZG reports use schema 7 with `preview: mcp-default`, three mode aggregates and 60 quality rows for a complete suite. `quality_mean` contains the five-call quality means and ranking stability; the representative fifth-call `file_retrieval` and `ndcg` retain detailed target evidence. Each quality row retains all five public result lists and measurement observations for recomputation.

The protocol ID is `sweqa20-zg-rust-three-modes-mcp-default-v7`. Compare only schema 7 reports with matching protocol and frozen inputs. Reports from other protocol versions require their matching scorer checkout; saved-evidence replay must not be presented as a new product run.
