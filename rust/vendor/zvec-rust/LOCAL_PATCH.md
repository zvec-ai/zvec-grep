# Local zvec-rust patch

This directory vendors the safe wrapper from the `zvec-rust` 0.7.1 crates.io
release. Its recorded upstream revision is
[`5b0a30e8a74e09c3fa85d3c52790a12a6a100763`](https://github.com/zvec-ai/zvec-rust/tree/5b0a30e8a74e09c3fa85d3c52790a12a6a100763/zvec).
The Apache 2.0 license is copied from that revision's repository root.

The only library patch adds `SearchQuery::scalar(topk)`, exposing the native
query constructor without setting a vector, FTS payload, or target field.
It uses the existing C ABI and preserves all existing Rust APIs. Handle
ownership is established before fallible setup to free it on failure.

`tests/scalar_query_test.rs` exercises scalar path lookup, projection, and the
result limit on a collection containing only scalar fields. Run it from the
repository root with:

```sh
cargo test --manifest-path vendor/zvec-rust/Cargo.toml --test scalar_query_test
```

The upstream build script and `links` metadata are retained for native library
resolution and Jieba dictionary discovery.

The package manifest retains the upstream library configuration and pins its
existing runtime dependency to `zvec-rust-sys = 0.7.1`. Upstream examples,
benchmarks, and integration tests are omitted,
with their manifest entries and unused development dependencies removed.
`zvec-rust-sys` and its native library continue to come from crates.io.

Remove the patch override and this directory when the published wrapper
provides an equivalent scalar constructor.
