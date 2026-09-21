# zvec-grep Rust rewrite

This directory contains the Rust implementation of zvec-grep. The TypeScript /
Node.js implementation remains at the repository root; the [main README](../README.md)
and [user documentation](../docs/README.md) describe that implementation.

Run the development commands below from `rust/` (`cd rust` from the repository
root). See the [contributor guide](CONTRIBUTING.md) for checks and conventions.

This workspace preserves its own crates, scripts, compatibility fixtures, and
benchmarks. The application API is centered on one reusable `ZvecGrep` engine
value:

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

`zg install --target opencode` respects a nonempty `OPENCODE_CONFIG` override.
Otherwise it selects an existing `opencode.jsonc` before `opencode.json` under
`${XDG_CONFIG_HOME:-~/.config}/opencode`, creating `opencode.json` when neither
exists. Installation reports the selected path and explains when both files
exist. JSONC comments, trailing commas, unrelated settings and other MCP entries
are preserved. Uninstall removes managed entries from both global files, or only
from the explicit override, and removes managed guidance from the adjacent
`AGENTS.md`.

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
Idle model runtimes expire 15 minutes after their final lease is released, even
without another request. The cache has a soft capacity of one runtime: it evicts
the least recently used idle models on acquisition and final release, while
active models may temporarily exceed the limit. Reacquiring a model resets its
idle period. Maintenance stops when the engine is closed or its last owner is
dropped, and native model destruction runs outside the cache lock.

`IndexOptions::on_progress` accepts an in-process `IndexProgressReporter` and
surfaces model downloads through `IndexProgress::embedding`. The reporter is
runtime-only and is omitted from serialized daemon requests.

The daemon enables an index read-session cache with a 60-second idle timeout.
Sequential and concurrent queries reuse the same generation's native storage
handles. Each request still reads current workspace metadata and resolves its
own model settings. Standalone engine instances can opt in with
`ZvecGrep::enable_read_session_cache()`; `close()` retires the cache while
in-flight queries retain their handles until completion.

Native open and close run under a per-workspace slot; the cache map lock only
manages entries, so a cold open or retirement cannot block another workspace's
cache hit.

Workspace reads and writes queue asynchronously. A cross-process writer-intent
lock prevents new reads from overtaking waiting writers while active readers
drain. Writers then retire cached handles before opening writable storage.
`ContextOptions::lock_timeout_ms` and `IndexOptions::lock_timeout_ms` bound lock
waits (30 seconds by default), including idle-cache draining; their cancellation
tokens interrupt waiting. Cache maintenance checks for writers every 50 milliseconds,
independently of the async executor. Cancelled, timed-out and aborted waiters
release admission without leaving pending writer markers.

Within one engine, queries using `Off` or `Background` refresh may borrow an active
incremental writer when the effective model configuration matches (including
credentials, endpoint, device and model cache). Borrowed queries keep storage,
models and the home lock alive; publication waits for them to finish. `Wait` and
legacy synchronous auto-update queries never borrow partial writer state. A
rebuild's unpublished generation remains private. The daemon preserves the refresh
policy when invoking the engine and also bounds cancellable waits for scheduled jobs.

The daemon retires workspace runtimes and watchers after four hours without a
foreground operation. Maintenance runs once per minute and does not renew the
idle deadline. Active queries, inspections, watcher setup, queued indexing and
running indexing prevent retirement; background changes and job completion do
not restart the four-hour timer. Retirement forgets the workspace's finished job
history and prevents old callbacks from restarting its watcher. A later request
can activate a new runtime without deleting the persisted index. Model runtimes
remain shared at engine scope and follow their own lease lifetime.

The native engine supports indexing, indexed FTS and vector search, `zg query
--rg`, workspace discovery, `info`, and idempotent `drop_index`.

This version indexes text with one embedding model per workspace. Choose it with
`zg index --embedding <model>` or set a default with
`zg config model set <model> --default`. Code, documents and structured text use
that same model. Images and other unsupported sources are reported as skipped;
multimodal content and per-content model routing are not supported. Changing the
model requires `zg index --rebuild --embedding <model>`.

Each entity stores one complete source content object, its location in the source
file, metadata, and its fragments. Each text fragment has a unique ID and selects
either full content or UTF-8 byte offsets relative to the entity's content. Source
locations, including line and column numbers, are calculated when needed;
fragments do not duplicate content or source coordinates.
Model collections store the text and vectors needed for retrieval, with entity
metadata included in search projections. Index format 5 uses this layout.

Lexical search runs in-process with ripgrep's `grep` and `ignore` crates; the
binary and ordinary CI jobs do not require a system `rg` executable.

