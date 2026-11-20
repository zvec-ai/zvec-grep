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

### Search

Indexed search combines full-text and vector retrieval to return relevant source snippets. Index refresh follows the caller's policy; resident execution can schedule background refresh. Query filters narrow results without changing the workspace's scanning rules. Direct text search reads source files without requiring an index.

### Vector storage

Embedding vectors are stored as IEEE binary16. The engine keeps embeddings and query vectors as `f32` until they are narrowed for the FP16 field. `indexVersion` remains 2. Existing FP32 indexes require an explicit rebuild with `zg --index --rebuild` before use.

### Embedding models

Each workspace uses an embedding model for semantic search. Local models run on your machine and may need an initial download; remote models send content or queries to an external service and require authorization. Changing the model or encountering an incompatible index format requires rebuilding the index.

Model selection runs at the shared engine boundary for Direct, Server and MCP.
An existing compatible index keeps its stored model unless a replacement is
explicitly requested. Resolve the final selection after acquiring the writer
lock so an index published while a request was queued takes precedence over
the caller's default.

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
