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

Index embedding concurrency is configured with
`zg --index --index-embedding-concurrency <n>`; the old
`--embedding-concurrency` spelling is rejected. For automatic indexing and
refreshes as well, set `ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY=<n>`. Values must be
positive integers. Explicit CLI/API index settings take precedence over this
environment variable, followed by the legacy
`ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM` for llama.cpp indexing, then backend defaults.
Invalid environment values warn and select the automatic default. These overrides
do not configure query-vector inference and are not persisted in workspace settings.
Direct and Server indexing use the same engine boundary; restart the Server after
changing its inherited environment, while explicit CLI overrides need no restart.

For llama.cpp, the limit controls contexts within a serialized batch and is capped
at eight, defaulting to one. Transformers/ORT limits in-flight batches to at most
eight, defaulting to one. ORT may create sessions within this budget;
CoreML retains one physical session. EmbeddingGemma2 serializes batches across its
shared ONNX sessions. Potion/model2vec defaults to two batch tasks
and retains an independent CPU worker capacity; remote models retain adaptive
request scheduling. Model2vec and remote overrides are not capped at eight.

Matching local concurrency configurations share runtime admission. Different
configurations use separate cached native resources; active leases keep their
models alive, while unused variants are released when switching configurations.

`zg --install --target opencode` respects a nonempty `OPENCODE_CONFIG` override.
Otherwise it selects an existing `opencode.jsonc` before `opencode.json` under
`${XDG_CONFIG_HOME:-~/.config}/opencode`, creating `opencode.json` when neither
exists. Installation reports the selected path and explains when both files
exist. JSONC comments, trailing commas, unrelated settings and other MCP entries
are preserved. Uninstall removes managed entries from both global files, or only
from the explicit override, and removes managed guidance from the adjacent
`AGENTS.md`.

`zg --install --target copilot` configures the user-level Copilot MCP server and
CLI instructions under `${COPILOT_HOME:-~/.copilot}`. `--target vscode` updates
every detected Stable and Insiders user profile, using `VSCODE_USER_DIR`,
`VSCODE_PORTABLE`, or `VSCODE_APPDATA` when set. It also registers the server in
Copilot's home because the shared instructions are read by both clients.
Uninstall preserves that shared server until both integrations are removed.

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

The native engine supports indexing, indexed FTS and vector search, `zg --rg`,
workspace discovery, `info`, and idempotent `drop_index`.

For isolated runs, set `ZVEC_GREP_CONFIG` to a dedicated configuration file,
`ZVEC_GREP_WORKSPACE_REGISTRY` to a dedicated registry file, `ZVEC_GREP_HOME` to a
runtime directory, and `ZVEC_GREP_MODEL_CACHE` (or `--model-cache`) to a model cache.
Workspace indexes default to each source root's `.zvec-grep/index/` directory.
`--index-dir PATH` selects the complete workspace data directory and overrides
`ZVEC_GREP_INDEX_DIR`. Rebuilds use that directory's `temp/` until publication; generation IDs remain metadata rather than directory names. Existing
model caches may be reused without changing the user's regular configuration.

On Unix, the CLI raises its own open-file soft limit toward 4096, capped by the
existing hard limit, before starting the runtime. Native index tables can exceed
macOS's common 256-descriptor default. This leaves the parent shell and system
settings unchanged; a restrictive hard limit is reported on stderr. Applications
embedding `zg-engine` manage their own process limits.

Search directly with `zg "where authentication is validated"`, `zg --fts
"AuthService"`, or `zg --rg -F "AuthService" src`. Management uses flags:
`zg --index`, `zg --status`, `zg --server`, `zg --config`, `zg --auth`,
`zg --install`, and `zg --uninstall`. Bare words such as `query`, `index`, and
`help` are literal search text, not commands. Use `zg --help [topic]` (for example,
`zg --help search`) and `zg --version` for help and version information.

