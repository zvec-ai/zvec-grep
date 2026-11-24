# Blink and Bekko vector retrieval at 1,024 tokens

Bekko retained the higher File Recall@10 on both pilots, by 3.33 percentage points on BEIR and 3.27 on DuRetrieval. Blink retained the higher File Recall@5 on both. At 1,024 tokens, Blink returned at least one relevant passage for all ten DuRetrieval queries and Bekko did so for nine.

## Recall at 1,024 tokens

| Suite | Model | File R@1 | File R@5 | File R@10 | Entity R@1 | Entity R@5 | Entity R@10 | File Hit@10 | File nDCG@10 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| BEIR | Bekko | 30.62% | 47.92% | 62.08% | 41.67% | 66.67% | 79.76% | 75.00% | 0.4979 |
| BEIR | Blink | 23.96% | 50.42% | 58.75% | 27.38% | 66.67% | 82.14% | 70.00% | 0.4489 |
| DuRetrieval | Bekko | 11.43% | 40.83% | 54.23% | 15.00% | 44.48% | 58.62% | 90.00% | 0.5128 |
| DuRetrieval | Blink | 12.26% | 44.94% | 50.95% | 15.83% | 47.83% | 54.60% | 100.00% | 0.5331 |

**Change from 32,768 tokens.** On BEIR, both models lost exactly 0.625 percentage points of File Recall@10. For both models the entire difference came from NFCorpus question `PLAIN-430`, where Recall@10 fell from 37.5% to 25%. Aggregate Entity Recall@10 held steady for both. On DuRetrieval, each model retained its File Recall@10 on every query. Blink's aggregate File and Entity Recall@5 both rose by 1.43 percentage points. Bekko's Recall@1/5/10 held steady at both levels. [32,768-token results](blink-multilingual-bekko.md)

## Vector query and indexing performance

Time deltas below subtract the earlier 32,768-token measurement. Each pair of latency and index-time columns shows the current time and the signed difference.

| Suite | Model | Mean (ms) | P50 (ms) | Δ P50 (ms) | P95 (ms) | Index wall time (s) | Δ index time (s) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| BEIR | Bekko | 3.80 | 3.72 | -0.49 | 4.50 | 73.86 | -0.02 |
| BEIR | Blink | 3.82 | 3.75 | -0.40 | 4.40 | 73.83 | +0.59 |
| DuRetrieval | Bekko | 4.37 | 4.29 | -0.73 | 4.89 | 75.35 | -3.30 |
| DuRetrieval | Blink | 4.28 | 4.16 | -1.37 | 4.84 | 71.50 | -5.16 |

Both models indexed the same 92,473 files across six BEIR indexes and 100,001 files in the DuRetrieval index at each budget. The smaller budget increased BEIR fragments from 92,435 to 102,286 and DuRetrieval fragments from 100,004 to 104,927 for each model. The new index snapshots report a 1,024-token limit with a normal fragment budget of 1,894 UTF-16 units and a dense fragment budget of 1,024 units.

Each run measured one fresh index build per corpus with cached weights. MCP latency includes full-preview output and was measured after hybrid and FTS, in the standard mode order. The timing differences describe these two sessions on one machine. Model weights, dimensions, CPU backend, corpus, gold and scorer were held fixed. [Runner and reproduction commands](README.md)

## Evidence

- Measured November 24, 2026 on the same 12-core Apple M6 and macOS 27.0 as the baseline. The new build used commit `8827af48d2838b7a4f7fcdf379a3a2f139374ceb` and CPU release tarball SHA-256 `757f414277a0637c7c58e9a515fe377b904476c6d1f6ce42b227f5b51efd9383`. Both model entries apply the 1,024-token limit to embedded text and to index fragment sizing.
- Across both models, all 300 vector calls succeeded. All five repetitions returned the same ordered top ten for each query. File metrics average 20 BEIR and ten DuRetrieval queries. Diagnostic Entity metrics average the 14 BEIR and ten DuRetrieval queries with scored gold. Gold, task IDs, query text and relevance judgments were verified across budgets. P95 is nearest rank.
- Runs executed serially, Blink then Bekko on BEIR and Bekko then Blink on DuRetrieval. Three preliminary Blink BEIR attempts encountered daemon initialization failures before queries in some groups. The third was stopped. For all four reported runs the harness launched a fresh daemon after direct indexing and checked its health before connecting the same public stdio MCP client. All 14 daemons became ready on the first attempt and all searches used the original timing and scoring code.
- Local run artifacts are under `.cache/blink-multilingual/1024/`. The four directories below contain the raw MCP records, index snapshots and reports. `comparison.json` contains per-question results and `budget-deltas.json` records the verified changes. The three preliminary directories are retained separately.

| Run directory | SHA-256 of report.json |
| --- | --- |
| `bekko-beir-final` | `9e12259b180a5927a8d8d592dd1896ac79158e6e95a6f3c53bf7870cb7f9eb95` |
| `blink-beir-final` | `87fd81e8f55d944d06c8d22071f951b6a6a910d210d876cda4c149be78299432` |
| `bekko-du-final` | `03bc003b1c98efd6c9d1aada91f0d7d8256cea644f1c0e6204636b8173e9818b` |
| `blink-du-final` | `efb957d887b078aad589bbbdb9cfaf3046df5f650e01d1a772292fe617e40a1a` |
