# BrowseComp+ Retrieval-only

An independent comparison of **PR base Rust → PR head Rust**, following #303. Both versions query all **100,195 documents** using ten fixed original questions, three modes and five repetitions: **150 public MCP calls per version**. No answering agent or LLM judge runs.

## Trigger and isolation

After this workflow reaches the default branch, a maintainer/admin posts a standalone line on an open PR:

```text
@zg-retrieval-bench doc
```

Quoted/fenced commands, bot comments, extra arguments and ordinary issues are ignored. Original and rerun actors both need maintain/admin access. The request freezes base/head repositories and full SHAs plus the trusted harness SHA. One bot comment moves from queued to a validated comparison or an incomplete result.

**BrowseComp-Plus Retrieval-only** owns separate package, index, query and reply jobs. It does not enter the standard Retrieval-only suite matrix. PR changes run lightweight contracts only. A manual diagnostic entry accepts `pr_number`; both entries compare Rust with Rust.

**Comment events require this workflow on the default branch.** Posting the command before then cannot exercise the entry. See [GitHub event documentation](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#issue_comment).

## Reusable Index Artifacts

When present, the FP16 codec crate's production sources, manifest and `half` dependency also participate. FP32 keys without this optional crate remain unchanged, even though both formats use index version 2.

The key hashes persisted storage/workspace production code, the explicit Rust index version and storage-backend dependency identities, together with pinned corpus/model inputs and Linux x64 settings. PR SHA, query code, benchmark harness and storage tests do not invalidate it. Production storage changes conservatively invalidate it, including nonfunctional edits.

1. Package the frozen base/head sources and compute storage keys.
2. Deduplicate identical keys: compatible base/head share **one** sealed index.
3. Find `browsecomp-doc-index-<key>` across this independent workflow's runs and verify producer provenance. Ordinary hits only record its reference, avoiding a duplicate multi-gigabyte download before Query.
4. For a previously unseen storage identity, build one full index, retain original time/RSS/size evidence and seal it with model bytes.
5. Query downloads each selected Artifact once and verifies archive/member hashes, full public coverage and exact model/corpus identities before use. Restore a pristine copy for base, stop the product and remove it, then restore a pristine copy for head on the same host. Disable automatic updates and verify status before/after.

API/download errors, corruption and a registered index that expired or disappeared **fail explicitly**, without silently reindexing. Bot result comments retain full receipts so a deleted Artifact is not mistaken for a new key. Preserve these comments and producer workflow runs.

**Artifacts have a retention limit; they are not permanent NAS storage.** Index retention is 90 days. A requested run finding fewer than seven days remaining republishes the same verified archive bytes. It does not reindex. If no run renews it before expiry, restore externally archived bytes first. There is no scheduled keepalive or automatic deletion of older storage versions. See [GitHub Artifact retention](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/download-workflow-artifacts).

| Artifact | Contents | Retention |
| --- | --- | --- |
| `browsecomp-doc-index-<key>` | Full index, exact model bytes, file hashes, original Index report/evidence | 90 days; byte-copy renewal |
| `browsecomp-doc-index-ref-<key>` | Selected Artifact/run IDs and storage key | 30 days |
| `browsecomp-doc-comparison` | Validated `summary.json` and readable `summary.md` | 90 days |
| `browsecomp-doc-query-evidence` | All 300 requests/responses, hashes, RT, RSS, status and reports | 30 days |
| `browsecomp-doc-index-evidence-<key>` | Complete or partial fresh-build logs, progress, RSS and report | 30 days |
| `browsecomp-doc-package-{baseline,candidate}` | Commit-bound Rust package and storage identity | 14 days |

The read-only Parquet cache reduces download time. It never substitutes for an index Artifact. Each run recreates query workspaces and verifies corpus/model identities. Model bytes are restored at the same absolute cache path recorded by the builder; a mismatch fails before Query rather than using a separate download.

## Budget and measurements

A [real Linux scout](https://github.com/zvec-ai/zvec-grep/actions/runs/40687725826) measured Rust fresh indexes at 10.508 s for 256 documents and 28.914 s for 1024 documents, including final optimization. These are not full-corpus acceptance or a runtime guarantee.

Each new key gets a separate hosted Ubuntu 24.04 job: **180 min internal Index timeout / 240 min job**. Both Query arms run sequentially on one hosted runner with a 120 min limit. Jobs reserve at least 50 GiB local disk. Archives use low compression and Artifact transport disables recompression. No self-hosted runner or model API credential is required.

Index stdout/stderr stream to disk. Observed file-count checkpoints occur every 30 s; Linux process-tree RSS targets 1000 ms. Timeout/cancellation kills the Index process group and retains partial evidence through always-run uploads. Forced runner loss cannot guarantee the last upload.

| Stage | Measurements |
| --- | --- |
| Historical Index builder | Full build time and exact regular-file bytes: total/mean/p95/p99; RSS mean/p95/p99/sampled peak and coverage |
| Current Query | Mean/p50/p95/p99 public MCP RT, 50 samples per mode/version; group total and RSS distribution |
| Quality | Original gold-document Hit@1/5/10, MRR@10 and binary nDCG@10 |

**Cached Index timings are historical builder measurements**, with builder SHA, environment, Artifact and run provenance. Reuse creates neither a fresh observation nor a synthetic zero regression. Each full build provides one observation, so its percentiles do not estimate run-to-run variation.

Query changes use `(head − base) / base × 100%`; missing values or zero denominators are unavailable. RT excludes parsing/scoring/capture; group totals include them. Both arms use identical model bytes and concurrency limit 4. Fixed hybrid → fts → vector order and sequential arms retain first-call/model/page-cache effects. RSS may double-count shared pages and miss short peaks.

The final audit reparses saved public responses, checks hashes and exact requests, and recomputes scores/statistics. Full coverage, all 300 successful calls, unchanged corpus/model identities and available RSS are required. Quality has no arbitrary passing threshold. Missing/failed data cannot publish a green result.

Package/index/query jobs have read-only GitHub permissions. Separate request/reply jobs execute the frozen harness with comment-write access. The reply validates typed JSON identities/numbers and renders its own Markdown.

## Frozen inputs

Inputs follow the [BrowseComp+ integration at `c1299bf2`](https://github.com/zvec-ai/zvec-grep/tree/c1299bf23ef4d06b3640880a1f0beed8cca092f8/benchmarks/browse-comp-plus).

- Queries: `Tevatron/browsecomp-plus@144cff8e35b5eaef7e526346aa60774a9deb941f`, 830 original records.
- Corpus: `Tevatron/browsecomp-plus-corpus@b27b02bc3e45511b8b82a13e6f90ce761df726f6`, 100,195 documents.
- CI IDs: **628, 700, 966, 827, 241, 165, 1068, 244, 843, 563**, selected with original seed `browsecomp-plus-ci-v1`; no outcome-based exclusions.
- `source.lock.json` pins all 13 Parquet hashes and the decrypted query digest. `suites/ci-selection.json` retains original gold/evidence IDs. Plaintext questions remain outside the repository and indexed root.
- Each `<docid>.md` contains exact source UTF-8 `text`, without title/URL injection. Size cap: 64 MiB. Public ready-status must cover every document.
- Embedding: **`local/potion-multilingual-128m`, CPU**. Fresh Rust model content hash: `1d2c04d6b5a9ef342e46e32745357f6c09a594f5b99554d8048db83820f0d123`; the lock lists all three files. Weights/tokenizer match the scout, while Rust generates its own minimal tokenizer config instead of inheriting Node's config from a shared cache.

Quality averages five calls per question, then equally averages ten original questions. Preserve native top-ten chunk ranks without document deduplication. These scores measure document localization; repetitions are not additional independent questions. The full 830-question answer evaluation is outside this workflow.

## Local checks

```sh
python -m pip install -r benchmarks/browse-comp-plus-retrieval/requirements.txt
python -m unittest discover -s benchmarks/browse-comp-plus-retrieval/test -p 'test_*.py' -v
node --test benchmarks/zg-retrieval/test/*.test.mjs benchmarks/browse-comp-plus-retrieval/test/*.test.mjs .github/scripts/browsecomp-*.test.mjs .github/scripts/retrieval-pr-comment.test.mjs
```

Archive tests use tiny synthetic files to exercise sealing/corruption/path safety, not benchmark measurements. Execution modes are `run.mjs --mode build` and `run.mjs --mode query`; the workflow supplies frozen identities and a verified Artifact receipt.
