# ZG Retrieval-only benchmark

Compare FTS, Vector and Hybrid search on five frozen suites. Each request uses the original question through the Rust public MCP search interface; there is no answering agent, query rewriting or LLM judge.

## Run

| Entry point | What to select |
| --- | --- |
| PR comparison | Opening, reopening or updating a PR compares its pinned base and head automatically. An authorized `@zg-bench` comment also requests a comparison. |
| [Manual single-version run](../../.github/workflows/retrieval-only.yml) | **Retrieval-only → Run workflow**. The workflow ref selects the harness; `candidate_ref` selects the Rust candidate and defaults to `main`. Enable `gold_stages` for traces. |
| [Manual paired comparison](../../.github/workflows/retrieval-pr.yml) | **Retrieval-only PR → Run workflow**. Supply full `baseline_sha` / `candidate_sha` and `baseline_repository` / `candidate_repository` (`owner/name`). This does not post a PR comment. |

All runs include all three modes. Paired runs use the same harness and configured local models, build separate fresh indexes, and enable stage capture on both versions. They do not run answering-agent E2E tests or receive remote model credentials.

Manual and comment runs require maintain/admin access for both the original actor and the re-run actor. See [benchmark setup](../README.md) for repository configuration. The [PR result publisher](../../.github/workflows/retrieval-pr-comment.yml) must be merged into the default branch before it can update comments; it ignores cancelled or outdated runs.

## Read the results

The PR comment shows File/Entity quality tables and a **download link to `retrieval-trace.html`**. Download the HTML and open it locally: it is a single file, not a ZIP, and needs no server or external resources. It contains suite summaries, original questions, per-question FTS/Vector/Hybrid Recall, Gold IDs and stage ranks.

**Artifacts expire after 14 days.** Save the reports and raw evidence if you need a longer-lived comparison.

| Artifact | Contents |
| --- | --- |
| `retrieval-trace.html` | Interactive stage trace for paired runs; select suite, question, baseline/candidate and repetition. |
| `retrieval-comparison` | Paired `summary.md` and `summary.json`; exact scores and deltas remain in JSON. |
| `retrieval-results` | Single-version combined summary across all five suites. |
| `retrieval-{zg,beir,duretrieval,quarry,aacr}-report` | Suite scores and per-question results. SWE-QA20 also retains `report.json`, `report.md` and `scores.jsonl`. |
| `retrieval-{zg,beir,duretrieval,quarry,aacr}-evidence` | Public requests/responses, corpus/model inventories and index status. |

Paired runs prefix underlying artifacts with `retrieval-base-` or `retrieval-head-`. Combined report bundles retain stage sidecars as `reports/{suite}-gold-stages.json`.

Equal comparison values appear once; changes show **baseline → candidate**. **N/A** means unavailable or unscored, never zero; **—** means a route did not run. Missing artifacts and failed questions remain visible. Invalid evidence fails CI even when a diagnostic partial table can be shown; quality scores have no arbitrary pass threshold.

## Protocol and scores

| Suite | Original queries | Indexed corpus |
| --- | ---: | --- |
| SWE-QA20 | 20 | 11 pinned repositories |
| BEIR | 20 | Four datasets; 75,128 unique documents |
| DuRetrieval | 10 | 100,001 passages |
| Quarry | 20 | 20 pinned preimages across eight languages |
| AACR traces | 6 | Three pinned PR heads; five semantic diagnostics and one separately reported lookup control |

| Mode | Public search input |
| --- | --- |
| Hybrid | `query: originalQuestion` |
| FTS | `fts: [originalQuestion]` |
| Vector | `vector: [originalQuestion]` |

Requests use `preview: "full"`, `limit: 10`, `autoUpdate: false`, `freshness: eventual` and `preferSymbol: false`. Modes run **hybrid → fts → vector**, with **five calls per question/mode**. FTS and Vector omit `query` and the other route. Preview affects presentation, not native ranking or Entity matching.

