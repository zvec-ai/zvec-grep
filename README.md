# zvec-grep Rust rewrite

This branch contains the Rust implementation of zvec-grep. The application API
is centered on one reusable `ZvecGrep` engine value:

```rust,ignore
use zg_engine::{
    ZvecGrep,
    api::context::ContextOptions,
};

let zg = ZvecGrep::new();
let reply = zg.context(ContextOptions {
    root: Some(root.into()),
    rg: true,
    query: Some("needle".to_owned()),
    ..ContextOptions::default()
}).await?;
zg.close();
```

`ZvecGrep` is normally shared for the lifetime of a process. Workspace root is
request state, so the same instance can serve multiple workspaces. It exposes
typed `context`, `index`, `info`, and `drop_index` methods. It
calls its private services directly; there is no public command dispatcher,
operation envelope, adapter registry or Core layer between a method and its
implementation.

Request and reply types are grouped under the matching method name in
`zg_engine::api` (`context`, `index`, and `info`). Each group exposes its primary
`Options` and `Result` types directly and keeps secondary types under `options`,
`result`, or `progress`.

The engine owns a private process-level model runtime manager. Workspaces using
the same model configuration share one runtime and its loaded weights/tokenizer;
embedding calls may execute concurrently against those shared resources.
`IndexOptions::on_progress` accepts an in-process `IndexProgressReporter` and
surfaces model downloads through `IndexProgress::embedding`. The reporter is
runtime-only and is omitted from serialized daemon requests.

`zg query --rg`, workspace discovery, `info`, and idempotent `drop_index` are
wired end to end. Indexed search and indexing are assembled around the private
storage SPI; they become available to the public engine once a concrete storage
factory is installed in the production composition root.

Lexical search runs in-process with ripgrep's `grep` and `ignore` crates; the
binary and ordinary CI jobs do not require a system `rg` executable.

## Remote embedding authorization

In an interactive terminal, `zg index` prompts before sending data to an
unauthorized remote embedding destination. Choose `1. Allow once`,
`2. Allow for this workspace`, or `3. Cancel`. Direct and server modes show the
same prompt; server mode resolves the destination and saves workspace consent
using the server's configuration. Invalid input, EOF, or cancellation stops
before indexing. Existing consent and `--allow-remote` skip the prompt.
Non-interactive commands require existing consent or `--allow-remote`.

You can also authorize a workspace explicitly:

```sh
zg auth grant /path/to/workspace --capability embedding --scope workspace --embedding qwen/text-embedding-v4
zg auth status /path/to/workspace
zg auth revoke /path/to/workspace
```

Granting consent does not send data, load a model, or build an index. Subsequent
remote indexing and semantic search may send workspace content and query text
to the selected endpoint and incur provider charges. API credentials remain
separate: use `ZVEC_GREP_API_KEY` or `--api-key` for actual operations.

Without `--embedding`, grant uses the existing index model, then
`ZVEC_GREP_EMBEDDING`. `--endpoint` overrides the stored index endpoint,
`ZVEC_GREP_ENDPOINT`, and provider default. The signed grant at
`.zvec-grep/authorization.json` binds the canonical workspace root, model, and
endpoint. Changing any of them requires a new grant. The signing key lives at
`$ZVEC_GREP_HOME/authorization.key` (default `~/.zvec-grep/authorization.key`),
or the path selected by `ZVEC_GREP_AUTHORIZATION_KEY_FILE`.

Direct CLI, server CLI, and MCP read the same authorization on each operation;
configure them to use the same signing key. Revocation takes effect for new
operations without restarting the server. `--allow-remote` on `zg index` or
`zg query` grants consent for that operation only, including its synchronous
refresh, and never authorizes later watcher jobs. MCP callers use workspace
grants; interactive consent elicitation is not implemented in this Rust version.

## Crates

- `zg-engine`: `ZvecGrep`, engine errors, and method-grouped types under `api`;
  lexical search, source extraction and embedding model implementations are
  private to this crate.
- `zg-cli`: CLI parsing and terminal rendering.
- `zg`: production binary.
- `zg-daemon`: process lifecycle, loopback HTTP server and stdio bootstrap.
- `zg-transport-mcp`: MCP schemas and direct translation to typed `ZvecGrep`
  calls.
- `zg-daemon-protocol`: daemon-only wire DTOs. These types are not part of the
  in-process engine API.
- `zg-host-native`: standalone native scanner and watcher implementation.
- `zg-testkit`: compatibility fixture readers.

Run the complete local gate with:

```sh
bash scripts/check.sh
```

## Local npm installation

Build the release binary with the same script name used by the TypeScript
project:

```sh
npm run build
```

Stage only the current native platform and link the local package into the
active npm global prefix:

```sh
npm run install:local
zg --version
```

The local install remains linked to `dist/npm/zvec-grep-local`; rerun the command
after cleaning `dist/`. To generate a self-contained tarball without installing
it:

```sh
npm run pack:local
```

Generated packages are written under `dist/npm/`. Run the isolated install
smoke test with:

```sh
npm run test:package
```

Pass `--no-build` after `--` to reuse an existing `target/release/zg`, or pass
`--prefix <path>` to install or smoke-test under a custom npm prefix:

```sh
npm run install:local -- --no-build --prefix /tmp/zvec-grep-npm
```

## npm release packaging

The registry distribution is prepared as one meta package plus exact-version
native optional dependencies declared in `npm/platforms.json`. The meta package
uses Node only during `postinstall` to verify and materialize the native
executable; running `zg` enters that executable directly. Published installs
support Node.js 14.14 and newer.

Validate the release manifest and build the current platform dry-run packages:

```sh
npm run release:verify
npm run release:pack
```

The release tarballs are written under `dist/npm/release/tarballs/`. Run a real
two-tarball install in an isolated npm prefix and verify the materialized binary
with:

```sh
npm run release:smoke
```

CI can package a previously built target artifact without rebuilding it:

```sh
node scripts/npm-release.mjs pack-platform \
  --target linux-x64-gnu \
  --binary /path/to/zg \
  --lib-dir /path/to/runtime-libs \
  --no-build
```

All native platform packages must be published and smoke-tested before the meta
package. The release script intentionally does not run `npm publish` or move a
dist-tag.
