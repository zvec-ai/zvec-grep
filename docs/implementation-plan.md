# Rust Rewrite Implementation Plan

> Temporary developer document
>
> This file tracks the active Rust rewrite. It is intentionally operational
> and may change frequently. Once the rewrite reaches its first stable release,
> completed history should move to release notes or issues and this document
> should be deleted or replaced by a normal roadmap.

## Objective

Replace the TypeScript beta with a clear Rust implementation consisting of a
standalone engine, a server built on the engine, and a CLI that can use the
engine directly or through the server.

There is no API or index compatibility requirement with the beta.

## Current status

The workspace and standalone engine are implemented on
`refactor/rust_rewrite`. The server and CLI crates are structural placeholders.

Completed engine capabilities:

- [x] Cargo workspace with `engine`, `server`, and `cli` crates.
- [x] Typed errors, operation progress, and cancellation.
- [x] Git-aware workspace scanning and caller filters.
- [x] Default pruning for dependency, build, and index directories.
- [x] Stable file and segment identities.
- [x] Incremental add, modify, delete, and unchanged detection.
- [x] Content hashing to avoid unnecessary re-embedding.
- [x] Per-file indexing failures without destroying the previous entry.
- [x] Plain text and Markdown extraction.
- [x] Tree-sitter extraction for Rust, TypeScript/TSX, JavaScript, Python, Go,
  Java, C, and C++.
- [x] Code symbol metadata.
- [x] Batched embedding abstraction.
- [x] Deterministic test/fallback embedder.
- [x] Optional llama.cpp GGUF embedder.
- [x] Real MiniLM GGUF runtime smoke test.
- [x] In-memory storage.
- [x] Persistent Zvec storage with FTS and vector fields.
- [x] Batched Zvec writes and bounded durable checkpoints.
- [x] Atomic manifest replacement and exclusive workspace locking.
- [x] Embedding model and dimension validation.
- [x] Explicit persistent index recreation for incompatible models.
- [x] Lexical, vector, and hybrid RRF search.
- [x] Adaptive recall after filtering.
- [x] Path, format, modification-time, symbol-name, and symbol-kind filters.
- [x] Search evidence containing recall scores and ranks.
- [x] Exhaustive lexical search without an index.
- [x] Engine status reporting.
- [x] Unit and cross-component tests for implemented behavior.
- [x] Formatting and strict Clippy checks for default and llama.cpp builds.

Current verification baseline:

- 21 default tests pass.
- The ignored MiniLM llama.cpp runtime smoke test passes with a 384-dimensional
  Q8 GGUF.
- `cargo clippy --workspace --all-targets -- -D warnings` passes.
- Clippy also passes for `zvec-grep-engine` with `llama-cpp` enabled.

## Decisions already made

- Rust replaces Node.js and TypeScript for the product core.
- Desktop and server platforms are the current targets; mobile is deferred.
- The engine is a synchronous library with no server or CLI dependency.
- Zvec is the production vector and full-text store.
- llama.cpp accepts GGUF models; the original MiniLM safetensors or ONNX files
  are not loaded directly.
- Local embedding is the default product direction. Remote providers remain
  replaceable implementations of the same trait.
- Lexical search belongs to the search subsystem, not a separate top-level
  engine.
- File parsing produces a common segment representation so future PDF support
  does not leak into search or storage.
- Index data lives under the workspace and is always treated as rebuildable.
- We optimize for a clear new API rather than compatibility with the beta.

## Remaining work

### Phase 1: performance characterization

Goal: establish evidence that the new pipeline fixes the original indexing
latency problem.

- [ ] Add a reproducible repository generator or benchmark fixture covering
  many small files, fewer large files, and mixed languages.
- [ ] Measure initial indexing at 10k and 100k files on representative CPU-only
  machines.
- [ ] Measure MiniLM throughput for embedding batch sizes such as 128, 256,
  and 512.
- [ ] Measure Zvec write and checkpoint costs independently from embedding.
- [ ] Measure warm incremental runs with no changes, timestamp-only changes,
  small edits, deletions, and interrupted runs.
