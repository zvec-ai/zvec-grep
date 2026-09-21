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
