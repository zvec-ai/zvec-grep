# Retrieval-only pilot suites

These pilots reuse the Rust public MCP search route and the existing five
file-level metrics. They run independently of the frozen SWE-QA20 suite.

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
  This is a file-level pilot, not Quarry's official function-level recall.
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
include File Hit@1/5/10, File MRR@10, binary file nDCG@10, ordered Top-10
stability, mean public output, and mean and median call latency. Each query/mode
first averages its five call-level quality scores. Native result ranks are retained and repeat chunks
consume ranks. BEIR and Quarry now have twenty queries each, with the original
ten retained. The per-dataset and per-language rows show completed/planned
coverage and the same metrics as the overall pilot row; they do not change the
existing per-query average. SWE-QA20 uses a repository macro average for its
overall nDCG, while these pilots use a query average. Absolute scores across
suites do not share a corpus or relevance definition.

Full preview displays each retrieved Entity's complete content; short preview
displays excerpts. Gold and file-scoring rules remain unchanged. Preserve earlier
default-short artifacts and use their matching scorer for review; they are not
full-preview runs.

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