The resident daemon writes JSON Lines to `<home>/daemon/logs/server.log`.
It rotates at 10 MiB and keeps five numbered backups by default. The global
`~/.zvec-grep/config.json` accepts `log.maxBytes`, `log.keep`, and `log.level`
(`info` or `debug`); restart the daemon after changing them. Bootstrap failures
are written to `<home>/daemon/bootstrap.log`.

Terminal searches default to human-readable output with full source previews;
piped output is compact. `--compact` forces compact output, and
`--preview=full` explicitly requests full previews even in compact output.
The former `--human` option is no longer accepted.

Each workspace generation has a separate retrieval table for each enabled
content kind: text, code and image. `zg --index --embedding <model>` selects the
default model; repeat `--embedding image=local/embeddinggemma-2` to override one
kind. `--embedding image=default` removes that override and inherits the workspace
default, including future changes. Omitted settings preserve saved values, and
a bare model changes only the default. Repeating the default or the same kind in
one command is an error. Unsupported default kinds are skipped; invalid explicit
model choices are errors. `--endpoint` applies to the default model; configure
other models with `zg --config model set <model> --endpoint <url>`.
Different kinds remain separate tables even when they share one model runtime.
Changing enabled tables or their model, vector dimension or metric requires
`--rebuild`. Endpoint changes, query instructions and runtime input limits do not
require rebuilding.
PNG, JPEG and static WebP are supported as standalone image sources.

Each entity stores its complete content snapshot, source location, metadata and
fragments. Text/code fragments select full content or relative UTF-8 byte ranges;
images use the complete encoded image. Source files stay unchanged. Kind tables
contain retrieval projections, with full-text and vector indexes for text/code
and only vector indexes for images. Index version remains 2; old development
schemas require an explicit rebuild and are not migrated.

Query input and target kind are independent. Use `zg --vector 'a red bicycle'
--kind image` for text-to-image retrieval, or `zg --input photo.png
--kind image` for image-to-image retrieval. `--input` reads one PNG, JPEG or static
WebP file and cannot be combined with text queries. Positional queries remain
literal text even if they name an existing file. The target table's model must
support the query's input kind. Without `--kind`, all compatible enabled targets are
searched. Text/code candidates are combined before query fusion and rule ranking.
Full-text-only queries combine both kinds using the fixed FTS configuration,
even when their embedding models differ. Native BM25 scores are compared directly;
this accepts the backend's local corpus statistics rather than claiming the
scores equal a single globally scored corpus.

Queries with vector routes combine text/code tables only when their model,
dimension, metric, fixed weights and query encoding are compatible. This also
applies to hybrid queries: full-text recall is pooled within that compatible
group. Different query instructions, such as Gemma2's text and code instructions,
keep groups separate. Each recall route is ordered across its participating
tables before fusion; rule ranking then considers the complete recalled pool,
including for single-vector text/code queries. Image results remain separately
ranked without text/code rules. Scores are not probabilities; text/code scores
include rule adjustments, while traces retain the original recall scores.

Without `--fuse`, queries are ranked separately before query-group coverage and
selection; with it, their recall routes are fused before rule ranking. The flag
does not force incompatible models into one relevance ranking. `--limit` is a
total cap from 1 to 2000 (default 30), allocated round-robin across the resulting
groups in text/code/image order. `--trace` explains targets, models, shared recall
and rule ranking; `--json` preserves paths, previews, target outcomes and result
groups. Text output shows group headings only when multiple groups return hits;
headings list the content kinds actually returned, and ranks apply within each
group. Search scope and ranking details remain available with `--trace`.
`--status --json` shows effective tables and per-kind entity counts.
Images return their path, format and encoded size. Public content-snapshot reading
is not exposed; internal entity snapshots remain part of the index.

Lexical search runs in-process with ripgrep's `grep` and `ignore` crates; the
binary and ordinary CI jobs do not require a system `rg` executable.

Managed `zg --rg` preserves literal pattern whitespace and supports empty
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

