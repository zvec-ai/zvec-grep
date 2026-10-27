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

A workspace is a directory of files that zg indexes and searches as a unit. Each workspace is identified by its canonical root path and has its own scanning rules, model settings, and index.

### Index storage

Workspace indexes and saved settings use a configurable storage location, defaulting to a `.zvec-grep` directory within the workspace. Downloaded models are cached separately and shared across workspaces. Moving or copying the source root together with `.zvec-grep` preserves the index.

### File selection

Scanning respects ignore rules, excludes hidden files, does not follow symlinks, and traverses nested Git repositories by default. File formats inferred from names, path patterns, additional ignore files, depth limits, and file size limits let you adjust the scope. These rules are saved with the workspace and reused for future indexing.

### Index lifecycle

An index is created on the first indexing run and updated as files are added, changed, or removed. It can be rebuilt from scratch when needed, or deleted without removing source files or shared model caches. Indexing can complete with individual file failures, which are reported separately.

### Search

Indexed search combines full-text and vector retrieval to return relevant source snippets, refreshing the index as needed by default. Query filters narrow results without changing the workspace's scanning rules. Direct text search reads source files without requiring an index.

### Vector storage

Embedding vectors are stored as IEEE binary16. The engine keeps embeddings and query vectors as `f32` until they are narrowed for the FP16 field. `indexVersion` remains 2. Existing FP32 indexes require an explicit rebuild with `zg --index --rebuild` before use.

### Embedding models

Each workspace uses an embedding model for semantic search. Local models run on your machine and may need an initial download; remote models send content or queries to an external service and require authorization. Changing the model or encountering an incompatible index format requires rebuilding the index.

## Modules

| Module | Responsibility |
| --- | --- |
| [API](src/api/mod.rs) and [service](src/service.rs) | Public options and results; operation dispatch behind `ZvecGrep`. |
| [Domain](src/domain/README.md) | defines shared data types for workspaces, sources, content, entities, and metadata. |
| [File selection](src/file_selection/README.md) | Scan/watch policies and query filters. |
| [Extraction](src/extraction/mod.rs) and [lexical search](src/lexical/mod.rs) | Source-preserving extraction and direct ripgrep search. |
| [Pipelines](src/pipelines/README.md) | Indexing, indexed retrieval, and direct search; [ranking](src/pipelines/indexed_search/ranking/README.md) combines retrieval evidence. |
| [Models](src/models/mod.rs) | Model catalogs, embedding backends, downloads, and reusable runtimes. |
| [Storage](src/storage/mod.rs) and [workspace](src/workspace/README.md) | Collection schemas and operations; manifests, root paths, locks, and publication. |
| [Authorization](src/authorization.rs) and [config](src/config.rs) | Remote-service consent and configuration resolution. |
| [Errors](src/error.rs) and [utilities](src/utils/mod.rs) | Diagnostics and shared text, encoding, hashing, and filesystem helpers. |
