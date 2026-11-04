# ZG Retrieval-only benchmark

The workflow measures **zg-hybrid, zg-fts and zg-vector** on four independent suites. The original SWE-QA20 suite uses 20 unchanged questions and 11 pinned repositories from [Actions run 35206585943](https://github.com/Cuiyus/zvec-grep/actions/runs/35206585943). The exploratory suites contain 20 original BEIR queries across four datasets, ten DuRetrieval queries and 20 original Quarry queries across eight languages; see the [pilot design](expansion/README.md). Each mode calls the Rust public MCP search endpoint directly with its default response presentation. No answering agent, query rewriting, subquery generation or LLM judge participates. See the [SWE-QA20 design](./zg-retrieval-only-sweqa20-design.md) for its frozen protocol and scoring details.

## Run CI and read the result

The [Retrieval-only workflow](../../.github/workflows/retrieval-only.yml) supports manual runs and reuse by the [PR version comparison](../../.github/workflows/retrieval-comparison.yml). For a manual single-version run, select **Retrieval-only → Run workflow**. Use GitHub's workflow-ref selector to choose the benchmark harness (`main` for the latest merged harness), then set `candidate_ref` to the branch, tag or commit containing the `rust/` workspace to compile. It defaults to `main`. The `embedding` choice defaults to `local`, which preserves each suite's configured local model. Select `remote` to run every suite with `qwen/qwen3.7-text-embedding`; this requires the repository secret `QWEN_EMBEDDING_API_KEY` and variable `QWEN_EMBEDDING_ENDPOINT`. Every run includes all three modes.

PR creation, reopening and new commits automatically compare the frozen PR base and head through all four suites. An authorized `@zg-bench` PR comment requests the same comparison and posts its table. Both versions use the same harness and local models, with separate fresh indexes; neither receives remote embedding credentials. e2e is skipped by default. Each metric cell is base / head / delta (head minus base), on a 0–1 scale. Missing or invalid results make the comparison incomplete rather than zero. The default-branch [result publisher](../../.github/workflows/retrieval-pr-comment.yml) creates or updates one English PR comment with the table and run link, including for fork PRs. It skips cancelled or outdated runs, keeps benchmark execution read-only, and becomes active after its workflow is merged into the default branch. The `retrieval-comparison` artifact and Actions summary also retain the result; `retrieval-base-*` and `retrieval-head-*` retain the underlying reports. See [benchmark setup](../README.md).

For a manual paired comparison without a PR comment, select **Retrieval-only PR → Run workflow** and supply full `baseline_sha` / `candidate_sha` commit identities and their `baseline_repository` / `candidate_repository` (`owner/name`). This uses the same local-only comparison and does not run e2e. Manual runs require maintain/admin access.

The workflow freezes the selected harness ref to a full commit, checks out the candidate separately, resolves it to another full commit SHA, and builds `candidate/rust/`. The packed native package cache is keyed by operating system, architecture and that exact candidate commit. A hit skips Rust compilation and packaging. On a miss, a second Cargo cache can reuse registry data and `rust/target` objects before `npm run pack:local` creates the candidate tarball. Node.js version is not part of candidate selection, package-cache identity or report identity.

Manual and comment runs require the original actor and current re-run actor to have the repository **maintain or admin** role; execution jobs check both, including partial re-runs. Automatic PR runs instead require the supported PR event and local embedding, with read-only permissions and no model secrets. These in-workflow checks apply to the checked-in workflow. Contributors who can modify the workflow or its local authorization action on another branch can bypass that check; a permission boundary against those contributors requires repository- or organization-level Actions execution policies.

The **Retrieval results** job publishes one page with a suite overview and a separate three-mode table for SWE-QA20, BEIR, DuRetrieval and Quarry. All four suite jobs also publish their own aggregate and per-question tables as soon as each job finishes. In the SWE-QA20 section, the three rows are:

| Arm | Public MCP search input | Presentation |
| --- | --- | --- |
| zg-hybrid | `query: originalQuestion` | Rust MCP default |
| zg-fts | `fts: [originalQuestion]` | Rust MCP default |
| zg-vector | `vector: [originalQuestion]` | Rust MCP default |

Every request also uses `limit: 10`, `autoUpdate: false`, `freshness: eventual` and `preferSymbol: false`. FTS and vector requests omit `query` and the other route. The benchmark leaves `preview` unset to measure the default presentation. The product retains its native chunking, aggregation, ranking and bounded response presentation.

The public response parser requires the MCP `freshness:` header and accepts both the legacy MCP body and the CLI compact body. Compact output does not expose per-hit route labels, so unavailable routes remain `null`; the parser never invents route evidence from the requested mode. Route validation still rejects any reported label that conflicts with the requested mode. Global CLI freshness and indexing diagnostics remain visible evidence before the compact body; legacy trailing diagnostics are also supported.

| Column | Meaning / aggregation |
| --- | --- |
| File Hit@1 | Mean of each question/mode's five binary rank-1 observations |
| File Hit@5 | Mean of each question/mode's five binary Top-5 observations |
| File Hit@10 | Mean of each question/mode's five binary Top-10 observations |
| File MRR@10 | Mean of five `1 / first matching native rank` observations per question/mode; Top-10 misses are zero |
| nDCG@10 | Mean of five binary discounted-gain observations per question/mode, then repository macro average across 11 repositories |
| Stable Top 10 | Count of question/mode cases whose five ordered public result locations match exactly |
| Mean output (KiB) | Mean public MCP text UTF-8 bytes / 1024, using successful fifth calls only; not model tokens |
| Avg RT / P50 RT (ms) | Mean and median of all successful MCP search calls, including five repetitions; excludes indexing |

Each repository uses one fresh index and one MCP session. Modes run in the fixed order **hybrid → fts → vector**; each original question is called five consecutive times within each mode. All five calls supply quality and stability observations; only the fifth supplies output size. A complete run contains **300 calls and 60 question/mode quality summaries**, with **20 questions, 20 output samples and 100 latency samples per mode** when all calls succeed. Repetitions are not independent questions.

Isolated public-response format failures still fail CI, but the overview displays diagnostic scores from validated questions with explicit question/repository coverage and a failed-task table. Invalid evidence is excluded, never counted as a miss or a zero. Protocol, identity, missing-call and other integrity errors still withhold all aggregates. Product failures with valid evidence retain zero quality credit and fail operational integrity; failed calls are excluded from output and latency measurements. Quality scores have no arbitrary pass threshold. Fixed mode order and shared runtime/model caches mean latency is an observation under this protocol, not a controlled comparison of cold-start or mode execution speed.

## Dataset and scoring

Frozen inputs are `data/source.lock.json`, `data/queries.jsonl`, `configs/protocol.json`, and `gold/files-v1.json`. The questions and 39 relevant-file targets are unchanged. Targets are unique accepted paths projected from AI-reviewed source annotations; bridge-only paths do not earn credit. They are partial positives, not independent human ground truth or complete answer evidence. The [metric rationale](./zg-retrieval-metric-review.md) explains how to interpret the scores and their limits.

All five quality metrics use the same file labels and normalized-path matching. Native ranks are preserved: repeated file chunks consume positions and are never collapsed or renumbered. Source declarations, outlines and response length cannot alter these quality scores. File Hit/RR/MRR use contract `sweqa-file-hit-rr-v1`.

`metrics/ndcg.mjs` uses first-target rank, binary gain and target-count IDCG. It is checked against a byte-identical pinned Python scoring reference, with its MIT attribution preserved in [`test/fixtures/ndcg-reference/`](test/fixtures/ndcg-reference/). The reference is used only by unit tests. The benchmark runs only ZG.

Questions, labels and reports remain outside indexed source checkouts. Indexing applies the frozen code-extension and size policy while retaining ZG's native ignore rules. Repository indexes are never cached; model downloads and compiled candidate packages may be cached. Corpus and model inventories plus the Rust CLI's public aggregate index status are checked before and after retrieval. Model identity includes artifact contents but excludes only the runtime-generated `.zvec-grep-artifacts-<24hex>.complete` cache marker, whose machine-specific timestamps do not identify model weights.

Index audits prefer the structured `Status diagnostics` snapshot on stderr from
`zg --status --check-ready --debug`. It preserves exact source bytes, entity
counts and readiness independently of the compact human layout. Historical
candidates without that snapshot retain the legacy status parser; missing audit
fields are never inferred from a coverage bar. Raw stdout and stderr are retained.

Rust candidates are built through the [shared package Action](../../.github/actions/rust-candidate-package/action.yml); its [manifest utility](../shared/rust-package-cache.mjs) binds each tarball to the exact source commit and package bytes. SWE-QA uses the same packaging path while retaining its own evaluation protocol.

## Reports and artifacts

- `retrieval-results`: the unified `summary.md` and machine-readable `summary.json`.
- `retrieval-zg-report`: the SWE-QA20 suite conclusion (`summary.json` and `summary.md`), validated `report.json`, `report.md` and per-call `scores.jsonl`.
- `retrieval-zg-evidence`: raw public requests/responses, installation evidence, corpus/model inventories and public index status for all 11 repositories.
- `retrieval-beir-report`, `retrieval-duretrieval-report` and `retrieval-quarry-report`: independent pilot summaries with BEIR dataset and Quarry language coverage and scores.
- `retrieval-beir-evidence`, `retrieval-duretrieval-evidence` and `retrieval-quarry-evidence`: pilot public requests/responses and index status.

Evidence retention is 14 days. After a shared candidate build, SWE-QA20, BEIR, DuRetrieval and Quarry run as four independent suite jobs. Each suite publishes its own aggregate metrics and a per-question table even if an individual question fails. The final results job publishes the combined page. Missing artifacts and failed upstream jobs remain explicit in the overview.

SWE-QA20 ZG reports use **schema 7**, with `preview: "mcp-default"`, three `modes`, and one quality row per question/mode. Its section uses the **schema 5** overview with fixed `zg-hybrid`, `zg-fts`, `zg-vector` rows; the combined page uses schema 1. The SWE-QA20 overview records coverage, failed task IDs/modes/reasons, the frozen harness commit and selected candidate ref/commit. Quality rows retain all five public result lists and measurement observations so validators can recompute the case mean, ranking stability, output size and latency. The representative fifth-call `file_retrieval` and `ndcg` fields remain for evidence; `quality_mean` supplies headline quality.

The protocol ID is `sweqa20-zg-rust-three-modes-mcp-default-v7`. The comparator accepts schema 7 reports with matching protocol and frozen inputs, Rust MCP default presentation and all three modes. Reports from other protocol versions require their matching scorer checkout. Replaying saved evidence is not a new retrieval run.

## Code structure

```text
zg-retrieval/
  *.mjs              Stable command-line entrypoints
  core/              Frozen suite, corpus, I/O and shared response scoring
  metrics/           File ranking, nDCG and operational measurements
  engines/zg/        Runner, public response parser, snapshots and evidence audit
  reports/           Validation, same-protocol comparisons and CI rendering
  configs/           Frozen protocol
  data/              Frozen questions and repository/source lock
  gold/              Frozen relevance labels and annotation provenance
  test/              Contract tests and pinned scoring reference
```

The runner captures public evidence; the aggregator reparses and audits it; metric functions score normalized results; report validators recompute quality and measurements before comparison or CI rendering. Metrics do not depend on engine implementations. Response presentation behavior remains covered by the product's MCP contract tests, rather than duplicate retrieval benchmark arms.

## Run locally

Use Node.js, Python 3, npm, Git, Rust and a platform supported by the packed product. Python is used only for the stdlib scoring oracle. The first retrieval run needs network access for repository checkouts, Rust dependencies and the embedding model. Run from the repository root:

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
  --candidate-commit "$(git rev-parse HEAD)"

node benchmarks/zg-retrieval/report.mjs "$retrieval_work/results"
```

Local commands use the configured local models by default. To reproduce the
remote workflow choice, set `RETRIEVAL_EMBEDDING=remote` together with
`ZVEC_GREP_API_KEY` and an HTTPS `ZVEC_GREP_ENDPOINT` before running the suite
and report commands.

`--package` accepts a tarball or a directory containing exactly one `.tgz`. The runner installs it in an isolated consumer. Use a new `--output` directory and fresh corpus checkout: output is never overwritten and an existing index is rejected. An external model-download cache may be reused.

Add `--repository reflex-dev/reflex` or `--tasks reflex:6` for an explicitly labeled subset smoke run; all three modes still execute. Standalone `report.mjs` requires all 20 questions, and CI rejects subset reports. Modes and MCP presentation are fixed by the protocol, without selection flags.

## Offline replay and comparisons

Download `retrieval-zg-evidence` into one directory, then recompute:

```sh
node benchmarks/zg-retrieval/report.mjs /absolute/path/to/downloaded-shards
node benchmarks/zg-retrieval/compare.mjs baseline/report.json candidate/report.json NEW_OUTPUT_DIR
node benchmarks/zg-retrieval/ci-report.mjs \
  --zg /absolute/path/to/report.json \
  --output /absolute/path/to/overview
```

An isolated task/mode failure can produce a diagnostic partial overview while the command and CI still fail. Incompatible or globally invalid evidence produces no scores. Candidate source selection happens through `candidate_ref`; the harness comes from the workflow ref selected for that run.

## Corpus size and remote embedding load

Query counts are small, but every run builds fresh corpus indexes before searching:

| Suite | Indexed corpus |
| --- | --- |
| SWE-QA20 | Source files from 11 pinned repositories |
| BEIR | 75,128 unique documents across SciFact (5,183), NFCorpus (3,633), ArguAna (8,674) and FiQA (57,638) |
| DuRetrieval | 100,001 passages |
| Quarry | 20 pinned repository preimages, filtered by each task's language |

ArguAna needs a separate leave-one-out index for each of its three selected queries, so BEIR processes 92,473 document files across its six indexes. Source files can yield multiple embedding entities. Model downloads and compiled packages may be cached; corpus indexes and their document embeddings are not reused between runs or versions. Each of the 70 queries is also searched in three modes with five calls per mode.

Manual remote runs start the four suite jobs independently. The Qwen text backend's default indexing policy starts at eight concurrent batches and can grow to twelve; Qwen3.7 text embedding accepts up to twenty inputs per batch. Separate runs for different candidates can overlap, with no account-wide token budget in this workflow. Large fresh indexes and overlapping batches can therefore exhaust a shared remote token rate limit even when there are few test queries. PR and comment version comparisons deliberately use local embedding.

For Model Studio, `Allocated quota exceeded` indicates a token rate limit; [the provider documents TPM/TPS limits and account-wide aggregation](https://www.alibabacloud.com/help/en/model-studio/rate-limit). Remote capacity planning needs token-aware pacing across suite jobs and runs, in addition to limiting per-process concurrency. Reducing test query counts alone does not remove the indexing load.

## Limits

These public development questions support regression diagnosis, not broad generalization claims. A file hit does not establish that its returned snippet answers the question. Reports do not claim complete candidate/fusion histories or embedding inputs; a miss alone cannot identify the responsible stage. Answer correctness and agent token use require separate evaluations.
