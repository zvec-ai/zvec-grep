# Retrieval-only pilot suites

These pilots use the Rust public MCP search route. They run independently of
the frozen SWE-QA20 suite and report File metrics and diagnostic Entity metrics.

`config.mjs` is the editable registry for pilot suite metadata: lock filename,
expected query count, embedding model, gold projection, report labels, optional
breakdown, and index timeout. Quarry's indexed language extensions live there
too. Original queries, relevance labels, corpus revisions, and source hashes
remain in the pinned `data/*.json` files. Adding a pilot still requires a
corpus preparation adapter and its own CI job; the registry keeps the shared
runner and combined report free of repeated suite-specific constants.

- **BEIR test:** the original ten SciFact claims, plus four NFCorpus questions,
  three ArguAna arguments and three FiQA questions. Each dataset uses its full
  corpus and original test qrels. Source revisions, Parquet hashes, qrels hashes
  and full-corpus content digests are pinned in `data/beir20.json`. SciFact
  keeps its original ten-query selection and document format. The other ten
  queries are recorded verbatim; positive NFCorpus grades remain in the lock,
  while the existing binary file metrics treat any positive grade as relevant.
  For ArguAna, each query's own document is removed before indexing, following
  the source dataset's self-match exclusion rule.
- **Quarry preimage:** the original ten quic-go queries plus one original query
  from each of ten additional repositories: ipython, litestar, wasmer, boa,
  webpack, Vue, Apache POI, StyleCopAnalyzers, s2n-tls and OPA. The twenty
  tasks span Go, Python, Rust, JavaScript, TypeScript, Java, C# and C. Each
  exact preimage revision is indexed separately. Original `positive_units`
  remain in `data/quarry20.json`; file metrics project their unique paths.
  The pilot uses ZG's containment rule for the function targets.
