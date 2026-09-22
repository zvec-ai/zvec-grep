# Models Module Guardrails

These instructions apply to `zg-engine/src/models/**`.

## Boundary

- Keep `models` private to `zg-engine`. User-facing model selection belongs in the
  engine API and domain configuration; do not expose backend, artifact, or runtime
  implementation types from `lib.rs`.
- Match `main`'s observable behavior where parity is required, but use Rust-native
  ownership and concurrency instead of copying the Node.js implementation shape.
- Do not add compatibility aliases or dead-code allowances for unreleased Rust APIs.

## Layout

- `mod.rs`: module declarations and deliberate crate-private re-exports only.
- `spi.rs`: backend-neutral embedding contracts and request-scoped options.
- `error.rs`: structured model failures and conversion to engine errors.
- `catalog/`: immutable model metadata and reference resolution. Keep backend
  execution and network access out of the catalog.
- `artifacts/`: download, verification, locking, cache identity, and atomic
  publication. Backends must not implement separate download/cache paths.
- `backends/<backend>/`: one backend implementation. Keep `mod.rs` declarative,
  production code in focused files, and unit tests in a sibling `tests.rs` or
  `*/tests.rs`.
- `backends/factory.rs`: the only switch that maps catalog entries to concrete
  backend implementations.
- `runtime/`: process-level model reuse, leases, concurrency admission, shared
  compute resources, and eviction. Pipelines must use runtime leases rather than
  concrete backends.
- `tests/`: cross-backend contracts, main-oracle fixtures, and manual benchmarks;
  backend-specific tests stay with the backend.
- Add a directory when introducing a distinct responsibility; do not grow a new
  flat collection of unrelated `models/*.rs` files.

## Runtime and Backend Rules

- The runtime owns model identity, reuse, and operation concurrency. A backend may
  own resources scoped to one cached runtime, but must honor the runtime-provided
  execution budget and must not create an independent process-global scheduler.
- Preparation must be explicit, cancellable, and idempotent for one runtime.
  Indexing invokes it only immediately before the first real embedding batch, so
  empty or unchanged operations do not download or load a model.
- Preserve cancellation and progress through every layer, including artifact
  acquisition and backend initialization.
- Use structured error metadata for retry and failure-scope decisions. Do not parse
  display strings to control retries, fallback, or fail-fast behavior.
- Retry only transient failures with a bounded budget. Shared terminal failures
  such as authentication, missing models, incompatible dimensions, and local
  download/load failures must stop the operation; request-specific content errors
  may use targeted batch-to-item fallback.
- Never log, persist, hash as identity, or include in `Debug` raw credentials.

## Verification

- Every behavior change needs deterministic unit or integration coverage for the
  success path, cancellation, and relevant failure/recovery path.
- Concurrency tests must assert calls/resource ownership, not wall-clock timing.
- Artifact tests must use isolated temporary caches and cover atomic publication;
  do not depend on a developer's model cache.
- Tests requiring public model downloads or special hardware must be explicitly
  ignored with prerequisites documented. Keep at least one enabled packaged-model
  smoke path in CI once that infrastructure exists.
- Run formatting, `cargo clippy -p zg-engine --all-targets -- -D warnings`, and the
  affected `zg-engine` tests before handoff.
