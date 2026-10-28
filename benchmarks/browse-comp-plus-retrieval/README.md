# BrowseComp-Plus Retrieval-only CI

Compare the unchanged official **Node.js 0.2.2** npm release with a commit-pinned **Rust** build, using ten fixed original queries against the complete **100,195-document** corpus. Both products run sequentially on one Ubuntu runner, with separate fresh indexes and a shared model download cache. Each mode executes five calls per query: **150 calls per runtime**, 300 per comparison.

The workflow uses public MCP search in **hybrid → fts → vector** order. It does not run an answering agent or an LLM judge. Index embedding batches and public queries have a concurrency limit of **4**. Storage commits retain each product's own scheduling.

## Run and view results

After merging, select **BrowseComp-Plus Retrieval-only → Run workflow** and provide the Rust `candidate_ref` (default `main`). The build job freezes it to a full commit SHA. The benchmark harness is frozen independently, and both npm packages are verified before installation.

For testing a PR branch before this new workflow is registered on the default branch, use the existing **Retrieval-only PR** dispatch entry point:

```sh
gh workflow run retrieval-pr.yml --repo zvec-ai/zvec-grep \
  --ref dev/browse-comp-plus-retrieval-ci \
  -f suite=browsecomp-plus \
  -f baseline_sha=npm:@zvec/zvec-grep@0.2.2 \
  -f baseline_repository=zvec-ai/zvec-grep \
  -f candidate_sha=FULL_UPSTREAM_MAIN_SHA \
  -f candidate_repository=zvec-ai/zvec-grep
```

Ordinary PR events keep the existing four-suite Retrieval-only workflow. The BrowseComp job runs only when explicitly selected by a manual caller; manual runs and partial re-runs require maintain/admin access. All jobs have read-only GitHub permissions. The public dataset and local model require no model API credentials.

The execution job publishes the complete comparison to its **Summary**, including relative percentages `(Rust − Node) / Node × 100%`. A negative value means lower. A zero denominator or missing measurement is unavailable, never an invented zero.

| Artifact | Contents | Retention |
| --- | --- | --- |
| `browsecomp-plus-retrieval-report` | `summary.md` and `summary.json`; quality, performance, versions and coverage | 90 days |
| `browsecomp-plus-retrieval-evidence` | Raw public requests/responses and SHA-256 identities, per-call RT, RSS samples, index command/status evidence, corpus/model manifests and failures | 30 days |
| `browsecomp-plus-retrieval-packages` | Original Node tarball and frozen Rust tarball, with verified identity manifests | 14 days |

Missing source data, partial indexing, product failures, changed corpus/model identities, malformed responses, incomplete calls or unavailable RSS fail the measured run. Partial evidence is retained. Retrieval quality is report-only, without an arbitrary score threshold.

## Collected measurements

| Stage | Measurement | Output |
| --- | --- | --- |
| Index | Complete build command time | Total, mean, p95, p99 |
| Query | Individual public MCP RT | Mean, p50, p95, p99; 50 samples per mode/runtime |
| Query | Complete corpus/mode phase time | Total, mean, p95, p99 |
| Index / Query | Product process-tree and resident daemon RSS | Mean, p95, p99, sampled peak; raw samples and coverage |
| Index | Generated `.zvec-grep` regular-file size | Exact bytes and MiB; total, mean, p95, p99 |
| Index / Query | Concurrency policy | Embedding batch limit 4; query in-flight limit 4 |

There is **one full-corpus index and one phase per mode/runtime**. Phase-time and index-size mean/p95/p99 therefore each describe one observation; they do not estimate run-to-run variance. Query percentiles use raw per-call samples; RSS percentiles pool raw time samples. p50 is the median; p95/p99 use nearest rank.

Models are downloaded/warmed separately before measurement. Index time includes CLI startup, model loading and final optimization. Query RT stops before parsing/scoring/capture; phase totals include capture. Fixed mode order and shared caches retain first-call/model/session effects. Sequential runs on one runner reduce host differences but do not establish a controlled causal speed comparison.

RSS sampling targets 100 ms, excludes the driver and may double-count shared pages or miss brief spikes. Model cache and symlinks are excluded from index size. Neither product is modified, and per-file write RT is excluded because Node 0.2.2 does not expose it. Indexes are never reused for measured builds; the first index is removed after the server stops and its measurements are saved, before the second build.

## Frozen inputs and scoring

Input selection and normalization come from the [BrowseComp+ integration at `c1299bf2`](https://github.com/zvec-ai/zvec-grep/tree/c1299bf23ef4d06b3640880a1f0beed8cca092f8/benchmarks/browse-comp-plus). This workflow is independent of that integration's agent execution/recovery pipeline.

- Queries: `Tevatron/browsecomp-plus@144cff8e35b5eaef7e526346aa60774a9deb941f`, 830 original records.
- Corpus: `Tevatron/browsecomp-plus-corpus@b27b02bc3e45511b8b82a13e6f90ce761df726f6`, 100,195 documents.
- CI IDs: **628, 700, 966, 827, 241, 165, 1068, 244, 843, 563**; the original first ten SHA-256 ranks with seed `browsecomp-plus-ci-v1`. No outcome-based exclusions or replacements.
- `source.lock.json` pins all 13 Parquet file hashes and the complete decrypted query digest; `suites/ci-selection.json` preserves the reference gold/evidence audit. Original questions are not rewritten or checked into this repository as plaintext.
- Each document is exactly its UTF-8 source `text` in `<docid>.md`, with no injected title or URL header. The 64 MiB size policy matches the referenced integration, and complete public index counts are required.
- Embedding: **`local/potion-multilingual-128m` on CPU**, with matching model artifact content hashes for both products.

The five quality metrics are document Hit@1/5/10, MRR@10 and binary nDCG@10 using original `gold_docs`, averaged over five calls then equally over the ten original queries. Native top-ten chunk ranks are preserved without document deduplication. These measure document localization, not official BrowseComp answer accuracy or multi-hop evidence sufficiency; repetitions are not additional independent questions. The full 830-question evaluation is not run.

## Local verification

```sh
python -m pip install --requirement benchmarks/browse-comp-plus-retrieval/requirements.txt
python -m unittest discover -s benchmarks/browse-comp-plus-retrieval/test -p 'test_*.py' -v
node --test benchmarks/zg-retrieval/test/*.test.mjs benchmarks/browse-comp-plus-retrieval/test/*.test.mjs
```

Preparation downloads and verifies the full corpus once. Questions, gold labels and evidence stay outside the indexed Markdown root. The retained comparison includes exact package, harness, source, corpus and model identities, and the report audit reparses saved public responses and recomputes scores and statistics.