- **DuRetrieval / Chinese web search dev:** ten original Chinese queries sampled
  at equal positions in the sorted dev-query IDs. All 100,001 passages in the
  pinned [C-MTEB DuRetrieval](https://huggingface.co/datasets/C-MTEB/DuRetrieval)
  corpus subset are indexed, one unchanged passage per Markdown file. The
  published [dev qrels](https://huggingface.co/datasets/C-MTEB/DuRetrieval-qrels)
  determine relevant passage IDs. Source revisions and Parquet SHA-256 hashes
  are frozen in `data/duretrieval10.json`; preparation verifies the original
  queries, qrels and target IDs. This is a ten-query pilot on the C-MTEB corpus
  subset, not a score on DuReader's original full 8.09-million-passage corpus.

Each suite uses its own specified embedding model. The MCP requests use the
same fixed mode order (hybrid, fts, vector), explicit `preview: "full"`,
`limit: 10`, no agent, and five calls per query; all five results supply quality and stability, while the fifth
supplies output size. Results
include Hit@1/5/10, Recall@1/5/10, MRR@10 and nDCG@10 at both levels.
Reports also include ordered Top-10 stability, mean public output, and mean
and median call latency. Each query/mode
first averages its five call-level quality scores. Native result ranks are retained and repeat chunks
consume ranks. BEIR and Quarry now have twenty queries each, with the original
ten retained. The per-dataset and per-language rows show completed/planned
coverage and the same metrics as the overall pilot row; they do not change the
existing per-query average. SWE-QA20 uses a repository macro average for its
overall nDCG, while these pilots use a query average. Absolute scores across
suites do not share a corpus or relevance definition.

Indexed Entity and Fragment totals sum the independently built corpus indexes:
six for BEIR, one for DuRetrieval and twenty for Quarry. Shared content across
separate indexes is counted in each index; modes and repeated calls do not
multiply counts. The JSON retains each index's counts. Missing counts remain
N/A, including Fragment counts from older candidates that did not expose them.
Counts come from explicit `zg --status --debug` calls before and after each
index's retrieval batch, outside search timing. Each call reads the counts once;
ordinary status, MCP status, and search freshness checks do not scan Fragments.

Full preview displays each retrieved Entity's complete content; short preview
displays excerpts. Preserve earlier default-short artifacts and use their
matching scorer for review.

All four suites use the same public response parser. It tolerates one empty
trailing source line immediately after a result's public range, matching the
Rust MCP presentation of files ending in a newline. Nonempty or further
out-of-range source lines are still rejected.

After one shared Rust package build, SWE-QA20, BEIR, DuRetrieval and Quarry run
in four parallel suite jobs. BEIR and DuRetrieval use
`local/potion-multilingual-128m`; the two code suites use
`local/potion-code-16m-v2`. These configured models are the default `local`
workflow choice. The optional `remote` choice overrides all four suites with
`qwen/qwen3.7-text-embedding` without changing the pinned dataset files. Each
job publishes its own aggregate metrics and
per-question results, including failed questions. The final `Retrieval results`
job publishes one page with all four suite sections, even if a suite fails.

BEIR and DuRetrieval's Parquet readers are pinned in separate requirements
files and installed only in their CI jobs. A local run needs the corresponding
dependency before invoking `expansion/run.mjs`. Downloaded source data and
materialized corpus files are run-local inputs; only hashes and selected
original query/qrel records are checked in.

## Gold and scoring

| Suite | File Gold | Entity Gold |
| --- | --- | --- |
| BEIR | Original positive document qrels | Original SciFact evidence sentences where available, reviewed evidence spans for NFCorpus and FiQA, and the paired ArguAna argument body |
| DuRetrieval | Original positive passage qrels | Reviewed answer spans within the published passages |
| Quarry | Unique paths of original positive units | Original function ranges on the pinned preimage, with indentation and separators outside the function excluded |

[Gold manifests](gold/) record each original label, its source file checksum, the
reviewed byte range and its checksum, or the reason a range could not be
verified. The source locks bind the query, original labels and corpus version.
BEIR writes a title and a blank line before the body. DuRetrieval writes the
unchanged passage. The runner checks these materialized bytes before indexing.
Gold and reports stay outside the searchable corpus.

File Recall uses all original positive paths with the same ranking and
aggregation rules as the existing File scores. Entity metrics are diagnostic.
The scored targets cover the verified evidence within the published positives.
A separate completeness review against the full corpus is still pending. Some
Quarry questions ask for declarations or parts of a code path beyond the
published functions. The Entity scores measure coverage of those published
functions.

BEIR has 22 verified Entity targets. The cited abstracts for SciFact claims
`1`, `268`, `552`, `702`, `830` and `975` do not establish the stated
claim-specific evidence. These cases still count for File and are unscored at
Entity level. DuRetrieval has 48 verified answer ranges. All ten questions have
Entity Gold. Quarry keeps all 107 published positive functions.

SciFact preserves every original evidence group: claim `436` accepts sentence
`1` or `2`; `1274` accepts both `4` and `5`, or `7`. Claim `137` requires `7`
and `9`, without requiring sentence `8`. These are zero-based source sentence
indices, following the [official evidence rules](https://github.com/allenai/scifact/blob/master/doc/evaluation.md).
Each document is one target. Complete any one group within the first K results;
each member span must be fully contained in a returned Entity. The target's
first-hit rank is the position completing its earliest group, and multiple
complete alternatives still earn one target's credit.

Each Gold span must be fully contained in one result from the same file.
The runner aligns full MCP results to the verified source because the
text header gives line numbers without columns. This distinguishes different
chunks on the same line. Scores use the resulting byte coordinates. An
ambiguous range remains unknown when it could affect a metric. Repeating a
target does not add credit. Partial pieces of one span cannot be combined
across results; separate spans in an evidence group can. See the
[matching rules](../../docs/benchmark-gold-authoring-and-matching.md) for
ranking, aggregation and missing evidence.

Pilot report schema 3 binds the Gold and scoring rules and saves per-call
source coordinates. Comparisons recheck both sides with the same Gold and rules.
BEIR Gold schema 2 stores alternative groups and individual sentence spans.
Pilot scoring version 2 recognizes complete groups; recalculate both versions
under these rules rather than comparing them with earlier scores.
Historical schema 2 reports retain their File metrics. To add Entity metrics to
an older run, use its saved full responses and pinned source to verify the
ranges, or rerun both versions. Record which method produced the scores.
