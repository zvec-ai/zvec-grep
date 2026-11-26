# zg-engine

`zg-engine` is the Rust library for indexing files and retrieving source content through full-text and vector search.

## Usage

Create a `ZvecGrep` instance and reuse it across operations and workspaces. This example indexes the current directory and searches it using the default options:

```rust
use zg_engine::{
    ZvecGrep,
    api::{context::ContextOptions, index::IndexOptions},
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::current_dir()?;
    let engine = ZvecGrep::new();

    engine
        .index(IndexOptions {
            root: Some(root.clone()),
            ..IndexOptions::default()
        })
        .await?;

    let results = engine
        .context(ContextOptions {
            root: Some(root),
            query: Some("where is authentication handled?".into()),
            limit: Some(5),
            ..ContextOptions::default()
        })
        .await?;

    for item in results.items {
        println!("{}", item.relative_path.display());
    }

    engine.close();
    Ok(())
}
```

## Behavior

### Workspaces

A workspace is a directory of files that zg indexes and searches as a unit, with its own scanning rules, model settings, and index. Search and status from a subdirectory discover the nearest enclosing workspace; a Git repository is not required.

### Index storage

Saved settings live in the source directory's fixed `.zvec-grep` child, with active index data in `.zvec-grep/index`. Moving or copying the source directory together with `.zvec-grep` preserves the index; downloaded models are cached separately and shared across workspaces. Sharing `.zvec-grep` also shares access to any credentials saved in it.

### File selection

Scanning respects ignore rules, excludes hidden files, does not follow symlinks, and traverses nested Git repositories by default. File formats inferred from names, path patterns, additional ignore files, depth limits, and file size limits let you adjust the scope. These rules are saved with the workspace and reused for future indexing.

### Index lifecycle

An index is created on the first indexing run and updated as files are added, changed, or removed. It can be rebuilt from scratch when needed, or deleted without removing source files or shared model caches. Indexing can complete with individual file failures, which are reported separately.

### Full-text analysis

New indexes use the defaults in `~/.zvec-grep/fts.json`. Omitted fields default
to `{"tokenizer":"standard","filters":["lowercase"],"extraParams":{}}`.
Providing only filters or extra parameters keeps the configured tokenizer.
A language preset supplies all three fields, and explicit fields override the
preset. A supplied filter list or parameter object replaces the whole field,
including an empty list or object.

For example, `zg --config fts set --fts-lang zh` saves Jieba with lowercase as
the default for new workspaces. English and other built-in stemming presets
use Standard with lowercase and a language-specific stemmer. To add a preset
or replace a built-in one, give its complete configuration in
`~/.zvec-grep/fts-languages.json`:

```json
{
  "version": 1,
  "languages": {
    "en": {"tokenizer": "standard", "filters": ["lowercase"], "extraParams": {}}
  }
}
```

`zg --config fts langs` lists the effective presets. The CLI expands a preset
on the caller before dispatch to Direct or Server mode. MCP expands `ftsLang`
using the server user's mappings. zvec validates the resolved analyzer.

Indexes persist the three resolved fields. Queries, watcher refreshes, and
ordinary rebuilds use the saved analysis even after defaults or mappings
change. Historical indexes without FTS metadata use Jieba with lowercase.
To change an index, supply the new analysis on the rebuild request, for example
`zg --index /path/to/docs --rebuild --fts-lang en`. Use `zg --status` to see
the saved analysis.

### Search

Indexed search combines full-text and vector retrieval to return relevant source snippets. Index refresh follows the caller's policy; resident execution can schedule background refresh. Query filters narrow results without changing the workspace's scanning rules. Direct text search reads source files without requiring an index.

In indexed search, `--trace` shows each hit's rank (`#1`, `#2`, …) and `score=`.
Code hits show the rule-ranking score. Other hits show the raw reciprocal rank
fusion (RRF) score. These scores use different scales, so use the displayed
rank for the final order. The trace's `fusion.score` gives the raw RRF score
for either kind of hit. See the [ranking guide](src/pipelines/indexed_search/ranking/README.md)
for the shared Direct, Server and MCP selection policy.

### Vector storage

Embedding vectors are stored as IEEE binary16. The engine keeps embeddings and query vectors as `f32` until they are narrowed for the FP16 field. `indexVersion` remains 2. Existing FP32 indexes require an explicit rebuild with `zg --index --rebuild` before use.

### Embedding models

Each workspace uses an embedding model for semantic search. Local models run on your machine and may need an initial download; remote models send content or queries to an external service and require authorization. Changing the model or encountering an incompatible index format requires rebuilding the index.

Model selection runs at the shared engine boundary for Direct, Server and MCP.
An existing compatible index keeps its stored model unless a replacement is
explicitly requested. Resolve the final selection after acquiring the writer
lock so an index published while a request was queued takes precedence over
the caller's default.

