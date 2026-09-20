# zvec-rust

[![CI](https://github.com/zvec-ai/zvec-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/zvec-ai/zvec-rust/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/zvec-rust.svg)](https://crates.io/crates/zvec-rust)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

English | [中文](README_CN.md)

Safe, idiomatic Rust bindings for the [zvec](https://github.com/alibaba/zvec) vector database.

## Features

- **RAII Resource Management** — All C resources are automatically freed via `Drop`
- **Builder Pattern** — Fluent APIs for schema, query, and configuration
- **Type Safety** — Rust enums for all C constants with compile-time checks
- **Comprehensive Error Handling** — All FFI calls return `Result<T>` with detailed error codes
- **Zero-Copy Where Possible** — Minimizes data copying across the FFI boundary
- **Prebuilt Libraries** — Automatically downloads prebuilt `libzvec_c_api` from GitHub Releases; advanced users can override with `ZVEC_LIB_DIR`
- **FTS with jieba Out of the Box** — Prebuilt packages ship the cppjieba dictionary (`data/jieba_dict`); `initialize` auto-discovers it and registers the default jieba dict dir for the `jieba` FTS tokenizer

## Supported Platforms

| Platform | Architecture | CI Status | Notes |
|----------|-------------|-----------|-------|
| **macOS** | ARM64 (Apple Silicon) | ✅ Clippy + Test | Primary development platform |
| **macOS** | x86_64 (Intel) | ✅ Clippy + Test | |
| **Linux** | x86_64 | ✅ Clippy + Test + Fuzz + Coverage + Benchmark | Full CI coverage |
| **Linux** | ARM64 (AArch64) | ✅ Clippy + Test + Fuzz + Coverage | |
| **Linux (musl)** | x86_64 | ✅ Prebuilt | Alpine/musl distros; built in `musllinux_1_2`, same baseline as upstream zvec |
| **Linux (musl)** | ARM64 (AArch64) | ✅ Prebuilt | Alpine/musl distros; built in `musllinux_1_2` |
| **Windows** | x86_64 (MSVC) | ✅ Clippy + Test | CMake + MSVC toolchain |

> Linux gnu prebuilts target glibc 2.28 (`manylinux_2_28`, same as upstream
> zvec), so they work on glibc distros as old as CentOS 8 / Ubuntu 20.04.
> musl prebuilts work on musl distros such as Alpine (musl 1.2+).

> The dynamic library name varies by platform: `libzvec_c_api.dylib` (macOS), `libzvec_c_api.so` (Linux), `zvec_c_api.dll` (Windows).

## Architecture

```
zvec-rust/
├── zvec-sys/    # Low-level FFI bindings to libzvec_c_api
├── zvec/        # Safe, high-level Rust wrapper
└── fuzz/        # Fuzz testing targets
```

- **`zvec-rust-sys`** — Raw `extern "C"` declarations, opaque pointer types, and constants
- **`zvec-rust`** — Safe wrappers with RAII, builders, iterators, and idiomatic Rust APIs

## Prerequisites

The Rust SDK depends on the zvec C library (`libzvec_c_api`). Choose one of the following ways to provide it:

### Option 1: Bundled Prebuilt Library (Zero Setup)

Add `zvec-rust` to your `Cargo.toml`. The default `bundled` feature automatically downloads the prebuilt `libzvec_c_api` for your platform from [GitHub Releases](https://github.com/zvec-ai/zvec-rust/releases):

```toml
[dependencies]
zvec-rust = "0.7.1"
```

`cargo run` / `cargo test` work out of the box because Cargo passes the
resolved library directory to the linker for you. A **directly executed** or
**deployed** binary, however, needs a runtime search path (`rpath`) pointing at
the shared library — Cargo does not add one automatically. Use the
[`zvec-rust-build`](https://crates.io/crates/zvec-rust-build) build-script
helper from your binary crate to emit the `rpath` and stage the library beside
the executable:

```toml
[dependencies]
zvec-rust = "0.7.1"

[build-dependencies]
zvec-rust-build = "0.7.1"
```

```rust
// build.rs
fn main() {
    zvec_rust_build::configure();
}
```

Without the helper you must instead set `DYLD_LIBRARY_PATH` (macOS) /
`LD_LIBRARY_PATH` (Linux) at runtime.

### Option 2: Custom Build

If you want to build the zvec C library yourself (e.g., for a custom configuration or unsupported platform), set the `ZVEC_LIB_DIR` environment variable:

```bash
# Build zvec from source
git clone https://github.com/alibaba/zvec.git && cd zvec
mkdir -p build && cd build
cmake .. -DCMAKE_BUILD_TYPE=Release -DBUILD_C_BINDINGS=ON
make -j$(nproc)

# Point zvec-rust to your custom build
export ZVEC_LIB_DIR=/path/to/zvec/build/lib
```

Or use the built-in Makefile for local development:

```bash
make setup        # Install dev tools + init git submodule
make zvec-build   # Build the zvec C library from submodule
make test-all     # Run all tests
```

### Library Resolution Order

The build script resolves the C library in this order:

1. **`ZVEC_LIB_DIR`** environment variable (highest priority)
2. **Sibling checkout**: `../zvec/build/lib`
3. **Git submodule**: `vendor/zvec/build/lib`
4. **Vendor directory**: `vendor/lib/`
5. **Prebuilt download**: from GitHub Releases (automatic)
6. **Auto-build**: clone and build from source (requires `git`, `cmake`, C++17 compiler)

Set `ZVEC_AUTO_BUILD=0` to disable steps 5 and 6.

## Quick Start

```rust
use zvec_rust::*;

fn main() -> zvec_rust::Result<()> {
    // 1. Initialize the engine
    initialize(None)?;

    // 2. Define schema
    let schema = CollectionSchema::builder("my_collection")
        // Note 1: use `?` to unwrap the Result returned by FieldSchema::new
        .add_field(FieldSchema::new("id", DataType::String, false, 0)?)
        // Note 2: use `?` to unwrap the Result returned by IndexParams::hnsw
        .add_vector_field(
            "embedding",
            DataType::VectorFp32,
            128,
            IndexParams::hnsw(MetricType::Cosine, 16, 200)?
        )
        .build()?;

    println!("Schema built successfully.");

    // 3. Create and open the collection (the directory will be created if it does not exist)
    // "./data" is the local storage path
    let collection = Collection::create_and_open("./data", &schema, None)?;

    println!(" Collection opened.");

    // 4. Insert data
    let mut doc = Doc::new()?;
    doc.set_pk("doc1");
    doc.add_string("id", "doc1")?;
    // Build a 128-dim vector filled with 0.1
    let vec_data = vec![0.1_f32; 128];
    doc.add_vector_f32("embedding", &vec_data)?;

    // `insert` accepts a slice of &[&Doc]
    collection.insert(&[&doc])?;

    println!("Document inserted.");

    // 5. Vector similarity search
    // Query vector: filled with 0.2
    let query_vec = vec![0.2_f32; 128];
    let query = SearchQuery::new("embedding", &query_vec, 10)?;

    let results = collection.query(&query)?;

    println!("Search Results:");
    for result in &results {
        let pk = result.get_pk().unwrap_or("unknown");

        let score = result.get_score();
        println!("   PK: {}, Score: {:.4}", pk, score);
    }

    // 6. Shutdown the engine
    shutdown()?;

    println!("Test Finished!");
    Ok(())
}
```

## Examples

Run any example with `cargo run --example <name>`:

| Example | Description |
|---|---|
| `basic` | End-to-end workflow: schema → insert → query → fetch → delete |
| `schema_builder` | Various schema configurations: field types, index types, quantization |
| `vector_search` | Vector query patterns: simple, builder, filter, output fields, HNSW params |
| `crud_operations` | Full CRUD: insert, fetch, update, upsert, delete, stats, flush |
| `config_logging` | Library configuration: memory limits, thread counts, logging |

```bash
# Using cargo directly (requires ZVEC_LIB_DIR / DYLD_LIBRARY_PATH)
export DYLD_LIBRARY_PATH=vendor/zvec/build/lib

cargo run --example basic
cargo run --example vector_search
```

## API Overview

### Initialization

| Function | Description |
|---|---|
| `initialize(config)` | Initialize the library (call once); pass `None` for defaults |
| `shutdown()` | Release all resources |
| `version()` | Get version string |
| `is_initialized()` | Check initialization status |
| `set_default_jieba_dict_dir(dir)` | Set the process-wide default jieba dict dir for the FTS `jieba` tokenizer |
| `get_default_jieba_dict_dir()` | Get the current default jieba dict dir (`""` when unset) |

Use [`ConfigBuilder`](zvec/src/config.rs) to customize memory limits, thread counts, and logging:

```rust
let config = ConfigBuilder::new()
    .memory_limit(1024 * 1024 * 1024)
    .num_threads(4)
    .enable_console_log(true)
    .build();
initialize(Some(&config))?;
```

#### jieba FTS tokenizer dictionary

The `jieba` FTS tokenizer needs cppjieba's dictionary files (`jieba.dict.utf8`
and `hmm_model.utf8`). The prebuilt libraries ship them under `data/jieba_dict`
next to `libzvec_c_api`, and `initialize` automatically discovers that
directory and registers it via `zvec_set_default_jieba_dict_dir` — Chinese
full-text search works with no extra configuration:

```rust
initialize(None)?; // jieba dict auto-discovered and registered

let schema = CollectionSchema::builder("articles")
    .add_field(FieldSchema::new("id", DataType::String, false, 0)?)
    .add_indexed_field("content", DataType::String,
        IndexParams::fts(Some("jieba"), None, None)?)
    .build()?;
```

To override the location (e.g. a custom dictionary), use one of:

- `ConfigBuilder::new().jieba_dict_dir("/path/to/jieba_dict")` — per `initialize` call (highest priority)
- `set_default_jieba_dict_dir("/path/to/jieba_dict")` — process-wide default
- `ZVEC_JIEBA_DICT_DIR` environment variable — read by the zvec library at query time
- per-field `extra_params` JSON with `"jieba_dict_dir"` — per index

### Schema Definition

```rust
let schema = CollectionSchema::builder("name")
    .add_field(FieldSchema::new("field", DataType::String, false, 0)?)
    .add_vector_field("vec", DataType::VectorFp32, 128,
        IndexParams::hnsw(MetricType::Cosine, 16, 200)?)
    .build()?;
```

### Collection Operations

| Method | Description |
|---|---|
| `Collection::create_and_open()` | Create a new collection |
| `Collection::open()` | Open an existing collection |
| `collection.insert(&docs)` | Insert documents |
| `collection.update(&docs)` | Update documents |
| `collection.upsert(&docs)` | Insert or update |
| `collection.delete(&pks)` | Delete by primary keys |
| `collection.delete_by_filter(filter)` | Delete documents matching a filter expression |
| `collection.query(&query)` | Vector similarity search |
| `collection.multi_query(&query)` | Multi-route search with RRF / weighted rerank |
| `collection.fetch(&pks)` | Fetch by primary keys |
| `collection.fetch_with_options(&pks, fields, include_vector)` | Fetch with output-field control |
| `collection.iter()` / `iter_with_options(fields, include_vector)` | Iterate over all documents (isolated snapshot) |
| `collection.create_index(field, params)` / `drop_index(field)` | Runtime index management |
| `collection.optimize()` | Rebuild indexes / merge segments |
| `collection.stats()` | Get collection statistics |
| `collection.flush()` | Flush to disk |

### Document Operations

```rust
let mut doc = Doc::new()?;
doc.set_pk("my_pk");
doc.add_string("name", "value")?;
doc.add_i64("count", 42)?;
doc.add_vector_f32("embedding", &[0.1, 0.2, 0.3])?;

// Getters return `Result<Option<T>>` — `?` only unwraps the Result.
// Use `unwrap_or_default()` / `expect(..)` etc. to handle the Option.
let name: Option<String> = doc.get_string("name")?;
let count: Option<i64> = doc.get_i64("count")?;
```

### Vector Query

```rust
// Simple query
let query = SearchQuery::new("embedding", &query_vec, 10)?;

// Builder pattern with filters
let query = SearchQuery::builder()
    .field_name("embedding")
    .vector(&query_vec)
    .topk(10)
    .filter("category = 'tech'")
    .output_fields(&["id", "name"])
    .build()?;
```

### Multi-Query (Hybrid Search)

`MultiQuery` combines multiple sub-queries (dense vector, sparse vector, or FTS) with RRF or weighted reranking:

```rust
// FTS + vector hybrid search with RRF reranking
let mut sub_vec = SubQuery::new()?;
sub_vec.set_field_name("embedding")?;
sub_vec.set_query_vector(&query_vec)?;
sub_vec.set_num_candidates(50)?;

let mut fts = Fts::new()?;
fts.set_match_string("Rust vector database")?;

let mut sub_fts = SubQuery::new()?;
sub_fts.set_field_name("content")?;
sub_fts.set_fts(&fts)?;
sub_fts.set_num_candidates(50)?;

let mut mq = MultiQuery::new()?;
mq.set_topk(10)?;
mq.set_rerank_rrf(60)?;   // rank constant for RRF
mq.add_sub_query(&sub_vec)?;
mq.add_sub_query(&sub_fts)?;

let results = collection.multi_query(&mq)?;

// Weighted reranking
mq.set_rerank_weighted(&[0.7, 0.3])?;  // weights per sub-query
```

## Supported Types

| Category | Types |
|---|---|
| **Scalar** | `Bool`, `Int32`, `Int64`, `Uint32`, `Uint64`, `Float`, `Double`, `String`, `Binary` |
| **Vector** | `VectorFp16`, `VectorFp32`, `VectorFp64`, `VectorInt4`, `VectorInt8`, `VectorInt16`, `VectorBinary32`, `VectorBinary64` |
| **Sparse** | `SparseVectorFp16`, `SparseVectorFp32` |
| **Array** | `ArrayBool`, `ArrayInt32`, `ArrayInt64`, `ArrayUint32`, `ArrayUint64`, `ArrayFloat`, `ArrayDouble`, `ArrayString`, `ArrayBinary` |

## Index Types

Available distance metrics: `L2`, `Ip`, `Cosine`, `MipsL2`.

| Type | Constructor | Description |
|---|---|---|
| HNSW | `IndexParams::hnsw(metric, m, ef)` | Graph index (recommended) |
| HNSW+Q | `IndexParams::hnsw_with_quantize(...)` | HNSW with quantization |
| IVF | `IndexParams::ivf(metric, nlist, niters, soar)` | Inverted file index |
| IVF RaBitQ | `IndexParams::ivf_rabitq(metric, nlist, total_bits, sample_count)` | IVF with RaBitQ quantization |
| Flat | `IndexParams::flat(metric)` | Brute-force index |
| DiskANN | `IndexParams::diskann(metric, max_degree, list_size, pq_chunk_num)` | Disk-based graph index for large datasets (Linux x86_64 only) |
| Invert | `IndexParams::invert(range, wildcard)` | Scalar field index |
| FTS | `IndexParams::fts(tokenizer, filters, extra)` | Full-text search index |

## Testing

```bash
# Using Makefile (recommended — auto-detects library paths)
make test-unit         # Unit tests (no C library required)
make test-integration  # Integration tests (requires C library)
make test-all          # All tests (unit + integration + doc)

# Using cargo directly (requires ZVEC_LIB_DIR / DYLD_LIBRARY_PATH)
cargo test --lib
cargo test --test integration_test

# Fuzz tests (requires nightly)
cargo install cargo-fuzz
cargo +nightly fuzz run fuzz_types -- -max_total_time=60

# Benchmarks
make bench

# Code coverage
cargo install cargo-llvm-cov
./scripts/coverage.sh --html
```

## Keeping in Sync with zvec Core

This SDK tracks the [zvec](https://github.com/alibaba/zvec) C-API. When the upstream C-API changes:

1. Update `zvec-sys/src/lib.rs` with new FFI declarations
2. Add safe wrappers in the `zvec` crate
3. Update integration tests to cover new functionality
4. Run the full test suite to verify compatibility

The CI pipeline automatically clones the latest zvec and builds the C library, ensuring FFI compatibility on every PR.

## Contributing

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/amazing-feature`)
3. Ensure all tests pass (`cargo test`)
4. Ensure code is formatted (`cargo fmt --all -- --check`)
5. Ensure clippy is clean (`cargo clippy --workspace --all-targets -- -D warnings`)
6. Submit a pull request

## License

Apache-2.0 — see [LICENSE](LICENSE).
