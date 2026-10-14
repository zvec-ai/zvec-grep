# zg-engine

`zg-engine` is the Rust library for indexing files and retrieving content through lexical and vector search.

## Usage

Reuse one `ZvecGrep` instance across requests and workspaces. Call its async methods from a Tokio runtime. This example indexes the current directory using the configured embedding model, then searches it:

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

Use `IndexOptions::embedding` and `embedding_routes` to select models. `ContextOptions::query_image` accepts image input, and `target_kind` limits the content kind searched. The engine also provides `info` for workspace status and `drop_index` to remove an index. See the [Rust README](../../README.md) for build setup.

## Behavior

**Source files.** Source files remain untouched. The engine indexes content extracted from supported files. Users can optionally configure scan rules to select files for indexing and query filters to narrow results.

**Workspace data.** The default data directory is `<source>/.zvec-grep`. Set `index_dir` on index, context, info and drop options to select another complete data directory. Its manifest stores `sourceRoot` relative to that directory; existing-index queries resolve the saved relationship rather than replacing it with the request's discovery root. Explicit directories never fall back to another index. The data directory holds `manifest.json`, `index/`, temporary build state and locks. Close writers and watchers before copying it together with the source files, preserving the relative layout. Reopening then resolves the destination source root automatically. Different Windows drives cannot form this relative relationship and are rejected. Changing the layout requires explicitly indexing the new source root to repair the relationship. Indexed queries fail when that source is unavailable; `rg` searches remain independent of index configuration. Relative file paths use portable components, and filenames must be representable without collisions on the destination filesystem. Source files can be read-only; the data directory must be writable. This directory can also contain encrypted saved credentials, whose decryption key travels with it; remote consent still requires authorization on the destination machine.

**Shared configuration and models.** Global defaults and credentials come from `~/.zvec-grep/config.json`; `workspaces.json` is an optional workspace discovery cache. Local models download missing files on first use. On macOS and Linux, their default cache is `~/.zvec-grep/models`; set `ZVEC_GREP_MODEL_CACHE` to change it. Device and model cache settings are local runtime choices (request, environment, local configuration, then default); they are not saved in the index. Remote models send content to their configured service and require credentials and authorization.

**Model routing.** Each enabled content kind uses one embedding model, selected through a workspace default and optional overrides. Content unsupported by the default model is skipped; an unsupported explicit route is an error.

**Search.** Text and code support full-text and vector search; images support vector search. Queries can target one content kind or search all enabled kinds supported for that input. Results contain paths and previews, with separate ranking groups where needed. The result limit applies across groups.

**Index lifecycle.** Repeated indexing updates changed files, reuses unchanged files, and removes deleted files. Failed files are reported and excluded from search until successfully indexed. Changing an index's embedding model or using an incompatible index format requires an explicit rebuild. A cancelled or aborted rebuild preserves the previous index. Dropping an index removes workspace settings and index data while preserving source files and shared model caches.

## Modules

The source code is organized into the following modules:

- **API** defines public request and result types.
- **Service** connects engine capabilities behind `ZvecGrep`.
- **Domain** defines shared data types for workspaces, sources, content, entities, and metadata.
- **File selection** compiles domain glob rules and supplies engine-owned scan/watch policies.
- **Extraction** turns source files into complete entities, metadata, and source-preserving fragment ranges.
- **Lexical** searches source files directly with embedded grep.
- **Models** provides embedding backends and manages model runtimes.
- **Storage** groups persistent data into `directories`, `files`, `entities`, and `fragments` modules. Each module owns its collection schema, encoding, and table operations. `IndexStore` coordinates their lifecycle, file write states, and cross-table reads and writes; `zvec.rs` contains shared native helpers.
- **Workspace** manages workspace names, configuration, index locations, and locks.
- **Pipelines** coordinates index construction (`indexing`), indexed queries (`indexed_search`), and direct queries (`direct_search`). Direct queries combine lexical search with structural extraction and result assembly.
- **Authorization** manages consent for sending data to remote embedding services.
- **Config** manages global settings and resolves runtime configuration.
- **Error** defines engine errors and diagnostic reports.
- **Utils** provides shared helpers for text, encoding, hashing, and filesystem operations.