| Report value | Counting rule |
| --- | --- |
| File/Entity Hit@1/5/10, Recall@1/5/10, MRR@10, nDCG@10 | Average five calls within each question, then give scored questions equal weight. PR SWE-QA20 rows use QA diagnostic labels; legacy File scores stay separate. |
| Stable Top 10 | The five ordered public result locations match exactly. |
| Mean output (KiB) | Public MCP text UTF-8 bytes / 1024, using successful fifth calls only. |
| Avg / P50 RT (ms) | All successful search calls, excluding indexing. Fixed mode order and shared caches prevent interpreting this as a controlled speed comparison. |
| Indexed Entities / Fragments | Sum each independently built index once, without multiplying by modes or repetitions. Missing counts remain N/A. |

Repeated chunks keep their native ranks. Entity matches require full containment of a Gold target in the same file; partial overlap is a miss. Unknown matches, missing annotations and parsing failures do not become zero scores. Gold and reports stay outside indexed sources. These development datasets diagnose retrieval regressions; their recorded positives do not establish exhaustive answer coverage.

Frozen SWE-QA20 inputs are [the source lock](data/source.lock.json), [queries](data/queries.jsonl), [protocol](configs/protocol.json) and [Gold](gold/). Protocol `sweqa20-zg-rust-three-modes-mcp-full-v8` uses report schema 7. QA containment scoring is version 2. Compare matching protocols and Gold versions; legacy SWE-QA20 File nDCG retains its repository macro average.

See [SWE-QA20 design](zg-retrieval-only-sweqa20-design.md), [pilot data and scoring](expansion/README.md), [QA labels](gold/qa-pairs.md) and [matching / nDCG reference](../docs/retrieval-only-qa-reference.md) for full definitions.

AACR uses [six enabled code-review queries](expansion/AACR.md). Its source-review gates apply to both File and Entity metrics; pending labels are N/A while retrieval completion, ranking stability and cost remain available.

## Gold coverage by retrieval stage

PR comparisons enable capture automatically. For a single-version run, enable `gold_stages`; locally, pass `--gold-stages` to the SWE-QA20 or expansion runner. **The engine must support stage capture. Older binaries, including a baseline without that support, show N/A.** Missing stage data does not invalidate otherwise valid retrieval scores.

| HTML row | Meaning |
| --- | --- |
| Gold covered by index | Gold has a searchable indexed Entity. |
| Gold recalled by FTS / Vector | Coverage in the route's complete recall pool, across adaptive attempts. |
| Before rule ranking (RRF) | Gold within Top K of the loaded candidate pool in RRF order. |
| After rule ranking (actual) | Gold within the actual Top K after static rules and same-file decay. |

Both ranking rows use the **same loaded pool**, with K = 10. Rules act on the full pool before selecting results. The HTML also shows candidate-pool sizes and per-call Gold ranks; complete intermediate Gold coverage remains in `gold-stages.json`.

Stage counts average each question's five observations, then sum across questions. Each question's Gold enters the denominator **once**: five identical observations of 45/88 display **45/88**, not 225/440. Varying observations may produce fractional counts; unknown coverage remains explicit. Formal, diagnostic and ineligible Gold are kept separate.

**Capture adds bookkeeping and metadata transfer to measured latency.** Compare normal search performance separately with capture disabled on both versions.

<details>
<summary>Capture transport and historical reports</summary>