Workspace settings live in `<index-dir>/manifest.json`; `index/` holds the active
storage and rebuilds use a temporary `temp/` directory. The default is
`<root>/.zvec-grep`. The manifest records the source root relative to the index
directory, while stored file paths are relative to the source root. An explicit
`--index-dir` selects that exact index; a missing index is an error. Without an
explicit directory or `ZVEC_GREP_INDEX_DIR`, queries discover the closest default
index upwards from the working directory. CLI relative arguments, including
`--input`, resolve against the caller's working directory, independently of the
saved source root. There is no query `--root` option.

```sh
zg --index ./materials --index-dir ./indexes/interior --embedding local/embeddinggemma-2
zg "bright living room" --index-dir ./indexes/interior --kind image
zg --status --index-dir ./indexes/interior
```

Stop writers and watchers, then copy the source tree and complete index directory
while retaining their relative layout. On the destination, selecting the copied
index also resolves the copied source tree; no registration or re-embedding is
needed solely because the package moved. Source/index layouts on different Windows
drives are rejected because they cannot preserve a relative relationship.
To change that relationship explicitly, run `zg --index <new-source> --index-dir
<index>`; this synchronizes the index, including removals. Omitting the source when
updating an existing index uses its saved location. Missing or unreadable source
roots fail indexed operations instead of switching to the caller's directory.
`--rg` remains independent of the index and can still scan available files.
Direct queries default to no refresh; server queries retain background refresh.
`--refresh off` does not stop a watcher activated by an earlier operation.
A failed or incomplete scan must not be treated as an empty source tree.

No global workspace registration is required. MCP search/index/status/drop accept
`indexDir`, an absolute server-visible path, without repeating `root`; it overrides
the server's startup `ZVEC_GREP_INDEX_DIR`. Creating a new index requires `root`.
Without `indexDir`, `root` supplies the default-index discovery starting point.
Different index directories for the same source have separate
resident jobs and watcher state. One index directory cannot be concurrently
attached to different source roots. Global configuration remains an optional
source of initial defaults and credentials; model assets may still use a shared
cache or need downloading. Device and model-cache paths are machine-local: they
are not saved in the manifest and resolve from request options, environment, local
configuration, then defaults. Old development indexes require `--rebuild`; index
version remains 2 and no development-schema migration is provided.

An explicit `zg --index --api-key <key>` saves the default remote model's key,
encrypted with XChaCha20-Poly1305. The ciphertext is in the manifest and the
random encryption key is `<index-dir>/credentials.key`. Repeat the command to
replace it; an omitted key preserves the saved value. Environment and global
configuration keys are used without being copied into the workspace. A query's
`--api-key` is temporary. Failed/cancelled builds do not replace saved credentials.
Anyone holding the complete workspace can decrypt its credentials; keep this in
mind when sharing it. Dropping an index removes its saved credentials as well.

## Remote embedding authorization

In an interactive terminal, `zg --index` prompts before sending data to an
unauthorized remote embedding destination. Choose `1. Allow once`,
`2. Allow for this workspace`, or `3. Cancel`. Direct and server modes show the
same prompt; server mode resolves the destination and saves workspace consent
using the server's configuration. Invalid input, EOF, or cancellation stops
before indexing. Existing consent and `--allow-remote` skip the prompt.
Non-interactive commands require existing consent or `--allow-remote`.

You can also authorize a workspace explicitly:

```sh
zg --auth grant /path/to/workspace --capability embedding --scope workspace --embedding qwen/text-embedding-v4
zg --auth status /path/to/workspace
zg --auth revoke /path/to/workspace
```

Granting consent does not send data, load a model, or build an index. Subsequent
remote indexing and semantic search may send workspace content and query text
to the selected endpoint and incur provider charges. API credentials remain
separate: use `ZVEC_GREP_API_KEY` or `--api-key` for actual operations.

Without `--embedding`, grant uses the existing index model, then
`ZVEC_GREP_EMBEDDING`, then the configured default model. `--endpoint` overrides
the stored index endpoint, per-model endpoint configuration, `ZVEC_GREP_ENDPOINT`,
and provider default. The signed grant at
`<index-dir>/authorization.json` binds the canonical workspace root, model, and
endpoint. Changing any of them requires a new grant. The signing key lives at
`$ZVEC_GREP_HOME/authorization.key` (default `~/.zvec-grep/authorization.key`),
or the path selected by `ZVEC_GREP_AUTHORIZATION_KEY_FILE`. This signing key stays
on the machine. Copying a workspace retains its saved API keys but requires fresh
remote consent on another machine; an unknown signer is treated as unapproved.
Without a global home, use `--allow-remote` for consent on that invocation.

