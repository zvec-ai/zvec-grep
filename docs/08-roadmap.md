# Roadmap

[Documentation](./README.md) · [Agents](./01-agents.md) ·
[CLI](./02-cli.md) · [MCP](./03-mcp.md) · [Pipeline](./04-pipeline.md) ·
[Architecture](./05-architecture.md) · [Server](./06-server.md) ·
[Embedding](./07-embedding.md) · [Roadmap](./08-roadmap.md)

zvec-grep is under active development. The destination is one dependable,
local-first search layer that people and agents can use without choosing among
separate retrieval tools. This roadmap describes direction, not promised dates;
priorities may change as we learn from real workloads.

## Now — make the public preview dependable

- Make installation, updates, and removal reliable across current platforms and
  agent integrations.
- Harden incremental indexing, freshness, Server recovery, and concurrent
  workspace access.
- Establish reproducible search-quality, performance, and agent-context
  evaluation.
- Stabilize the CLI, MCP contracts, configuration, diagnostics, and index
  compatibility policy before a stable release.

## Evaluation roadmap

Evaluation advances in layers so that a regression can be attributed to
retrieval, context presentation, or agent behavior instead of being hidden in
one end-to-end score.

### Retrieval-only — implemented and expanding

The current Retrieval-only benchmark calls the public Rust MCP search endpoint
directly, without an answering agent, query rewriting, subqueries, or an LLM
judge. It preserves each dataset's original queries and published or frozen
relevance labels. SWE-QA connects these search diagnostics to later end-to-end
agent evaluation; BEIR, DuRetrieval, and Quarry broaden general, Chinese, and
multilingual code-retrieval coverage.

Completed in [#198](https://github.com/zvec-ai/zvec-grep/pull/198):

- [x] Exercise `hybrid`, `fts`, and `vector` retrieval under one versioned
  protocol, with five calls per query.
- [x] Report File Hit@1/5/10, File MRR@10, nDCG@10, output size, latency, and
  ranking stability from public MCP responses.
- [x] Run SWE-QA20, BEIR, DuRetrieval, and Quarry as independent parallel suites
  that reuse one exact-commit Rust build.
- [x] Publish suite-level and per-query diagnostics, preserve partial results
  when individual tasks fail, and aggregate all suites on one final page.
- [x] Support the suite-specific local models and an optional remote
  `qwen/qwen3.7-text-embedding` profile.

Next steps:

- [ ] Expand the current pilot samples while retaining source-provided queries,
  relevance labels, dataset revisions, and deterministic projections into zg's
  file-level result model.
- [ ] Add dataset-native function, span, or line-level scoring where authoritative
  fine-grained labels exist; keep file-level scoring for datasets that only
  publish document-level relevance.
- [ ] Accumulate repeatable baselines across zg versions and embedding profiles,
  then define regression policy from observed variance instead of an arbitrary
  quality threshold.
- [ ] Add new suites through the shared declarative suite configuration and keep
  dataset preparation, scoring, reporting, and product execution independent.

The benchmark design, limits, and reproduction commands live in the
[Retrieval-only benchmark guide](../benchmarks/zg-retrieval/README.md).

### Agent end-to-end — next

- [ ] Use the Retrieval-only SWE-QA results as diagnostic context for answer and
  task-completion evaluation, so retrieval regressions can be separated from
  planning or reasoning regressions.
- [ ] Measure whether sufficient evidence remains visible under a fixed output
  budget, together with answer correctness, citation support, tool calls,
  latency, and token use.
- [ ] Add controlled comparisons for context presentation and agent behavior
  without changing the frozen retrieval inputs underneath them.

## Product direction

The following four directions are part of the product vision, not optional
experiments. Their sequencing may change, but each is required for zg to become
the search layer we want it to be.

### 1. Search more data formats, natively

- Make PDF, PowerPoint (`.ppt` and `.pptx`), HTML, images, and other multimodal
  documents first-class searchable inputs.
- Add format-aware extraction that preserves useful structure, layout, metadata,
  and relationships instead of flattening every file into plain text.
- Combine text extraction, OCR, vision-language understanding, and multimodal
  Embedding where each format benefits from them.
- Use format- and content-aware retrieval so zg can choose a smarter strategy
  for each query and source.

### 2. Strengthen retrieval

- Add knowledge-graph construction and graph retrieval to complement BM25,
  vector search, and managed ripgrep.
- Expand multi-route hybrid retrieval across lexical, vector, graph, structural,
  and metadata signals.
- Improve query planning so people and agents can express intent without
  manually choosing retrieval routes.
- Improve fusion, reranking, evaluation, and explainability while keeping the
  returned context compact.

### 3. Make zg more out of the box

- Provide a local GUI for search, workspace management, indexing, model setup,
  permissions, and diagnostics.
- Support more installation paths beyond npm, including platform-native package
  managers and installers where appropriate.
- Make first-run setup, Agent discovery, model selection, updates, and recovery
  increasingly automatic with useful defaults.
- Preserve CLI and configuration control for advanced users while removing it
  from the critical path for everyone else.

### 4. Reach every platform, from PC to mobile

- Deliver a consistent experience across macOS, Windows, and Linux desktops.
- Extend the local search layer to iOS and Android instead of treating mobile as
  a remote client to a required cloud service.
- Adapt indexing, storage, and model execution to the memory, power, and lifecycle
  constraints of mobile devices.
- Keep the same local-first trust and permission model across desktop and mobile.

## Guardrails

- Local-first remains the default; remote content transfer always requires
  explicit authorization.
- zg should hide retrieval-tool choice from users without hiding useful control.
- Better recall must not come at the cost of noisy, oversized agent context.
- A hosted service must never be required for the core local workflow.

## Help shape the roadmap

Priorities should come from real use. Open a
[GitHub issue](https://github.com/zvec-ai/zvec-grep/issues) to describe a
workflow, limitation, or result that matters to you. Contributions are welcome;
see the [Contributing Guide](../CONTRIBUTING.md).