- The runner starts its isolated daemon with `ZG_TRACE_STAGES=1` and `ZG_TRACE_PATHS` (repository-relative watched paths, without Gold labels or target coordinates). Setting only the client environment does not reconfigure an existing daemon.
- The engine looks up paths directly and returns captures in MCP `_meta.zg_stage_trace`; CLI capture uses `[ZG_STAGE]` records on stderr. ZG writes no diagnostic files. The benchmark retains `gold-stages.json` outside the searchable corpus.
- Stage summary schema 2 counts Gold once per question. Schema 1 accumulated target-call counts; rebuild from saved calls instead of dividing by an assumed repeat count. The old `static_top10` field kept static rules enabled and cannot be relabeled as the current RRF comparison.
- Keep historical artifacts and identify the scorer commit when rescoring. Earlier stage range handling could overcount a following line; the current scorer uses Rust's half-open coordinates. Pilots use verified UTF-8 byte ranges; SciFact requires a complete accepted evidence group. See the [stage scorer](metrics/gold-stages.mjs) and [matching reference](../docs/retrieval-only-qa-reference.md). Rescoring is not a new retrieval run.

</details>

## Run SWE-QA20 locally

Use Node.js, npm, Git, Rust and a platform supported by the packed product. Python 3 is needed for the offline scoring-reference tests. The first run downloads repositories, build dependencies and model files. From the repository root:

```sh
node --test benchmarks/zg-retrieval/test/*.test.mjs
retrieval_work="$(mktemp -d)"
mkdir -p "$retrieval_work/package"
(cd rust && npm ci && npm run pack:local)
cp rust/dist/npm/*.tgz "$retrieval_work/package/"

node benchmarks/zg-retrieval/run.mjs \
  --package "$retrieval_work/package" \
  --output "$retrieval_work/results" \
  --corpus "$retrieval_work/corpus" \
  --model-cache "$retrieval_work/model-cache" \
  --candidate-commit "$(git rev-parse HEAD)" \
  --gold-stages

node benchmarks/zg-retrieval/report.mjs "$retrieval_work/results"
```

`--package` accepts a tarball or a directory containing exactly one `.tgz`. Use a new output directory and fresh corpus; existing indexes are rejected. Model downloads and compiled packages may be cached, but corpus indexes are not reused. See [pilot local commands](expansion/README.md) for the other suites.

Add `--repository reflex-dev/reflex` or `--tasks reflex:6` for a subset smoke run. All three modes still execute; standalone `report.mjs` and CI require the complete 20-question suite.

<details>
<summary>Remote embedding</summary>

Manual CI runs can select `embedding: remote` to use `qwen/qwen3.7-text-embedding` for every suite. Configure repository secret `QWEN_EMBEDDING_API_KEY` and variable `QWEN_EMBEDDING_ENDPOINT`. Locally, set `RETRIEVAL_EMBEDDING=remote`, `ZVEC_GREP_API_KEY` and an HTTPS `ZVEC_GREP_ENDPOINT` before the runner and report commands.

Fresh corpus indexing dominates remote API load, even with few queries. BEIR builds six indexes (92,473 document files) because ArguAna excludes each selected query's own document; Quarry builds each preimage separately. Parallel suites and overlapping runs share provider quota. PR comparisons use local embedding.

</details>

## Replay saved evidence

Download `retrieval-zg-evidence`, then use the matching scorer checkout:

```sh
node benchmarks/zg-retrieval/report.mjs /absolute/path/to/downloaded-shards
node benchmarks/zg-retrieval/compare.mjs baseline/report.json candidate/report.json NEW_OUTPUT_DIR
node benchmarks/zg-retrieval/ci-report.mjs \
  --zg /absolute/path/to/report.json \
  --output /absolute/path/to/overview
```

These commands rescore saved SWE-QA20 responses; they do not rerun retrieval. Preserve earlier results when changing Gold or scoring rules, and rescore both versions under the same rules.

## Code map

| Directory | Responsibility |
| --- | --- |
| `core/`, `engines/zg/` | Frozen inputs, public requests/responses, runner and evidence audits |
| `metrics/` | File/Entity matching, ranking scores and stage coverage |
| `reports/` | Validation, comparisons and HTML rendering |
| `expansion/` | Pilot configuration, corpus preparation and runners |
| `configs/`, `data/`, `gold/` | Pinned protocol, source/query records and labels |