Managed `zg query --rg` preserves literal pattern whitespace and supports empty
patterns with `-e ''`. Matching options include fixed strings (`-F`), case
selection (`-i`, `-s`, `-S`, with the last option winning), word/whole-line
matching (`-w`, `-x`), inversion (`-v`), multiline search (`-U`),
`--multiline-dotall`, `--crlf`, and text search through NUL bytes (`-a`).
Case-sensitive and insensitive glob rules (`-g`, `--iglob`) retain command-line
order. `-m` bounds matches per file and reports truncated coverage when additional
matches are omitted; `--limit` bounds the final result list. `-j 0` selects the
automatic thread count. Ignore controls include `-u`/`-uu`/`-uuu`,
`--no-ignore-dot`, `--no-ignore-files`, `--no-ignore-global`,
`--no-ignore-parent`, and `--no-ignore-vcs`. Managed search still excludes its
internal directories. The full MCP toolset uses the same argument parser and
engine. PCRE2, compressed-file search, explicit encoding selection, and native
ripgrep output-format switches are rejected; Unicode BOM decoding is automatic.

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
`ZVEC_GREP_EMBEDDING`, then the configured default model. `--endpoint` overrides
the stored index endpoint, per-model endpoint configuration, `ZVEC_GREP_ENDPOINT`,
and provider default. The signed grant at
`.zvec-grep/authorization.json` binds the canonical workspace root, model, and
endpoint. Changing any of them requires a new grant. The signing key lives at
`$ZVEC_GREP_HOME/authorization.key` (default `~/.zvec-grep/authorization.key`),
or the path selected by `ZVEC_GREP_AUTHORIZATION_KEY_FILE`.

Prepare the outer directory before initializing a custom signing-key location.
For example, with `ZVEC_GREP_HOME=/data/apps/zg`, `/data/apps` must already exist;
the engine creates `zg`. For an explicit key-file path, the engine can create its
direct parent directory, but that directory's parent must already exist.

Direct CLI, server CLI, and MCP read the same authorization on each operation;
configure them to use the same signing key. Revocation takes effect for new
operations without restarting the server. `--allow-remote` on `zg index` or
`zg query` grants consent for that operation only, including its synchronous
refresh, and never authorizes later watcher jobs. MCP reuses existing workspace
grants and otherwise asks form-capable clients for explicit consent before index
or search sends data remotely. The form discloses the root, source roots, model,
endpoint and data categories. Choose `once` for this operation, `workspace` for a
signed persistent grant, or `cancel` to deny transmission. Search also offers
`fts_only`, which disables vector retrieval and refresh. Unsupported clients,
invalid responses, declined forms, cancellation and timeouts never grant consent.
Cancelling the originating request also sends a cancellation notification for its
pending consent form; clients control how that notification is presented.

Daemon freshness checks reuse successful reconciliation proofs while the watcher
is active and all observed revisions are indexed. `Wait` drains delivered watcher
events and queued successors; concurrent waiters share one refresh. A clean
`Background` query reports `idle` without submitting another job. Initial watcher
installation, recovery, overflow, rejected batches, and failed indexing require a
full reconciliation. Proofs retain their original revision and recovery epoch so
an older completion cannot clear newer changes; partial file failures are never
cached as fresh. Native `flush_pending` drains delivered events without forcing a
scan, while explicit `flush` and periodic/recovery checks retain full rescans.
Explicit `info` inspection still reads current disk status.

## MCP request lifecycle

Search accepts `preview: "short"` (the default) or `preview: "full"` in both
toolsets. Short preview shows up to ten source lines around the matched range
and seven available outline lines. Full preview preserves all available source
and outline content of each retrieved item; it neither reads whole files nor
changes retrieval or ranking. Responses include query groups, selection reasons,
matched ranges, source line numbers, and available code or Markdown metadata.
The current engine supplies source snapshots without generated outlines; the
optional outline is displayed only when provided with a result.

The engine provides `content_range` for the exact source coordinates of returned
content, independently of the entity `range` and matched `excerpt_range`. CLI and
MCP use these coordinates for source numbering; preview never infers a range from
the number of content lines. The engine also interprets half-open line bounds.
The required field changes the internal daemon reply contract (version 12).
Restart older resident daemons when updating the CLI; replies without
`content_range` are rejected during deserialization. Direct and server rendering
consume the same engine contract.

Public search reports `freshness: fresh` or `freshness: possibly_stale`.
`results: served_from_current_index` describes where results came from, with
`background_refresh` reported separately. A provider that only reports current
index provenance without verified freshness is conservatively shown as
`possibly_stale`; a successful waited refresh reports `fresh`.

Both HTTP and stdio expose the same tools. Search accepts `device`; index accepts
`debug: true` to return completed statistics, timings and at most 100 skipped files.
Use `wait: true` to obtain these diagnostics in the index response; background
submissions return job state instead of pretending that indexing has completed.

Index and search requests carrying `_meta.progressToken` receive coalesced MCP
progress notifications during indexing, synchronous refresh and model download.
The progress number is a monotonic event sequence; `message` contains the engine's
JSON progress snapshot, including phase and file/download counters.

Cancellation stops waiting and signals request-owned work cooperatively. An
exclusive synchronous index job can be cancelled; shared jobs, watcher refreshes
and already-submitted `wait: false` jobs continue under daemon ownership. Native
operations already executing may finish their current non-interruptible step.

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

The build uses the published `zvec-rust` and `zvec-rust-build` 0.7.2 crates,
without a local wrapper patch. Their bundled native library tracks upstream zvec
at [`1ab7975`](https://github.com/alibaba/zvec/commit/1ab7975dfc2d2160054bafff614831b7099cd930)
(53 commits after v0.7.0). The build stages the shared library and
`data/jieba_dict` resources beside the executable. Both local and release
packages include these SDK assets; the engine uses the bundled dictionary.

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

The runtime library directory must include zvec's shared library and its
`data/jieba_dict` directory. Without `--lib-dir`, these assets are read from
the binary's directory.

All native platform packages must be published and smoke-tested before the meta
package. The release script intentionally does not run `npm publish` or move a
dist-tag.