Runtime HTTP models use `http/<alias>` entries in the global `models` configuration, specifying the endpoint, served model, vector shape, limits, authentication, and query/document prefixes alongside each other. For `bearer` authentication, `--api-key` takes precedence over `apiKeyEnv` and follows the workspace's encrypted indexing and temporary query credential policy. The workspace saves the resolved descriptor and authorized endpoint for subsequent indexing and search; global edits do not silently change an existing index's embedding space. HTTP models retain the same content-transfer authorization and runtime concurrency/cancellation boundaries as built-in remote models. See the [embedding model guide](https://zvec.org/en/docs/zvec-grep/embedding-models/) for configuration.

## Resource ownership

Reuse one `ZvecGrep` across requests and workspaces. The engine owns model
runtimes; compatible model configurations share loaded resources. Active leases
keep those resources alive. `close()` retires cached resources while in-flight
users retain ownership until completion.

Model preparation is lazy, cancellable and shared. Empty or unchanged indexing
work should not load models. Artifact acquisition verifies downloads before
publication and cleans up partial files. Backends use the shared runtime and
artifact layers rather than creating independent caches or schedulers.

Concurrency limits belong to the resources they protect. In a resident ORT
runtime, a request's concurrency limits its use of the existing pool; it must
not resize the pool or change the shared thread configuration. Direct execution
owns resources for one command; server settings inherited from the environment
require a daemon restart to change resident capacity.

The daemon enables cached index read sessions. In-process callers can opt in
with `enable_read_session_cache()`. Cache entries are generation-specific;
workspace metadata and effective model settings still resolve for each request.
See [workspace coordination](src/workspace/README.md#read-and-write-coordination)
for writer admission and publication rules.

`IndexOptions::on_progress` is an in-process callback. It is omitted from
serialized requests; transports carry progress events instead.

### llama.cpp concurrency

Index embedding concurrency is configured with
`zg --index --embedding-concurrency <n>`.
The option is rejected outside `--index`. For automatic indexing and
refreshes as well, set `ZVEC_GREP_EMBEDDING_CONCURRENCY=<n>`.
Explicit zero is rejected. Explicit CLI/API index settings take precedence over
this environment variable, followed by `ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM` as
the llama.cpp indexing fallback, then backend defaults.
Invalid environment values warn and select the automatic default. These overrides
do not configure a separate query worker and are not persisted in workspace settings.
Direct and Server indexing use the same engine boundary; restart the Server after
changing its inherited environment, while explicit CLI overrides need no restart.

For llama.cpp, C is the concurrent context limit on CPU, Metal and CUDA:

- Direct mode sizes the context pool from the operation's C (default 1).
  Pools are local to that process; independent Direct processes do not share resources.
- Each operation admits one embedding batch at a time, independently of C. That
  batch can use up to C contexts internally. Separate operations have independent
  batch admission and can execute concurrently through the shared context pool;
  this does not serialize all Server requests.
- Server startup captures `ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM` (default 1) as a
  hard limit on executing llama.cpp contexts across all models and devices in
  that Server. The general index environment and client C cannot enlarge this
  capacity. Index requests retain the CLI/API > index environment > llama.cpp
  environment > default precedence above, then are capped to the Server limit.
  The effective value is reported through progress/logging. Each operation's
  batches share its own C allowance; different C values reuse the same model/device
  runtime. Omitted index C uses the general index default when set, otherwise the
  configured llama.cpp capacity. Query concurrency keeps its own default.
- `ZVEC_GREP_LLAMA_THREADS_PER_CONTEXT` sets **CPU threads per context** in both
  Direct and Server mode, for indexing and queries. The default is
  `max(1, floor(available_logical_cpus / 2 / pool_capacity))`.
  In Server mode, capacity is the startup limit, not an individual request's C;
  requests never change thread configuration. GPU contexts always use one native
  CPU helper thread, including when this CPU override is set. Invalid environment
  values warn and use defaults. Restart the Server to change either setting.
- The CPU compute budget is capacity times threads per context. With more contexts
  than half the available CPUs, the one-thread minimum exceeds that half-CPU target.
  This is not a limit on all process threads or an ORT-style shared compute pool.
  Resident weights/KV across models are not bounded by the execution-slot limit.
- Contexts share model weights, are created on demand, and release native buffers
  after 30 idle seconds. Qwen KV capacity follows each job's token requirements,
  preserving the catalog's maximum input length.
- CPU, Metal and CUDA process one complete input per native job. An embedding
  batch dispatches its inputs across the available contexts. Cancellation/error
  paths drain submitted work before returning its slots.

## Remote authorization

The public [authorization module](src/authorization.rs) supports consent
preflight independently of engine operations. Inspecting or granting consent
does not load models, open index storage or send remote requests.

API credentials and permission to send data are separate. CLI, daemon and MCP
check the same workspace grants for the effective destination on each operation;
revocation affects new operations without restarting the daemon. Grant scope
and credential persistence are described under
[workspace consent](src/workspace/README.md#credentials-and-consent).

Single-operation consent covers that operation and its synchronous refresh,
not later watcher jobs. Background work needs consent covering its own scope.
[MCP setup and consent](../zg-transport-mcp/README.md#setup-and-consent) preserve
this boundary when setup submits a background build.

## Modules

| Module | Responsibility |
| --- | --- |
| [API](src/api/mod.rs) and [service](src/service.rs) | Public options and results; operation dispatch behind `ZvecGrep`. |
| [Domain](src/domain/README.md) | defines shared data types for workspaces, sources, content, entities, and metadata. |
| [File selection](src/file_selection/README.md) | Scan/watch policies and query filters. |
| [Extraction](src/extraction/mod.rs) and [lexical search](src/lexical/mod.rs) | Source-preserving extraction and direct ripgrep search. |
| [Pipelines](src/pipelines/README.md) | Indexing, indexed retrieval, and direct search; [ranking](src/pipelines/indexed_search/ranking/README.md) combines retrieval evidence. |
| [Models](src/models/mod.rs) | Model catalogs, embedding backends, downloads, and reusable runtimes. |
| [Storage](src/storage/mod.rs) and [workspace](src/workspace/README.md) | Collection schemas and operations; workspace discovery, manifests, locks, and publication. |
| [Authorization](src/authorization.rs) and [config](src/config.rs) | Remote-service consent and configuration resolution. |
| [Errors](src/error.rs) and [utilities](src/utils/mod.rs) | Diagnostics and shared text, encoding, hashing, and filesystem helpers. |