Prepare the outer directory before initializing a custom signing-key location.
For example, with `ZVEC_GREP_HOME=/data/apps/zg`, `/data/apps` must already exist;
the engine creates `zg`. For an explicit key-file path, the engine can create its
direct parent directory, but that directory's parent must already exist.

Direct CLI, server CLI, and MCP read the same authorization on each operation;
configure them to use the same signing key. Revocation takes effect for new
operations without restarting the server. `--allow-remote` on `zg --index` or
`zg <query>` grants consent for that operation only, including its synchronous
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

For protocol `2026-07-28`, remote consent uses `input_required`. Echo the exact
`requestState` and unchanged tool arguments with
`inputResponses.remote_embedding_authorization = {"action":"accept","content":{"decision":"allow_once"}}`.
Other choices are `allow_workspace`, `use_local_search` (search only), and `cancel`.
The opaque state is kept in bounded daemon memory, expires after ten minutes,
binds the caller, tool, arguments and disclosed targets, and is consumed once.
Restarting the daemon invalidates pending prompts; request a new prompt. Older
clients retain reverse form elicitation. Consent is rechecked before execution.

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
The required field changes the internal daemon reply contract. Version 13 also
added native ripgrep type filters; these are now query-only. Version 14
separates case-sensitive and case-insensitive glob updates on the daemon wire.
Version 15 distinguishes complete ordered glob replacement from category updates.
Restart older resident daemons when updating the CLI; replies without
`content_range` are rejected during deserialization. Direct and server rendering
consume the same engine contract.

Public search reports `freshness: fresh` or `freshness: possibly_stale`.
`results: served_from_current_index` describes where results came from, with
`background_refresh` reported separately. A provider that only reports current
index provenance without verified freshness is conservatively shown as
`possibly_stale`; a successful waited refresh reports `fresh`.

Search and index accept `globs` and `insensitiveGlobs` as a string or list;
case-insensitive string rules follow case-sensitive string rules. Index string
parameters update only the supplied case category. Typed glob lists and CLI glob
arguments replace the complete ordered list. Search-only `fileTypes` and
`excludedFileTypes` use the embedded ripgrep catalog, including `h`, `cpp`, `ts`
and `py`, independently of extractor `formats`. Index uses globs for file
selection and also accepts `follow`.
Search never changes persisted scan policy: `includeHidden`, `useDefaultIgnores`, `ignoreFiles`,
`maxDepth`, `maxFileSizeBytes` and `follow` belong on index requests. Node's
indexed-search path ignores its corresponding scan options; Rust rejects them explicitly.
`includeHidden` allows hidden files and directories and corresponds to CLI `--hidden`.
`useDefaultIgnores` defaults to true. Setting it to false disables built-in and
discovered ignore rules; explicit `ignoreFiles` remain active. CLI `--no-ignore`
sets the same policy to false, and `--no-ignore=false` restores it to true.
Unknown search/index fields and public nulls are rejected, except index
`maxDepth: null` / `maxFileSizeBytes: null`, which clear saved limits.
Existing typed globs, `formats/categories`, `enum`, `name`, `nestedGit`, diagnostic
extensions and their compatibility boundaries are recorded in
[`allowed-differences.toml`](compat/allowed-differences.toml).

Managed rg uses a file/line/context presentation and preserves all requested
matches and context. Only an explicit trailing `head` bounds output, with an
omission notice. Commands are parsed as arguments and never passed to a shell.
Legacy HTTP sessions are capped at 256 and expire after 30 idle minutes; active
request streams are protected. Discovery and tools/list advertise a one-hour
private cache lifetime. The stdio bridge tolerates unreadable daemon state while
the connected process is alive, and closes when that process exits or a new
instance replaces it.

