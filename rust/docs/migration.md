# Preparing the workspace for the repository root

This checklist tracks remaining work before removing the legacy TypeScript
implementation and moving the native workspace to the repository root.
Documentation organization does not complete that migration.

## Intended documentation layout

The current workspace layout can move as a unit:

```text
README.md
CONTRIBUTING.md
ARCHITECTURE.md
docs/
  README.md
  runtime/
  development/
  migration.md
crates/                   # Keep module documentation next to the code.
compat/                   # Keep regression fixtures and difference records.
npm/                      # Keep distribution and published-package instructions.
standalone/               # Keep installer instructions.
benchmarks/               # Keep protocols, datasets and recorded evidence.
micro-bench/              # Keep performance contracts.
```

The topic guides use workspace-relative links. Root entry documents, workflow
links and commands explicitly depending on the current repository layout need
a separate review during the move.

## Dependencies to resolve

| Area | Current dependency | Required action |
| --- | --- | --- |
| Root entry documents | The repository already has README translations and a contributor guide. | Merge the product-facing introduction, translations and links with the native workspace entry points. Do not blindly overwrite them. |
| Root build files | The repository root and `rust/` both have npm manifests, scripts and build outputs. | Reconcile their responsibilities before moving files; remove legacy runtime scripts and retain native packaging tools. |
| Native package license | [npm-release.mjs](../scripts/npm-release.mjs) reads the project license from `../LICENSE` relative to the workspace. | Resolve it from the new workspace root and verify every release package retains the license and upstream notices. |
| Legacy fixture capture | `scripts/capture-*.mjs` imports repository-root `dist/` modules; CLI capture also uses root test fixtures. | Capture any required baseline while the old implementation is available. Retire or replace the old capture commands when deleting it. |
| Runtime regression data | [Compatibility fixtures](../compat/README.md) and `zg-testkit` preserve accepted behavior and differences. | Keep fixtures consumed by Rust tests. Remove obsolete comparison tooling separately from the data it produced. |
| CI and release paths | Repository workflows/actions refer to `rust/`, its Cargo files, scripts, target directories and package outputs. | Update working directories, path filters, cache keys, mounts, upload/download paths and links together. |
| Smoke-test input | Native CI and local package smoke search the README for `zvec-grep Rust workspace`. | Keep that text until the smoke input and its callers are updated together. |
| Benchmark trees | Both the root and native workspace contain `benchmarks/`; setup commands and local package paths depend on that layout. | Compare runners, data and protocols, select the retained tree, and revalidate package paths. Preserve published reports as evidence, without treating them as current implementation documentation. |
| Public documentation | User documentation lives in the separate documentation site. | Update source links, installation guidance and runtime descriptions when the repository move lands. |

## Node.js scope

Removing the legacy runtime is separate from removing Node.js from the toolchain:

- The native CLI executes without Node.js; standalone installers also work
  without Node/npm on the user's machine.
- [package.json](../package.json) still uses Node for native build orchestration,
  packaging, publication and installer tests. [check.sh](../scripts/check.sh)
  includes those tests.
- The npm meta package uses Node during installation to verify and materialize
  the native executable and handle plugins and supported integration upgrades.
- Legacy comparison capture scripts need the old implementation, not merely
  Node. They must be retired or replaced when that implementation is deleted.

## Completion checks for the actual move

- [ ] Merge root documents and resolve duplicate build/benchmark files.
- [ ] Update path-sensitive scripts, workflows, packaging inputs and documentation links.
- [ ] Keep captured compatibility data; remove obsolete capture imports and instructions.
- [ ] Run ordinary native checks on Linux, Windows and macOS.
- [ ] Verify Direct/Server behavior and keep intentional differences explicit.
- [ ] Validate native npm and standalone candidates, including install, upgrade and recovery.
- [ ] Verify the resulting root has no unintended dependency on the deleted runtime.
