# Rust contributor guide

Run development commands from the workspace directory, currently `rust/`.
The repository root retains the legacy TypeScript implementation and its separate
checks. User documentation lives in
[zvec-web](https://github.com/zvec-ai/zvec-web/tree/main/content/docs), published
on [zvec.org](https://zvec.org/en/docs/zvec-grep/).

## Start

After entering the workspace directory:

```sh
cargo run -p zg -- --rg needle .
```

See [native builds](docs/development/building.md) for prerequisites, native
SDKs and step-by-step Cargo build instructions.

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

`zg_engine::authorization` is an explicit exception for remote-consent preflight
and workspace grant-file management. CLI, daemon and MCP callers must be able to
resolve destinations and inspect, grant or revoke consent before starting an
engine operation. These functions use workspace metadata and authorization files;
they do not require a `ZvecGrep` instance, acquire model runtimes, open index
storage or send remote requests. Keep their implementation helpers private and
return typed data for new callers; terminal rendering belongs in `zg-cli`.
The existing string-returning helpers remain for compatibility. This exception
does not extend to indexing, search or other engine operations, which continue
to use typed `ZvecGrep` methods backed by private services.

## Native and transport changes

Native dependency types remain in their owning crate. Daemon framing and wire
commands remain in `zg-daemon-protocol`; MCP schemas remain in
`zg-transport-mcp`. Transport handlers call public `ZvecGrep` methods directly.

Create a separate native crate only when the component is independently useful
or testable. Keep its library types and runtime policy in that crate. When
integrating it, reuse typed engine options and results, put orchestration behind
a private engine service, call it from `ZvecGrep`, and map failures to
`EngineError`. Add shared abstractions only for multiple real implementations.

If a private engine service needs code currently living in another crate,
prefer moving that concrete implementation behind the engine boundary. Do not
make an engine-internal module public solely to avoid a crate dependency cycle.

Check cancellation during expensive work, bound processes and queues, preserve
deterministic output order, clean up partial artifacts, and record native
revisions and artifact fingerprints where applicable.

## Compatibility

The TypeScript implementation and captured fixtures remain comparison baselines
where parity is required. Current Rust APIs and regression tests define the Rust
interface. Store stable, machine-readable cases under `compat/`, normalize paths,
random identifiers and timings in the runner, and document intentional behavior
differences. See [compatibility fixtures](compat/README.md) for the current
layout and capture rules.

## Verification

Run:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc -p zg-engine --no-deps
```

The complete local gate is `bash scripts/check.sh`; it also runs Node-based
packaging and installer tests. For broader packaging regression coverage, run:

```sh
node --test scripts/npm-*.test.mjs scripts/standalone-*.test.mjs npm/meta/install.test.cjs
```

Use [validation](docs/development/validation.md) to select local or CI checks
and find failure evidence. Use [releasing](docs/development/releasing.md) for
standalone installer candidates and GitHub Release publication.

## Documentation

Start with the [documentation map](docs/README.md). Update the document that
owns the changed behavior, and link to it instead of repeating the description
in the workspace README. Keep current contracts and operational instructions;
remove completed iteration notes when they no longer explain supported behavior.
The [migration checklist](docs/migration.md) tracks dependencies for moving the
native workspace to the repository root.
