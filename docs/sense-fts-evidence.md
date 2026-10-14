# Rust Sense-derived FTS projection: implementation and synthetic evidence

This is the single evidence document for the fork contribution. It explains
the narrowly scoped change to the original upstream Rust indexer and links to
the frozen synthetic fixtures and result artifacts. The fixture sources,
qrels, raw runs, and generated metrics remain in the public Engram repository;
this contribution does not copy them into `zvec-grep`.

## Scope and provenance

The starting point is [`zvec-ai/zvec-grep@c1d2297`](https://github.com/zvec-ai/zvec-grep/tree/c1d2297ccd815af9305920943a1e0f08ae502d51),
the upstream revision used for the review-ready Rust candidate. Before this
change, [`lexical_text`](https://github.com/zvec-ai/zvec-grep/blob/c1d2297ccd815af9305920943a1e0f08ae502d51/rust/crates/zg-engine/src/pipelines/indexing/pipeline.rs#L1067-L1095)
added unlabelled symbol name, scope, signature, and documentation values to
the FTS text.

The implementation was developed as two focused commits in the fork:

1. [`87bf2863`](https://github.com/jordigilh/zvec-grep/commit/87bf2863f2be1693f47fe4137e7ce5aa71db3bc9)
   adds qualified code names and identifier parts to the Rust FTS projection.
2. [`dfb05527`](https://github.com/jordigilh/zvec-grep/commit/dfb0552753f7df5a83f9cce739119e6c335fedda)
   adds the NFKC and Unicode-category behavior needed by the identifier
   decomposition contract.

The upstream-ready branch replays those changes on `c1d2297` as commits
`808109c64e1a79e438b56d95b36ed8d20e8ac0f2` and
`0bbe5066a587197ce412ac5c712e88bbd0d6ccb2`. The lexical decomposition is
derived from the pinned [Sense source revision
`a65cd920`](https://github.com/luuuc/sense/tree/a65cd920d72a914b8d3d017463de8f0584bb383a),
but this is not a claim of complete Sense storage, graph, or search parity.

## What changed

For code entities, the Rust index now emits labeled lexical lines for:

- `symbol:` — the symbol kind and name when available;
- `qualified:` — the source-attested `scope::symbol` name;
- `name_parts:` — normalized, lower-case identifier parts;
- `scope:`, `signature:`, and `doc:` — the existing metadata, with signature
  and documentation whitespace collapsed for the searchable projection.

Identifier parts use NFKC normalization, Unicode letter/number categories,
underscore and punctuation boundaries, ASCII camel-case and acronym
boundaries, lower-casing, and first-occurrence de-duplication. For example,
`WorkflowState::Add` contributes:

```text
symbol: function Add
qualified: WorkflowState::Add
name_parts: workflow state add
scope: WorkflowState
```

Only the FTS lexical projection changes. The source fragment, vector/embedding
input, model, fusion, ranking, query modes, and result contract are unchanged.
The tests also assert that `qualified:` and `name_parts:` do not leak into the
vector content. Existing indexes must be rebuilt because their stored FTS text
predates this projection.

The implementation adds focused tests for scoped names, acronym and camel-case
boundaries, Unicode identifiers, NFKC compatibility characters, combining
marks, source preservation, extracted Go methods, and Markdown non-regression.
The candidate passed formatting, locked workspace checking, strict Clippy, and
the serial `zg-engine` library suite (**456 passed, 11 ignored**).

## Paired synthetic evaluation

The paired evaluation uses the same source snapshot, source-authored qrels,
embedding model, CPU device, index/query flags, result limit, and normalized
source-unit `@10` scorer for each baseline/candidate pair. Each source language
has its own fixture and relevance universe; scores are not pooled across
languages.

The table below reproduces the immutable [Engram paired multi-language
report](https://github.com/jordigilh/engram/blob/dd9422c0bda8e024988be370118603502e71c5f5/benchmarks/semantic_search/MULTILANGUAGE_PAIRED_RESULTS.md)
and shows values in the order **nDCG@10 / MRR@10 / Recall@10 /
Precision@10**. The TypeScript rows measure the TypeScript Sense-inspired
projection. The Rust rows measure the Rust qualified-name/identifier-parts
projection; TypeScript values must not be presented as Rust measurements.

| Source language | Engine change | Baseline | Candidate | Absolute delta |
| --- | --- | ---: | ---: | ---: |
| Go | TypeScript Sense lexical | 0.402280 / 0.712500 / 0.570833 / 0.200000 | **0.588774 / 0.733333 / 0.677083 / 0.250000** | +0.186494 / +0.020833 / +0.106250 / +0.050000 |
| Go | Rust lexical projection | 0.471013 / 0.667857 / 0.620833 / 0.225000 | **0.594156 / 0.743750 / 0.677083 / 0.250000** | +0.123143 / +0.075893 / +0.056250 / +0.025000 |
| Python | TypeScript Sense lexical | 0.508985 / 0.512996 / 0.664583 / 0.237500 | **0.546521 / 0.547917 / 0.689583 / 0.250000** | +0.037536 / +0.034921 / +0.025000 / +0.012500 |
| Python | Rust lexical projection | 0.530362 / 0.543750 / 0.639583 / 0.225000 | **0.554708 / 0.568750 / 0.689583 / 0.250000** | +0.024346 / +0.025000 / +0.050000 / +0.025000 |
| Rust | TypeScript Sense lexical | 0.434273 / 0.434028 / 0.585417 / 0.212500 | **0.416330 / 0.455556 / 0.560417 / 0.200000** | -0.017943 / +0.021528 / -0.025000 / -0.012500 |
| Rust | Rust lexical projection | 0.430053 / 0.435714 / 0.585417 / 0.212500 | **0.462670 / 0.457589 / 0.647917 / 0.237500** | +0.032617 / +0.021875 / +0.062500 / +0.025000 |
| TypeScript | TypeScript Sense lexical | 0.341929 / 0.417708 / 0.554167 / 0.200000 | **0.426791 / 0.591667 / 0.550000 / 0.200000** | +0.084862 / +0.173958 / -0.004167 / +0.000000 |
| TypeScript | Rust lexical projection | 0.320709 / 0.323958 / 0.570833 / 0.200000 | **0.411556 / 0.574603 / 0.512500 / 0.187500** | +0.090846 / +0.250645 / -0.058333 / -0.012500 |

### What the metrics mean

- **nDCG@10** measures ranked retrieval quality with graded relevance. Highly
  relevant source units receive more gain, and gain is discounted as the rank
  moves lower.
- **MRR@10** measures how early the first relevant source unit appears. A rank-1
  hit contributes `1.0`, a rank-2 hit `0.5`, and a query with no relevant hit in
  the top 10 contributes `0`.
- **Recall@10** is the fraction of all relevant source units retrieved within
  the normalized top 10.
- **Precision@10** is the fraction of the normalized top-10 source units that
  are relevant.

The scorer operates on normalized source units rather than raw overlapping
backend chunks. For MRR, recall, and precision, qrel grades 2–3 count as
relevant; nDCG retains the graded judgments.

### Interpretation and limitation

On these eight-query synthetic lanes, the Rust projection improves all four
aggregate metrics for Go, Python, and Rust source, while losing recall and
precision for TypeScript source. The result therefore demonstrates useful
lexical coverage and retrieval behavior on the frozen fixtures, not a universal
no-regression guarantee or a real-repository/LLM-answer quality claim.

The published paired replay was captured from an earlier isolated Rust
projection worktree based on `196a730`, with candidate patch hash
`0ddb37681a06720f284efd6781f4508da2b17de036697bddbe5cc38ce8994e4e`. It is
historical synthetic evidence for the same FTS-only projection scope, not a
fresh measurement of the rebased commits listed above. The current branch has
additional Unicode correctness coverage; exact current-commit ranking claims
require a matched rerun.

## Fixtures and reproducibility links

Each fixture contains a manifest, source-authored truth, and complete qrels:

| Source language | Fixture |
| --- | --- |
| Go | [`go-workflow-discovery-v1`](https://github.com/jordigilh/engram/tree/dd9422c0bda8e024988be370118603502e71c5f5/benchmarks/semantic_search/fixtures/go-workflow-discovery-v1) |
| Python | [`python-workflow-discovery-v1`](https://github.com/jordigilh/engram/tree/dd9422c0bda8e024988be370118603502e71c5f5/benchmarks/semantic_search/fixtures/python-workflow-discovery-v1) |
| Rust | [`rust-workflow-discovery-v1`](https://github.com/jordigilh/engram/tree/dd9422c0bda8e024988be370118603502e71c5f5/benchmarks/semantic_search/fixtures/rust-workflow-discovery-v1) |
| TypeScript | [`typescript-workflow-discovery-v1`](https://github.com/jordigilh/engram/tree/dd9422c0bda8e024988be370118603502e71c5f5/benchmarks/semantic_search/fixtures/typescript-workflow-discovery-v1) |

The exact raw runs, normalized runs, per-arm metrics, manifests, source/qrels
digests, binary hashes, and aggregate comparison are in the immutable
[paired run directory](https://github.com/jordigilh/engram/tree/dd9422c0bda8e024988be370118603502e71c5f5/benchmarks/semantic_search/multilanguage-runs/2026-09-24-four-language-paired-v4),
including [`comparison.json`](https://github.com/jordigilh/engram/blob/dd9422c0bda8e024988be370118603502e71c5f5/benchmarks/semantic_search/multilanguage-runs/2026-09-24-four-language-paired-v4/comparison.json).

The [four-language Sense lexical summary](https://github.com/jordigilh/engram/blob/d4e4bd6a2186d119f5258cab3457002303c6991f/benchmarks/semantic_search/fixtures/FOUR_LANGUAGE_SENSE_LEXICAL_BASELINE.md)
is useful as a TypeScript post-Sense reference, but its values are not Rust
measurements. The [focused Go Rust before/after replay](https://github.com/jordigilh/engram/blob/bb962f264904b27f2f6c1901637596224a30d603/benchmarks/semantic_search/fixtures/go-workflow-discovery-v1/QEVAL_RESULTS.md#isolated-rust-lexical-evidence-projection)
contains the same Rust Go comparison and its raw artifacts.

The complete [four-language protocol](https://github.com/jordigilh/engram/blob/dd9422c0bda8e024988be370118603502e71c5f5/benchmarks/semantic_search/MULTILANGUAGE_PROTOCOL.md)
specifies fresh indexes, CPU `local/potion-code-16m-v2`, direct hybrid search,
`refresh=off`, raw limit 10, source-unit normalization, and the replay command.
It also records the rule that a candidate must be evaluated separately for
each source language and that a failing lane must not be averaged away.
