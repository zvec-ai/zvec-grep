# CoIR-ZG

CoIR-ZG measures embedding models through the real zvec-grep indexing and
retrieval pipeline. It uses the official CoIR CosQA test split, materializes
each corpus document as a Python file, indexes those files with zvec-grep, and
evaluates the resulting document ranking.

This is intentionally different from the CoIR-Original protocol. CoIR-Original
embeds each source document directly. CoIR-ZG includes extraction, fragment
splitting, vector search, and fragment-to-document deduplication, so scores from
the two protocols must not be compared as if they were the same benchmark.

The checked-in [CosQA report](reports/cosqa.md) contains the results used by
the embedding model selection guide.

## Protocol

- Dataset: pinned revisions of the official CoIR CosQA corpus and test qrels
- Corpus: 20,604 Python files
- Queries: 500 test queries
- Retrieval: zvec-grep vector route only
- Candidate limit: 500 fragments
- Evaluation: deduplicate fragments to the top 100 CoIR document IDs
- Metrics: nDCG, Recall, MAP, and MRR at the documented cutoffs
- External verification: `ir-measures`

The materializer validates this corpus checksum:

```text
753082a57c28ef708ccf1fe327067b99a96c04e4383921be9099742a5f681fac
```

## Prerequisites

Install Node.js 22 or newer, npm, and
[uv](https://docs.astral.sh/uv/). From the repository root:

```sh
npm ci
npm run build
```

The Python scripts contain pinned inline dependencies, so they do not modify
the Harbor benchmark environment in the parent directory.

## Materialize CosQA

From the repository root:

```sh
uv run benchmarks/coir-zg/materialize_cosqa.py
```

Generated data is written to `benchmarks/coir-zg/work/data/cosqa/` and is not
committed. The script refuses to overwrite an existing materialization. Use
`--output <path>` to create a separate copy.

## Run a model

Populate the local model cache and create an untimed warm-up index:

```sh
node benchmarks/coir-zg/run.mjs \
  --model local/jina-embeddings-v2-base-code \
  --phase index
```

Then run the full measured index and all 500 queries:

```sh
node benchmarks/coir-zg/run.mjs \
  --model local/jina-embeddings-v2-base-code
```

This two-command sequence keeps a first model download out of the reported
index time. The second command rebuilds the index before querying it.

Use `--query-limit 10` for a workflow smoke test. It still indexes the full
corpus, but evaluates only the first ten queries:

```sh
node benchmarks/coir-zg/run.mjs \
  --model local/potion-code-16m-v2 \
  --query-limit 10
```

Use `--phase index` or `--phase query` to rerun only one phase. A query-only run
requires a compatible index already present under the materialized corpus.

### Remote models

Remote providers send the materialized corpus and query text to the configured
embedding service. For Qwen:

```sh
export DASHSCOPE_API_KEY="..."
node benchmarks/coir-zg/run.mjs --model qwen/text-embedding-v4
```

The runner creates a one-operation remote embedding permit for the benchmark
root. Remote model implementations can change behind a stable API model name,
so record the run date when publishing remote results.

## Verify and summarize

Recompute every complete result with the independent `ir-measures` evaluator.
The command fails if any metric differs from zvec-grep's calculation:

```sh
uv run benchmarks/coir-zg/verify_with_ir_measures.py
```

Render all complete model results as one Markdown table:

```sh
node benchmarks/coir-zg/summarize_results.mjs
```

Both commands accept path overrides; use `--help` for details.

## Generated artifacts

The default result for each model is stored under:

```text
benchmarks/coir-zg/work/results/cosqa/<provider>__<model>/
├── index.json
├── metrics.json
├── rankings.jsonl
└── ir-measures.json
```

`work/` is ignored because it contains the downloaded dataset, indexes, model
run artifacts, and potentially credentials-adjacent operational metadata.
Commit only reviewed aggregate reports from `reports/`.

## Interpreting performance

Quality metrics are reproducible for the pinned dataset, model, zvec-grep
revision, and protocol. Timing and memory are machine-dependent. Query latency
is end-to-end zvec-grep service latency rather than isolated encoder inference.
Different models can also lead the extractor to choose different fragment
sizes because zvec-grep respects each model's input limit. This behavior is part
of the CoIR-ZG system benchmark.
