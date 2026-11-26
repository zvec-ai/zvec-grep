# Documentation map

These documents describe the current native implementation. Start with the
[workspace README](../README.md) or the [contributor guide](../CONTRIBUTING.md).
End-user tutorials live on [zvec.org](https://zvec.org/en/docs/zvec-grep/).

## Development and delivery

| Document | Scope |
| --- | --- |
| [Contributing](../CONTRIBUTING.md) | Development workflow, API conventions and ordinary checks |
| [Architecture](../ARCHITECTURE.md) | Public boundaries, dependency direction and crate responsibilities |
| [Native builds](development/building.md) | Platform prerequisites, first Cargo build, executable paths and optional CUDA |
| [Validation](development/validation.md) | Which checks to run and where to find failure evidence |
| [Releasing](development/releasing.md) | Native installer candidates, verification and GitHub Release publication |
| [Migration checklist](migration.md) | Remaining dependencies before removing the old implementation and moving this workspace |

## Documentation kept with its owner

| Location | Purpose |
| --- | --- |
| [Engine](../crates/zg-engine/README.md) | Public API, model/resource ownership, authorization and internal module navigation |
| [Workspace](../crates/zg-engine/src/workspace/README.md) | Persistence, read/write coordination, relocation and consent records |
| [Daemon](../crates/zg-daemon/README.md) | Resident resources, refresh, job lifetime and execution-mode differences |
| [MCP transport](../crates/zg-transport-mcp/README.md) | Tool boundaries, setup, consent, progress and cancellation |
| [Native host](../crates/zg-host-native/README.md) | Scanner and watcher contracts |
| [Compatibility fixtures](../compat/README.md) and [MCP fixtures](../compat/mcp/README.md) | Test data, regeneration and accepted differences |
| [npm distribution](../npm/README.md) | Package layout, runtime support and installation policy |
| [Published package README](../npm/meta/README.md) | Installation and recovery instructions shipped to npm users |
| [Package compliance](../npm/compliance/README.md) | Optional supplemental licenses and SBOM inputs |
| [Standalone installers](../standalone/README.md) | Install, upgrade, uninstall and recovery |
| [Benchmarks](../benchmarks/README.md) | Retrieval evaluation protocols and recorded results |
| [Model micro-benchmark](../micro-bench/model/README.md) | Reproducible model performance measurement |

## Scope and maintenance

- Keep the workspace README short. Put build, validation and release procedures
  under `docs/development/`; keep design constraints with the responsible module.
- Keep user-facing command and integration instructions in the user documentation
  and CLI help. Do not maintain a second option reference or a prose inventory of
  implementation constants in this repository.
- Keep crate/module READMEs beside the code. Keep package READMEs, fixture
  instructions, benchmark prompts, datasets and result reports beside their
  consumers; they are not general development notes.
- Document current behavior and supported recovery/upgrade paths. Remove
  completed iteration narratives and duplicated CI descriptions. Git history
  retains previous designs; captured test fixtures remain executable evidence.
- Development commands run from the workspace directory, currently `rust/`,
  unless a document explicitly says repository root. Paths inside commands
  are relative to that execution directory, not the Markdown file.
- Use relative document links within the workspace so the documentation can
  move with it. Repository-level workflows and legacy implementation references
  are tracked in the migration checklist.