- [ ] Record peak memory, time to first durable checkpoint, and cancellation
  latency.
- [ ] Profile before changing architecture; keep benchmark results with machine
  and model details.

Exit criteria:

- Initial and incremental performance numbers are documented.
- The dominant costs are known rather than assumed.
- Defaults for embedding and commit batch sizes are supported by measurements.
- No per-file model invocation, Zvec flush, or full-manifest rewrite remains.

### Phase 2: model lifecycle and embedding policy

Goal: make the local-model path usable without manual setup while preserving a
clean engine boundary.

- [ ] Choose the initial recommended MiniLM GGUF and quantization.
- [ ] Verify model provenance, redistribution terms, checksum, size, and
  embedding quality.
- [ ] Decide whether model download belongs in the CLI, server, a dedicated
  model-management library, or packaging.
- [ ] Define model cache location and offline behavior.
- [ ] Add checksum verification and safe partial-download recovery.
- [ ] Define CPU defaults and opt-in Metal, CUDA, or Vulkan build/distribution
  strategy.
- [ ] Benchmark whether an embedding cache materially helps duplicate content
  or branch-heavy workspaces.
- [ ] Define the remote-provider interface only when a concrete provider is
  implemented and tested.

Exit criteria:

- A new user can select or obtain a supported model with a clear license and
  checksum.
- Offline use is predictable.
- Switching models produces an explicit rebuild flow.
- CPU behavior is functional on every supported desktop platform.

### Phase 3: engine hardening

Goal: finish the library surface needed by both server and CLI.

- [ ] Review the public API and hide types that are implementation details.
- [ ] Add a builder if configuration growth makes direct constructors unclear.
- [ ] Decide whether search needs cooperative cancellation for very large
  exhaustive scans.
- [ ] Define index health and diagnostic information required by user-facing
  status commands.
- [ ] Test crash recovery around Zvec mutation and manifest checkpoint
  boundaries.
- [ ] Add malformed source, unusual path, permission, non-UTF-8, and disk-full
  tests where the platform permits them.
- [ ] Validate supported Windows and Linux builds in CI.
- [ ] Test the minimum supported Rust version rather than only newer compilers.
- [ ] Add license and source-policy checks using `cargo-deny` in CI.

Exit criteria:

- The public engine API is sufficient for server and CLI without exposing Zvec
  internals.
- Recovery behavior is tested.
- Linux, macOS, and Windows build/test jobs are green.
- The minimum Rust version is enforced.

### Phase 4: server

Goal: provide one long-running owner for workspace indexes and multiple clients.

- [ ] Choose the first transport after listing real clients and deployment
  constraints. Do not couple engine types directly to transport types.
- [ ] Define versioned request and response DTOs for index, search, status, and
  cancellation.
- [ ] Run synchronous engine operations on bounded blocking workers.
- [ ] Coordinate one indexing writer with concurrent search requests.
- [ ] Stream or poll structured progress without losing terminal errors.
- [ ] Propagate cancellation from disconnected or explicit client requests.
- [ ] Define workspace lifecycle, idle cleanup, shutdown, and recovery.
- [ ] Add server unit tests and end-to-end client/server tests.

Exit criteria:

- Two clients can safely search the same server-owned workspace.
- Indexing progress and cancellation work across the transport.
- A server restart reopens a compatible index and clearly rejects an
  incompatible one.

### Phase 5: CLI

Goal: deliver a small, understandable interface over the engine and server.

- [ ] Define the minimal command surface for index, search, status, model
  management, and server operation.
- [ ] Support direct lexical search without requiring a server or model.
- [ ] Decide when the CLI uses a server automatically and how users override
  that choice.
- [ ] Render progress without corrupting piped output.
- [ ] Provide human-readable and machine-readable search results.
- [ ] Show matched-by and evidence information in a useful debug mode.
- [ ] Map typed engine/server errors to concise messages and stable exit codes.
- [ ] Add parser, formatting, snapshot, and command-level tests.