Both HTTP and stdio expose the same tools. Public search rejects per-request
`device` and `apiKey` overrides. Index accepts `device` and `debug: true` to return
completed statistics, timings and at most 100 skipped files. Use `wait: true` to
obtain these diagnostics in the index response; background submissions return job
state instead of pretending that indexing has completed.

Index and search requests carrying `_meta.progressToken` receive coalesced MCP
progress notifications during indexing, synchronous refresh and model download.
The progress number is a monotonic event sequence; `message` describes the phase
and file/download counters in readable text. Structured counters remain in status.

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

The `Publish Rust packages` workflow builds all five published native targets on matching
GitHub runners. Pushes to `main` or `release/rust-*` branches and manual runs
produce downloadable candidates without publishing to npm. Each runner installs
its platform and meta tarballs, then runs `zg --version`, checks managed-rg,
starts the packaged daemon, compares workspace status in direct and server
modes, and stops the daemon. The server status request crosses the IPC boundary;
managed-rg intentionally stays local. macOS binaries receive ad hoc signatures;
Developer ID, notarization, and Windows certificate signing are not required for
candidate builds.

Intel macOS is built and verified first. Only after its tarball installation,
CLI startup, managed-rg, and daemon IPC checks pass do the other four native
builds start. The final artifact is collected after all five targets pass.

Linux candidates are built and smoke-tested on Ubuntu 24.04. The current ONNX
Runtime prebuilt libraries cannot link against Ubuntu 22.04's glibc and C++
runtime, so older Linux distributions are not covered by this release workflow.
Intel macOS builds ONNX Runtime 1.28.0 with CoreML and the native zvec SDK from
pinned source revisions because neither dependency supplies prebuilt Intel
macOS libraries. Both SDKs are cached and their shared libraries are included
in the npm payload. The Intel release targets macOS 14 and later.

Download the `rust-release-<version>` artifact from the successful workflow run
and unzip it. It contains all five platform tarballs, the meta tarball,
`SHA256SUMS`, `release.json` with the source commit, and platform-specific install
commands. For example, on an ARM64 Mac:

```sh
npm install --global --allow-scripts=./zvec-zvec-grep-0.0.1.tgz ./zvec-zvec-grep-darwin-arm64-0.0.1.tgz ./zvec-zvec-grep-0.0.1.tgz
zg --version
zg --rg -F "zvec-grep" README.md
```

Use the filenames from the downloaded artifact when the version changes. On
Windows, use `zg.cmd` if PowerShell blocks the generated `zg.ps1` shim. Artifacts
are retained for 14 days.

To publish, update the Rust workspace version, tag the release commit on `main`
as `rust-v<version>`, and manually run the workflow with `publish` enabled. The
publishing repository must have an npm trusted publisher configured for all six
packages with both **Allow npm publish** and the independent **Allow npm
dist-tag** permission enabled, or provide an eligible `NPM_TOKEN` secret.
Publishing and promotion install npm 11.21.0 explicitly for OIDC dist-tag support.

Registry publication is currently blocked until reviewed license texts and
SPDX SBOMs are supplied for every target in [`npm/compliance/`](npm/compliance/README.md).
Candidate downloads remain available while these inputs are prepared.
Every publication uses `rust-canary`, letting users opt into Rust with
`npm install --global @zvec/zvec-grep@rust-canary`. Five native runners then
install only the meta package from the registry, test actual daemon IPC, and
exercise update and rollback with a distinct previously published version.
Selecting `latest` requests a separate promotion job that changes the default
installation only after all registry checks pass.

`release:collect` requires all platform tarballs and packs the meta package last.
`release:publish` validates the collected files before publishing native packages
and then the meta package under `rust-canary` with npm provenance. An already published version is
reused only when its tarball integrity matches, allowing a partial publication
to be retried without replacing a version. `release:promote` only moves tags for
an already published identical release and runs after registry validation in the
workflow. Ordinary build and packaging commands
do not publish or move dist-tags.
