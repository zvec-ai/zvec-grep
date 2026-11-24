# Blink Multilingual 64M and Bekko 256 INT8 on vector retrieval

On the November 24, 2026 run, Bekko's File Recall@10 was higher by 3.33 percentage points on BEIR and 3.27 on DuRetrieval. Blink's File Recall@5 was higher on both pilots. On DuRetrieval, Blink returned a positive passage in the top ten for all ten queries, while Bekko did so for nine. Bekko covered a larger fraction of the published positive passages per query.

## Recall

File metrics use the original positive document or passage IDs. Entity metrics use diagnostic evidence spans. All 20 BEIR questions have File gold and 14 have scored Entity gold. All ten DuRetrieval questions have both. Percentages are query averages after averaging five calls per query.

| Suite | Model | File R@1 | File R@5 | File R@10 | Entity R@1 | Entity R@5 | Entity R@10 | File Hit@10 | File nDCG@10 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| BEIR | Bekko | 30.62% | 47.92% | 62.71% | 45.24% | 66.67% | 79.76% | 75.00% | 0.5046 |
| BEIR | Blink | 23.96% | 50.42% | 59.38% | 30.95% | 70.24% | 82.14% | 70.00% | 0.4544 |
| DuRetrieval | Bekko | 11.43% | 40.83% | 54.23% | 15.00% | 44.48% | 58.62% | 90.00% | 0.5128 |
| DuRetrieval | Blink | 12.26% | 43.51% | 50.95% | 15.83% | 46.40% | 54.60% | 100.00% | 0.5311 |

| BEIR dataset | Queries | Bekko File R@10 | Blink File R@10 |
| --- | ---: | ---: | ---: |
| SciFact | 10 | 70.00% | 60.00% |
| NFCorpus | 4 | 51.04% | 59.38% |
| ArguAna | 3 | 100.00% | 100.00% |
| FiQA | 3 | 16.67% | 16.67% |

Relative to Bekko, Blink improved File Recall@10 on one BEIR question, tied on 18 and declined on one. On DuRetrieval it improved on two, tied on five and declined on three.

## Performance

| Suite | Model | Vector calls | Mean (ms) | P50 (ms) | P95 (ms) | Fresh indexing (s) |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| BEIR | Bekko | 100 | 4.29 | 4.21 | 4.91 | 73.88 |
| BEIR | Blink | 100 | 4.26 | 4.15 | 5.14 | 73.24 |
| DuRetrieval | Bekko | 50 | 5.13 | 5.02 | 6.14 | 78.65 |
| DuRetrieval | Blink | 50 | 5.58 | 5.53 | 6.50 | 76.66 |

MCP latency includes query embedding, retrieval and full-preview response handling. The standard runner executes hybrid, FTS and then vector. All 300 vector calls succeeded and each question returned the same ordered top ten on all five calls. P95 uses nearest rank. Each indexing measurement covers one fresh build per corpus with cached model files. The six BEIR builds are summed. The short query sets and single builds limit the precision of timing comparisons.

## Reproduction and evidence

- Tested source and harness `2cabe3a18ad1987367f0d3d6564c2f57773da14f`, based on `origin/main` at `3d57ea50a72aab8a54f783a76f265efdc52a9b16`. All four runs used the same CPU release tarball, SHA-256 `d4856f3077c3425c4d1fe91167fded9a337b59a04bd86386cb0a5c9d727a1f26`, on a 12-core Apple M6 running macOS 27.0.
- Both models use normalized 256-dimensional vectors with a 32,768-token ZG input budget. Blink uses the Model2Vec FP32 table, 252.03 MiB including its tokenizer. Bekko uses the row-wise INT8 table, 54.46 MiB including its tokenizer. [Blink artifacts](https://huggingface.co/minishlab/blink-multilingual-64M/tree/a7198ef875b9a756b8b641ab064542f7408e50f3) · [Bekko artifacts](https://huggingface.co/hotchpotch/bekko-static-embedding-v1/tree/c18167b8f244c1bc6de21ce9b710bac8a29c4615)
- BEIR indexes 75,128 unique documents. ArguAna's per-query self-match exclusion brings the six-index total to 92,473 files per model. DuRetrieval indexes all 100,001 passages in the pinned C-MTEB subset. Each pair produced identical entity and fragment counts, 92,435 of each on BEIR and 100,004 of each on DuRetrieval. [BEIR lock](data/beir20.json) · [DuRetrieval lock](data/duretrieval10.json) · [Runner and scoring](README.md)
- The four successful runs executed serially, Blink then Bekko on BEIR, followed by Bekko then Blink on DuRetrieval. After two macOS CLI startup failures during setup, the runs shared one verified native installation. Both models used `GGML_METAL_NO_RESIDENCY=1` (the CLI's macOS default) and `ZVEC_GREP_INSTALL_SKIP_SERVER=1` during setup. Each run used fresh corpus, index and daemon state. The runner verified the cached source files against the locks and used PyArrow 17.0.0 for BEIR and 21.0.0 for DuRetrieval.
- Local evidence is under `.cache/blink-multilingual/`. Each directory below contains `report.json`, `report.md` and the per-call MCP records under `evidence/`. The same root holds the run script, verifier and `comparison.json`. The earlier setup attempts are retained in `blink-beir/` and `blink-beir-native/`.

| Run directory | SHA-256 of report.json |
| --- | --- |
| `bekko-beir-shared` | `fe5bcae40b99560aff3941fd2ab055df5c02279950212a4556f7086ee47562ae` |
| `blink-beir-shared` | `4470d39d5788fc09248507741a1367913eadd37d19b70fe0ca62c53c6f3b6c0a` |
| `bekko-du-shared` | `816313cd5f2653f5039df3d17a189bf78a3fec33a05fe66d19f236049e3e814b` |
| `blink-du-shared` | `c2dda3cd36721800636fe4676ab0dd96c2de6d0b8878b818421eede06bb7a12e` |
