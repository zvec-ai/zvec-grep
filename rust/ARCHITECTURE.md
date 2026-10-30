# Rust architecture

## Public application boundary

[`ZvecGrep`](crates/zg-engine/src/lib.rs) is the reusable engine entry point.
Its main asynchronous methods accept concrete options and return typed results:

```rust,ignore
ZvecGrep::context(ContextOptions) -> EngineResult<ContextResult>
ZvecGrep::files(FilesOptions) -> EngineResult<FilesResult>
ZvecGrep::paths(PathsOptions) -> EngineResult<PathsResult>
ZvecGrep::lookup_path(PathsOptions) -> EngineResult<PathLookupResult>
ZvecGrep::index(IndexOptions) -> EngineResult<IndexResult>
ZvecGrep::info(InfoOptions) -> EngineResult<InfoResult>
ZvecGrep::index_setup(IndexSetupOptions) -> EngineResult<IndexSetup>
ZvecGrep::drop_index(InfoOptions) -> EngineResult<bool>
```

`EngineResult<T>` is `Result<T, EngineError>`. Types live under the matching
method groups in [`zg_engine::api`](crates/zg-engine/src/api/mod.rs).
`ZvecGrep::new()` creates an instance; `close()` retires its resources.
Resident callers can enable the read-session cache, inspect runtime counts,
watch workspaces, and call `context_after_refresh` after coordinating a refresh.

Workspace roots are request state, so one long-lived instance can serve many
roots. Optional roots resolve from the current working directory according to
the operation. Multiple instances remain useful for tests and isolated callers;
there is no global engine singleton.

Public methods call private services directly. Transport command envelopes and
MCP tool routing live outside the engine; an in-process call returns the typed
result it requested. Authorization preflight and configuration are explicit
public modules, with their boundaries described in
[the contributor guide](CONTRIBUTING.md#engine-changes).

## Retrieval and indexing

`context` selects indexed retrieval or direct lexical search from its options:

```text
CLI / MCP -> ZvecGrep::context -> indexed retrieval (FTS / vector / hybrid)
                             -> embedded grep when rg is enabled
```

The private lexical service owns admission, grep matcher/searcher construction,
ignore-aware traversal, filtering, and deterministic ordering. Blocking search
runs outside Tokio's async workers; no external ripgrep executable is required.
`files` enumerates current files, while `paths` and `lookup_path` handle current
file-name lookup and its literal-source fallback without creating an index.

Indexing coordinates the native scanner, source extraction, model runtimes,
and zvec storage. Scan rules belong to the persisted workspace; query filters
narrow retrieval without rewriting those rules. Text and code share one
embedding model per workspace; images are currently skipped.

The workspace layer owns manifests, portable paths, credentials, locks,
and recoverable index publication under `.zvec-grep/`. See the
[engine guide](crates/zg-engine/README.md) and
[workspace internals](crates/zg-engine/src/workspace/README.md) for storage,
relocation, and authorization behavior.

## Resource ownership

The engine owns a private model runtime manager. Matching model configuration
and concurrency settings reuse cached runtimes and their loaded resources.
Leases keep active runtimes alive; closing the engine rejects new operations
and retires idle resources while active leases finish.

Embedding lifecycle events become `IndexProgress::embedding`. In-process
callers attach an `IndexProgressReporter` to `IndexOptions`; daemon streams carry
serializable progress events without the runtime-only callback.

The resident daemon owns workspace coordination, background jobs, watchers,
and one process-level `Arc<ZvecGrep>`. It enables cached index read sessions and
reuses engine resources across requests and roots. Direct CLI execution retains
resources for the command's lifetime. Mode differences are recorded in
[`compat/mode-differences.toml`](compat/mode-differences.toml).

## Crate boundaries

- `zg-engine`: typed engine API and private retrieval, indexing, extraction,
  model, storage, and workspace services.
- `zg-cli` and `zg`: argument parsing, terminal rendering, and binary lifecycle.
- `zg-host-native`: native scanner and watcher implementations used by the engine.
- `zg-llama-native`: native llama/ggml backend loading boundary.
- `zg-daemon`: resident process, HTTP endpoints, jobs, and stdio bootstrap.
- `zg-daemon-protocol`: transport-only command, reply, and progress DTOs.
- `zg-transport-mcp`: tool schemas, input validation, result presentation,
  consent, and index setup.
- `zg-testkit`: fixtures and shared test helpers.

## Dependency rules

- Application code uses the engine's public types and methods.
- Transport DTOs stay outside the engine API.
- Native dependency types stay in their owning crate; private engine services
  coordinate them without an adapter registry or generic command bus.
- Shared abstractions require multiple real implementations.
- Concurrency limits stay next to the resources they constrain.
- Cancellation, deterministic output, partial-artifact cleanup, and native
  revision tracking are part of integration review.

See [contributing](CONTRIBUTING.md) for checks and compatibility expectations.
