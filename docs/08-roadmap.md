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
