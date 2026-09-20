# Rust rewrite contributor guide

## Start

```sh
bash scripts/check.sh
cargo run -p zg -- query --rg needle .
```

The workspace pins its Rust toolchain in `rust-toolchain.toml`. Lexical search
uses embedded `grep` and `ignore` crates, so a system `rg` executable is not
required for builds or tests.

## Engine changes

Keep the application surface centered on `ZvecGrep`:

- add a typed method or extend its request/reply types;
- implement behavior in a private service module;
- call that service directly from `ZvecGrep`;
- return `EngineError` without wrapping the result in a generic command outcome.

Do not add a generic `Core`, command bus, operation envelope, adapter registry or
transport executor to connect an in-process method to its implementation.

## Native and transport changes

Native dependency types remain in their owning crate. Daemon framing and wire
commands remain in `zg-daemon-protocol`; MCP schemas remain in
`zg-transport-mcp`. Transport handlers call public `ZvecGrep` methods directly.

If a private engine service needs code currently living in another crate,
prefer moving that concrete implementation behind the engine boundary. Do not
make an engine-internal module public solely to avoid a crate dependency cycle.

## Compatibility

The TypeScript implementation on `origin/main` is the behavioral oracle during
the rewrite. Store stable, machine-readable cases under `compat/` and normalize
paths, random identifiers and timings in the runner.

## Verification

Run:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc -p zg-engine --no-deps
```
