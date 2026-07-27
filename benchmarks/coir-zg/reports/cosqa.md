# CoIR-ZG CosQA results — 2026-07-25

These results evaluate embedding models through zvec-grep's real file
extraction, fragment indexing, vector retrieval, and document deduplication
pipeline. They are not directly comparable to the CoIR-Original public
leaderboard.

## Run metadata

| Field              | Value                                                              |
| ------------------ | ------------------------------------------------------------------ |
| zvec-grep commit   | `796a3c5762160110dbf6a04c956fc1f2935870b2`                         |
| Corpus             | 20,604 Python files                                                |
| Test queries       | 500                                                                |
| Corpus SHA-256     | `753082a57c28ef708ccf1fe327067b99a96c04e4383921be9099742a5f681fac` |
| Retrieval          | vector-only, 500 fragment candidates                               |
| Evaluation         | fragment hits deduplicated to top 100 documents                    |
| External evaluator | `ir-measures==0.4.1`                                               |
| Machine            | Apple M4 Pro, 48 GiB RAM                                           |
| Operating system   | macOS 15.1.1                                                       |
| Node.js            | 25.5.0                                                             |
| `@zvec/zvec`       | 0.5.0                                                              |
| Transformers.js    | 3.8.1                                                              |
| node-llama-cpp     | 3.18.1                                                             |

Local index timings use an already populated model cache and therefore exclude
the initial model download. Remote index timings include network requests to
the provider. All metrics were independently reproduced from the saved
rankings with `ir-measures`.

## Results

| Model                                | nDCG@10 | Recall@10 | MAP@10 | MRR@10 | nDCG@100 | Recall@100 | MAP@100 | Index time | Mean query | Index peak RSS | Query peak RSS |
| ------------------------------------ | ------: | --------: | -----: | -----: | -------: | ---------: | ------: | ---------: | ---------: | -------------: | -------------: |
| `local/jina-embeddings-v2-base-code` |  0.3947 |    0.6860 | 0.3060 | 0.3060 |   0.4591 |     0.9820 |  0.3196 |    432.2 s |   355.5 ms |       2.58 GiB |       3.46 GiB |
| `local/embeddinggemma-300m`          |  0.3892 |    0.6900 | 0.2975 | 0.2975 |   0.4532 |     0.9740 |  0.3119 |    794.3 s |   368.5 ms |       2.74 GiB |       3.47 GiB |
| `local/qwen3-embedding-0.6b`         |  0.3680 |    0.6520 | 0.2812 | 0.2812 |   0.4324 |     0.9520 |  0.2947 |   1695.5 s |   358.6 ms |       4.44 GiB |       6.94 GiB |
| `qwen/qwen3.7-text-embedding`        |  0.3381 |    0.6120 | 0.2549 | 0.2549 |   0.4092 |     0.9380 |  0.2700 |    169.5 s |   576.3 ms |       1.08 GiB |       1.58 GiB |
| `qwen/text-embedding-v4`             |  0.3304 |    0.5780 | 0.2549 | 0.2549 |   0.4054 |     0.9300 |  0.2701 |    142.6 s |   542.3 ms |       1.13 GiB |       1.58 GiB |
| `local/all-minilm-l6-v2`             |  0.2850 |    0.4960 | 0.2209 | 0.2209 |   0.3655 |     0.8740 |  0.2374 |    197.1 s |   348.0 ms |       1.23 GiB |       1.79 GiB |
| `local/bge-small-en-v1.5`            |  0.2798 |    0.5080 | 0.2101 | 0.2101 |   0.3493 |     0.8420 |  0.2236 |    305.9 s |   331.1 ms |       1.34 GiB |       1.99 GiB |
| `local/potion-code-16m-v2`           |  0.2486 |    0.4520 | 0.1867 | 0.1867 |   0.3296 |     0.8440 |  0.2022 |     40.0 s |   349.5 ms |       1.06 GiB |       1.59 GiB |
| `local/potion-base-8m`               |  0.1464 |    0.2680 | 0.1097 | 0.1097 |   0.2103 |     0.5780 |  0.1218 |     41.6 s |   339.4 ms |       1.06 GiB |       1.72 GiB |

All nine runs indexed 20,604 files with zero failed files and zero truncated
fragments. CosQA has one relevant document per query in this test setup, so
MAP@10 and MRR@10 are numerically equal.

## Selection takeaways

- `local/jina-embeddings-v2-base-code` delivered the best overall local code
  retrieval quality and the highest Recall@100.
- `local/embeddinggemma-300m` was in the same quality tier as Jina, with
  slightly higher Recall@10 but slower indexing.
- `local/qwen3-embedding-0.6b` ranked third locally, but required the most time
  and memory.
- `local/all-minilm-l6-v2` was a better lightweight balance than BGE in this
  run: BGE had slightly higher Recall@10, while MiniLM had higher nDCG@10 and
  Recall@100 and indexed faster.
- `local/potion-code-16m-v2` indexed roughly five times faster than MiniLM and
  more than ten times faster than Jina, at a clear quality cost.

## Limitations

- This is one code-search task, not a complete multilingual or mixed-document
  benchmark.
- Model input limits affect zvec-grep's extractor fragment sizes, so the result
  measures the complete system rather than an isolated encoder.
- Query latency includes zvec-grep and vector-store overhead.
- Performance numbers are specific to the machine and runtime versions above.
- Remote provider implementations may change without changing the public model
  name.
