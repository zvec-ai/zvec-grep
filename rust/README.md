# zvec-grep Rust workspace

Native CLI, reusable search engine, resident daemon and MCP transport for
zvec-grep. This workspace is currently under `rust/`; the repository root also
contains the legacy TypeScript implementation.

## Use zvec-grep

- [User documentation](https://zvec.org/en/docs/zvec-grep/): installation and everyday use.
- [Standalone installation](standalone/README.md): native bundles without Node/npm.
- [npm installation](npm/meta/README.md): native packages distributed through npm.
- [Agent integration](docs/runtime/integrations.md): configuration and launcher upgrades.

## Develop

Run development commands from the workspace directory:

```sh
cd rust
cargo run -p zg -- --rg needle .
```

Use the pinned Rust toolchain, platform build prerequisites, Python 3 and curl.
See [native builds](docs/development/building.md) for SDKs, CUDA and local packages.

```sh
bash scripts/check.sh
```

The full check script also runs packaging tests and requires Node.js.
The [contributor guide](CONTRIBUTING.md) lists individual checks and conventions.

## Documentation

| Topic | Entry point |
| --- | --- |
| Complete documentation map | [Documentation index](docs/README.md) |
| Architecture and crate boundaries | [Architecture](ARCHITECTURE.md) |
| CLI search, previews and setup | [CLI behavior](docs/runtime/cli.md) |
| Engine API and internal modules | [Engine guide](crates/zg-engine/README.md) |
| Models, concurrency and downloads | [Embedding models](docs/runtime/models.md) |
| Daemon, MCP and remote consent | [MCP](docs/runtime/mcp.md), [authorization](docs/runtime/authorization.md) |
| CI and real-runtime checks | [Validation](docs/development/validation.md) |
| Native packaging and publication | [Releasing](docs/development/releasing.md) |
| Compatibility fixtures | [Compatibility](compat/README.md) |
| Retrieval and model benchmarks | [Benchmarks](benchmarks/README.md), [model micro-benchmark](micro-bench/model/README.md) |
| Preparing to move Rust to the repository root | [Migration checklist](docs/migration.md) |

Keep detailed behavior and operational procedures in the topic guides. This
README is the workspace entry point.
