# zvec-grep

`zvec-grep` is a local code and document search engine. It combines full-text
search with semantic vector search and keeps its index inside the workspace.

This branch is a clean Rust rewrite of the beta TypeScript implementation. It
does not preserve the old API or index format.

## Workspace

```text
crates/
├── engine/   standalone indexing and search library
├── server/   server and client library (next layer)
└── cli/      command-line application (next layer)
```

The engine has no dependency on the server or CLI. The server depends on the
engine, and the CLI may use either one.

## Documentation

- [Design and principles](docs/design.md) describes the durable architecture
  and the reasoning behind it.
- [Rust rewrite implementation plan](docs/implementation-plan.md) is the
  temporary, developer-oriented plan for completing the rewrite.

## Engine today

The engine is functional as a library and includes:

- Git-aware workspace scanning, include/exclude globs, hidden-file policy,
  dependency/build-directory pruning, and file-size limits.
- Stable file identities and incremental add/modify/delete detection.
- Syntax-aware extraction for Rust, TypeScript/TSX, JavaScript, Python, Go,
  Java, C, and C++, with line-based fallback.
- Markdown section extraction that understands fenced code blocks.
- Batched embedding and batched persistent writes with progress and cooperative
  cancellation.
- A replaceable embedding interface, a deterministic test/offline embedder,
  and an optional batched llama.cpp GGUF embedder.
- Persistent Zvec vector and FTS indexes, plus an in-memory implementation for
  tests and embedding the engine elsewhere.
- Lexical, vector, and hybrid RRF search; path, format, time, and symbol
  filters; adaptive recall; and score/rank evidence.
- Exhaustive lexical search over live files without creating an index.
- Per-file indexing/search failures and engine status.

The server and CLI crates intentionally remain minimal while the engine API is
stabilized.

## Build and test

The repository pins Rust 1.85.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The optional llama.cpp backend builds on common desktop platforms:

```sh
cargo check -p zvec-grep-engine --features llama-cpp
```

On macOS, `--features metal` enables the llama.cpp Metal build.

## Library example

```rust,no_run
use std::sync::Arc;

use zvec_grep_engine::{
    DeterministicEmbedder, Embedder, Engine, EngineConfig, IndexRequest,
    SearchRequest, WorkspaceConfig,
};

fn main() -> zvec_grep_engine::Result<()> {
    let workspace = WorkspaceConfig::new(".");
    let embedder: Arc<dyn Embedder> = Arc::new(DeterministicEmbedder::new(384)?);
    let engine = Engine::persistent(EngineConfig::new(workspace), embedder)?;

    engine.index(IndexRequest::default())?;
    for item in engine.search(SearchRequest::new("incremental indexing"))?.items {
        println!("{}: {:?}", item.segment.relative_path.display(), item.segment.location);
    }
    Ok(())
}
```

`DeterministicEmbedder` is useful for tests and a dependency-free fallback. For
real semantic search, enable `llama-cpp` and construct `LlamaEmbedder` with a
small embedding GGUF. The original `all-MiniLM-L6-v2` safetensors/ONNX files
cannot be loaded directly by llama.cpp; use an equivalent GGUF export. Its
input limit defaults conservatively to 900 characters for the usual 256-token
MiniLM context and can be configured for a particular model.

An ignored runtime smoke test can validate a local GGUF:

```sh
ZVEC_GREP_TEST_MODEL=/path/to/all-MiniLM-L6-v2-Q8_0.gguf \
  cargo test -p zvec-grep-engine --features llama-cpp \
  llama_minilm_runtime_smoke_test -- --ignored
```

## Index layout

Persistent indexes live at:

```text
<workspace>/.zvec-grep/index/
├── manifest.json
└── segments/       Zvec collection
```

The manifest records the embedding model and dimension. Opening an index with
a different model fails clearly instead of silently mixing incompatible
vectors. `Engine::persistent_rebuild` explicitly discards that derived index
when changing models. A workspace lock prevents two processes from opening the
same persistent collection at once; a long-running server can therefore be the
single owner used by multiple clients.

## License

Apache-2.0. Direct dependencies use permissive licenses compatible with the
project policy; `deny.toml` defines the accepted license and source rules.
