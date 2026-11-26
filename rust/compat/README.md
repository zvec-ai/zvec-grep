# Compatibility fixtures

This directory holds captured regression data consumed by Rust tests.
Current Rust APIs and tests define the native interface; captured TypeScript
output remains a baseline where compatibility is required. Direct/Server
differences are tracked separately from TypeScript/Rust differences.

## Fixture rules

1. For behavior covered by a legacy oracle, capture its case before changing
   that behavior. Review intentional differences explicitly.
2. Use repository-relative fixture roots and stable placeholders; never record a
   developer's absolute home path, token, API key, timing or random ID.
3. Normalize path separators, volatile IDs and timings in the differential
   runner rather than editing expected output after each run.
4. Every accepted Direct/Server or TypeScript/Rust difference must reference an
   ID in [mode-differences.toml](mode-differences.toml) or
   [allowed-differences.toml](allowed-differences.toml), respectively.
5. Rust tests load fixtures through `zg-testkit`; adapter white-box tests do not
   redefine compatibility semantics.

## Current layout

| Path | Contents |
| --- | --- |
| [cli/](cli/) | Managed-rg, path lookup and server option precedence cases |
| [cli-search-presentation.json](cli-search-presentation.json) | Captured CLI result presentation |
| [mcp/](mcp/README.md) | Search/rg presentation and toolset fixtures |
| [schema/](schema/) | Versioned CLI fixture schemas |
| [mode-differences.toml](mode-differences.toml) | Accepted Direct/Server differences |
| [allowed-differences.toml](allowed-differences.toml) | Accepted TypeScript/Rust differences |

## Capture and runtime dependencies

The `scripts/capture-*.mjs` tools import the legacy implementation's build
output. Run them from the repository root after its `npm run build`; see the
[MCP fixture guide](mcp/README.md) for exact commands. Running Rust tests against
captured data does not require the legacy runtime.

Keep fixtures used by regression tests when deleting the old implementation.
Retire or replace capture tools and their instructions separately, as recorded
in the [migration checklist](../docs/migration.md).
