# Public MCP search presentation fixtures

`search-presentation.json` captures text returned by the Node.js public MCP
handler, using fixed search results rather than network models or a live index.
The cases cover both preview modes, long and Unicode lines, CRLF and trailing
blank lines, matched windows, half-open Rust ranges, query groups, selection
reasons, optional outlines, metadata, stale results and empty results.

Regenerate from the repository root after intentional Node.js contract changes:

```sh
npm run build
node rust/scripts/capture-mcp-search-presentation.mjs
```

The capture script converts field names and half-open ranges to the equivalent
Node.js representation. It obtains expected text through an actual MCP client
and handler. Fixtures store relative paths and no credentials or volatile IDs.
Review regenerated text instead of accepting output changes automatically.

Rust loads the cases through `zg-testkit`. Tests compare exact text and exercise
the actual public tool handler in `agent` and `full`, checking default/short/full
preview and identical requests to the engine. No Node.js process is needed to
run the captured Rust tests:

```sh
cargo test --manifest-path rust/Cargo.toml -p zg-transport-mcp
```

Configure the native library path as for the other Rust workspace tests.
The legacy provider's unverified freshness is covered separately under
`mcp-unverified-current-index-freshness` in `allowed-differences.toml`.

Fixtures explicitly declare the source range of each item's content; the capture
script does not infer it from text length. Node.js still uses a line-count heuristic
in its formatter. Rust follows the engine's `content_range` instead, including when
an EOF fragment retains a final empty line. Separate regression cases cover that
known oracle bug, whole-entity content, and both preview modes under
`engine-content-source-coordinates` in `allowed-differences.toml`.

`rg-presentation.json` captures eight responses from the real Node MCP rg handler:
context, long unbounded output, explicit head truncation, empty results, symbols,
overlapping symbol matches, redundant declarations and multiple files. Regenerate
with `node rust/scripts/capture-mcp-rg-presentation.mjs` after `npm run build`.
`rg_format::tests::captured_node_rg_presentation` compares the text verbatim.

The regular `cargo test --workspace --all-targets` entry point also runs:

- MCP input mapping, native type selection, scan diagnostics, preview and metadata;
- real duplex protocol consent, progress and cancellation tests;
- daemon session admission/idle expiry and stdio failure-tolerance tests;
- actual daemon HTTP discovery, tools/list, validation, origin checks and remote
  consent continuation for `2026-07-28`;
- actual legacy HTTP and new/legacy stdio remote consent, index lifecycle, runtime
  refresh and direct/server source-coordinate regressions.

These tests are part of the existing Linux/Windows/macOS Rust CI matrix. They use
local HTTP fixtures, temporary roots and mock embedding responses. Standalone test
runs in a restricted filesystem should set `ZVEC_GREP_WORKSPACE_REGISTRY` to a
writable temporary file. Deferred MCP-01 remains separate: these focused cases do
not claim to be a complete snapshot of every public Node schema and response.