Exit criteria:

- Common index and search workflows work end to end.
- Piped output is stable.
- A user can diagnose model mismatch, busy index, partial file failures, and
  unavailable server conditions.

### Phase 6: additional file formats

Goal: expand extraction without changing indexing or search architecture.

- [ ] Define an extractor selection mechanism if the current format dispatch
  becomes unwieldy.
- [ ] Add PDF text extraction with page locations.
- [ ] Decide whether scanned PDFs require optional OCR and keep it out of the
  default dependency set unless justified.
- [ ] Evaluate Office, HTML, notebook, archive, and image metadata support based
  on user demand.
- [ ] Add fixture-based extraction tests and document dependency licenses and
  native platform requirements for each format.

Exit criteria for PDF:

- Text PDFs produce searchable page-based segments.
- Corrupt and encrypted PDFs fail per file without aborting the index.
- Search and storage require no PDF-specific branches.

### Phase 7: release readiness

- [ ] Add CI for formatting, tests, Clippy, minimum Rust, platform builds,
  optional backends, and dependency policy.
- [ ] Add benchmarks with regression thresholds where stable enough.
- [ ] Complete user and operator documentation.
- [ ] Produce a third-party license or attribution artifact for distributed
  binaries and model assets.
- [ ] Define versioning for APIs, server protocol, index schema, and models.
- [ ] Package binaries and required native libraries for supported platforms.
- [ ] Perform a clean-machine installation and offline-use test.

## Legacy behavior checklist

The TypeScript implementation remains a reference for product behavior, not
for code structure. Before the first Rust release, explicitly decide the fate
of each useful capability:

- [x] Workspace-local indexes.
- [x] Incremental file diffing.
- [x] Git ignore and file filtering.
- [x] Maximum file size.
- [x] Code and Markdown structural extraction.
- [x] Embedding schema validation.
- [x] FTS and vector hybrid search.
- [x] Reciprocal-rank fusion.
- [x] Symbol, path, format, and time filters.
- [x] Exhaustive no-index lexical search.
- [x] Progress, cancellation, and per-file failures.
- [x] Diagnostics through search evidence and engine status.
- [x] Workspace locking.
- [ ] Automatic index freshness policy before search.
- [ ] Context assembly and structure enrichment above raw search results.
- [ ] Detailed timing and recall traces for diagnostics.
- [ ] Final decision on named collections or multi-root workspaces.
- [ ] Final decision on index-disable markers and related CLI behavior.
- [ ] Final decision on remote embeddings.
- [ ] Final decision on images and multimodal search.

An unchecked item is not automatically required. It means we must make and
record an explicit product decision rather than dropping the behavior by
accident.

## Quality gates for every phase

Before considering a phase complete:

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

When llama.cpp code changes:

```sh
cargo clippy -p zvec-grep-engine \
  --all-targets --features llama-cpp --locked -- -D warnings
```

When a compatible model is available locally:

```sh
ZVEC_GREP_TEST_MODEL=/path/to/model.gguf \
  cargo test -p zvec-grep-engine --features llama-cpp \
  llama_minilm_runtime_smoke_test -- --ignored
```

New behavior requires tests at the lowest sensible layer. Cross-component
behavior also needs an integration test. Performance changes require before
and after measurements rather than intuition alone.

## Open decisions

These decisions should be made only when their phase begins and enough evidence
is available:

- Recommended embedding model and quantization.
- Model download and update ownership.
- Server transport and protocol versioning.
- Direct-versus-server CLI selection policy.
- Automatic refresh semantics before search.
- Whether to add a persistent embedding cache.
- PDF library and optional OCR strategy.
- Distribution strategy for Zvec and llama.cpp native libraries.
- When mobile support should return to scope.

## Keeping this document useful

- Update checkboxes in the same change that completes the work.
- Record durable architectural decisions in `docs/design.md`, not only here.
- Move detailed work into issues once an issue tracker becomes the source of
  truth.
- Do not keep completed historical detail indefinitely.
- Remove this document when it no longer guides active implementation.

