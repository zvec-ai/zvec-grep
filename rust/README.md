# zvec-grep Rust workspace

This directory contains the native zvec-grep CLI, engine, daemon, and MCP
transport. Start with the [main README](../README.md) or the
[user documentation](https://zvec.org/en/docs/zvec-grep/) for installation and
everyday use. This page covers implementation behavior, native builds, and
packaging. The repository root retains the legacy TypeScript implementation and
its separate checks.

Run the development commands below from `rust/` (`cd rust` from the repository
root). See the [contributor guide](CONTRIBUTING.md) for checks and conventions.

The Rust CLI joins positional words into one hybrid query:
`zg connection pool timeout` is equivalent to `zg "connection pool timeout"`.
Separately quoted positional arguments also merge: both
`zg "connection pool" "timeout"` and `zg "connection pool" "socket timeout"`
produce one query. When at least two nonempty positional arguments include a
phrase with internal whitespace, a warning on stderr explains the merge and
recommends explicit queries, such as
`zg --hybrid "connection pool" --hybrid "socket timeout"`.
Shell quote characters are not available to the CLI, so separately quoted single
words behave like unquoted words without a warning. Each positional argument is
trimmed, empty arguments are ignored, and the remaining parts are joined with one
space while preserving internal whitespace. Existing scripts relying on multiple
positional query groups should switch to repeated `--hybrid` flags. Explicit
`--hybrid`, `--fts`, and `--vector` queries stay separate. Direct and Server modes
use the same parsing; managed `--rg` retains its pattern/path syntax.

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

Rg requests return plain text matches and requested context without parsing code
or Markdown structure. CLI and MCP rg searches share this behavior; current-file
path lookup keeps its structural entity grouping.

Index embedding concurrency is configured with
`zg --index --embedding-concurrency <n>`.
The option is rejected outside `--index`. For automatic indexing and
refreshes as well, set `ZVEC_GREP_EMBEDDING_CONCURRENCY=<n>`. Values must be
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
CoreML retains one physical session. Potion/model2vec defaults to two batch tasks
and retains an independent CPU worker capacity; remote models retain adaptive
request scheduling. Model2vec and remote overrides are not capped at eight.

Matching local concurrency configurations share runtime admission. Different
configurations use separate cached native resources; active leases keep their
models alive, while unused variants are released when switching configurations.

`zg --install --upgrade-installed` migrates existing user-level MCP launchers
from 0.2.x without selecting agents or installing new integrations. It changes
`zg server --stdio` to `zg --server --stdio` and replaces recognized Node/JS
entrypoints with the running native executable. Legacy `zg` launchers that specify
an executable path also switch to the running native executable, so an old npm
prefix cannot keep invoking the Node.js version. Bare `zg` commands retain PATH
lookup, and launchers already using native syntax retain their executable paths.
Known config-directory overrides are respected; project files are not searched.
HTTP connections, toolsets, timeouts, environment variables, disabled state,
permissions and guidance remain unchanged. JSONC and TOML comments are preserved.
Only existing entries are changed, including when related clients use separate
config files.

Use `zg --install --upgrade-installed --dry-run` to preview. Each changed file
gets a unique adjacent `.zg-upgrade-*.bak` backup before atomic replacement.
Repeated runs make no additional changes. Invalid files and unsupported launchers
are reported while other independent files are processed; any unresolved entry
produces a nonzero exit status. Migration does not start a daemon or load models.
Reconnect agents afterwards. HTTP users can run `zg --server on` to replace an
old daemon using the existing protocol-upgrade path.

Published npm packages attempt this migration after a successful global
installation or rebuild for the current user. Local dependencies, temporary
execution, CI, elevated Unix installs and ambiguous user homes are skipped.
Set `ZG_INSTALL_SKIP_AGENT_UPGRADE=1` to opt out. Automatic migration has a
10-second timeout; failure preserves package installation and writes details to
`agent-upgrade.log` in the installed package. The manual command remains available
for skipped or failed migrations. Packaging and release verification opt out.

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

`zg --install --target pi` configures Pi's built-in user-level MCP in
`${PI_CODING_AGENT_DIR:-~/.pi/agent}/mcp.json` and adds guidance to the active
global context file, including `AGENTS.override.md` when present. Both stdio
and HTTP use the shared daemon; new entries have direct exposure and a timeout
in seconds. Reinstall preserves Pi UI choices and existing HTTP authentication
unless an explicit token environment option replaces it. Uninstall uses a
stable ownership marker, so changing the daemon URL does not leave stale
managed entries. Use `/reload` or a new session. Native filesystem paths are
required for the directory override; file URIs and Windows shell drive paths
are rejected. Pi's built-in MCP currently needs user-managed `zg --auth grant`
authorization for Remote Embedding; see the
[integration guide](https://zvec.org/en/docs/zvec-grep/agents/).

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
rebuild's unpublished replacement remains private. The daemon preserves the refresh
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

Search and status discover the nearest enclosing `.zvec-grep` directory. Its
parent is the source directory, and the active data lives at `.zvec-grep/index`.
Moving or copying the source directory together with `.zvec-grep` preserves the
index without registration or a workspace name. The manifest records the previous
absolute source location for environment checks; it does not locate the source.

First builds and rebuilds prepare data in `.zvec-grep/temp/new`, retaining the
previous index until publication succeeds. An interrupted directory swap is
recovered before readers enter; successful publication keeps the fixed `index/`
path and clears temporary data.

Stored file paths and internal ignore-file references move with the workspace.
Missing ignore files and external paths that are not absolute on the current
platform are skipped without removing their saved references. Other read errors,
invalid rules, and invalid path components produce errors. Filename collisions
introduced by moving to a different filesystem are not detected; rebuild the
index to match the files present at the destination.
Incomplete scans do not apply deletions, cancelled file preparation preserves
existing content, and content-identical updates refresh timestamps without
embedding again.

Explicit index device and model-cache options are saved as workspace defaults;
omitted options preserve them. Each operation resolves these settings from
explicit options, environment, workspace defaults, global configuration, then
backend defaults. After moving a workspace, override incompatible preferences
with `zg --index --device cpu --model-cache <path>`; no rebuild is required.
Query overrides remain temporary and are not written back by automatic refresh
or retained by watchers.

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

The instance record at `<home>/daemon/instance.lock` carries
`daemonProtocolVersion: 1`. Status checks and daemon-backed CLI requests reject
live legacy daemons without this field, including Node.js daemons; `auto` mode
does not silently fall back to direct execution. Run `zg --server on` with the
same home and any required token file to stop the legacy daemon and start Rust.
The stdio bootstrap performs the same replacement automatically. Concurrent
starts serialize shutdown and startup. Automatic migration only requests graceful
shutdown: an unreachable endpoint, HTTP rejection, or exit timeout preserves the
process instead of sending termination signals. OS process start times are checked
against the instance record to reject reused PIDs. When changing listening addresses,
the target is checked before stopping the old service. Legacy `localhost` endpoints
are probed on both IPv4 and IPv6; ambiguous healthy endpoints block migration.
Other protocol versions are rejected without replacement;
use a compatible client or explicitly stop that daemon first. Index format
compatibility remains separate and may require `zg --index --rebuild`.

Searches default to `--preview short` in terminals, pipes, and redirected files.
Use `none` for a one-line source summary, `short` for compact bounded source,
`full` for compact full source, or `detail` for full previews with metadata,
outlines, and file/hit grouping. Short and full preserve the original Rust compact
per-hit layout, including indented `line: source` rows. Every preview preserves each
query group's results, ranks and matched content; only `--fuse` combines groups.
Query echoes, group headings, hit counts and `matchedBy` labels are omitted from
the CLI result display. Full previews do not retrieve an entire function or file.
Metadata labels use purple, file locations use cyan, and query matches use yellow.
Automatic colors are disabled for redirected output, `TERM=dumb`, or `NO_COLOR`;
`--color always` explicitly enables colors, including through pipes, and
`--color never` disables them. `--color auto` is the default; the redundant
`--no-color` option is no longer accepted.
The former `--compact` and `--human` options are no longer accepted.

When an interactive search finds no index, it asks you to choose an embedding
model before building. The picker has **Local models** and **Remote models** tabs:
downloaded local models and globally configured remote models appear first.
Use ↑/↓ to select, ←/→ or Tab to switch, Enter to confirm and Esc to exit.
Each tab keeps its selection when switching. A valid model preference selects the
initial tab; without one the picker opens Local models. Remote models show whether
credentials are available. Selecting one with missing credentials opens a masked
API Key prompt, with paste, Backspace, Ctrl+U to clear and Esc to exit. Enter saves
a non-empty key to the provider's global configuration and continues to remote
authorization. The new key is used for both the build and following CLI query.

After files are processed, indexing shows **Optimizing index**, **Saving index**,
and **Finishing index** while storage is finalized and the index is published.
Interactive terminals show an animated indicator and elapsed time during these
stages; completion is reported
only after publication succeeds. Incompatible indexes show a rebuild command and
their directory, with internal diagnostics available through `--debug`.

Local selection starts the build. Remote selection first discloses the workspace,
model, endpoint, data sent and possible API charges, then offers Allow once, Allow
for this workspace, or Back to model selection. Existing matching workspace consent
skips this step. Allow once covers the CLI build and the following query without
persisting a grant. Esc exits either screen before any workspace content is sent.

CLI searches display model preparation/download and file indexing progress and wait
for the full index to be published before running the original search. This also
applies to `--mode server`, where the CLI follows the daemon's background job.
Non-interactive searches never create a missing index; initialize it explicitly
with `zg --index --embedding <model>` first. Empty workspaces do not download models.

On MCP connections with background scheduling and form elicitation, missing-index
searches ask the user to select a model and authorize remote use when needed.
MCP model choices require credentials to be configured beforehand.
Acceptance submits a background build and returns its job ID immediately; retry
search after completion. Allow once covers that background build only. Later search
requests report current build progress without repeating model selection. Unsupported
clients receive CLI initialization instructions. Native terminal tabs and Escape
handling are controlled by the MCP host. For protocol `2026-07-28`, echo the exact
`requestState` and unchanged tool arguments with `inputResponses.index_setup`;
each model selection or remote authorization round issues a new, single-use state.

`zg --callers <symbol> [root]` and `zg --callees <symbol> [root]` query an existing
workspace graph. CLI `-g`/`--glob` and `--iglob` use the same ordered,
workspace-relative glob rules as ordinary search (for example, `-g 'src/**'`).
They filter definitions and suggestions, while call-site files remain unrestricted.
Use `--limit N` to cap call sites per matching definition and
`--mode direct|server|auto` to select execution. Scope-qualified names such as
`Class::method` are supported. Same-name definitions remain separate, and repeated
call sites retain their locations; `truncated` indicates additional visible call
sites beyond the per-symbol limit. Endpoint visibility is checked before the limit;
queries do not compute an exact total. Each definition and its call sites share a
read snapshot; indexing may advance between definitions. Definitions changed since
initial selection are skipped until the next query.
Each edge has a caller `filePath` independently of its optional `symbol` metadata.
Both MCP toolsets expose `zvec_grep_callers` and `zvec_grep_callees` with an absolute `root`, `symbol`,
and optional `globs`, `insensitiveGlobs`, and `limit`, using the same glob inputs as
search. These queries do not load models or refresh the index.
Graph extraction and resolution must have populated the index for call edges to
be available; unresolved references are excluded from results.

This version indexes text and code with one embedding model per workspace. Choose
it with `zg --index --embedding <model>` or set a default with
`zg --config model set <model> --default`. Code, documents and structured text use
that same model and retrieval collection. Code has its own content payload kind.
Images and other unsupported sources are reported as skipped; multimodal content
and per-content model routing are not supported. Changing the model requires
`zg --index --rebuild --embedding <model>`.

Each entity stores one complete source content object, its location in the source
file, metadata, and its fragments. Each text fragment has a unique ID and selects
either full content or UTF-8 byte offsets relative to the entity's content. Source
locations, including line and column numbers, are calculated when needed;
fragments do not duplicate content or source coordinates.
Model collections store the text and vectors needed for retrieval, with entity
metadata included in search projections. Index format 2 uses this layout.

Lexical search runs in-process with ripgrep's `grep` and `ignore` crates; the
binary and ordinary CI jobs do not require a system `rg` executable.

A single file name or path, such as `zg auth.ts` or `zg src/auth.ts`, first
looks up current file paths. Matches use `#1 src/auth.ts`, with
forward slashes on every platform and exact workspace-relative paths before suffix
matches. Ties use deterministic native path ordering; `--limit` bounds the list
(default: 10). Lookup uses the nearest workspace's saved scan rules and query
file filters, and includes empty files. It runs locally in direct, server, and
auto modes without loading a model, creating an index, or requiring a daemon.
File discovery does not apply index-only default exclusions for build directories,
lock files, or images; saved scanning rules and ignore files still apply.
File matches do not read or embed contents. If no file matches, lookup searches
the selected current source text literally using the same scan rules and filters.
A path mentioned only in source text returns those text matches. No match prints
`No matches.` instead of unrelated vector neighbors, even with an existing index.
Text results deduplicate complete structural entities before applying `--limit`;
multiple retrieval fragments of one function or Markdown section count once.
Literal matching and highlighting use the same smart-case rules: queries with
uppercase letters are case-sensitive; lowercase queries ignore case.
Results show line numbers, match/context markers, and structural headings. Terminal output also
shows coverage and per-file/per-hit metadata. Truncated file or text results warn
on stderr. Unlike the Node.js CLI, truncated text remains local instead of starting
indexed retrieval; direct, server, and auto modes share this behavior and never
require model loading or indexing. Native glob ordering, format aliases, ignore
handling, and path normalization retain the Rust CLI's existing semantics.
Explicit `--hybrid`, `--fts`, `--vector`, `--fuse`, symbol options, `--trace`, and
`--refresh` retain their indexed behavior. Text questions, URLs, and glob patterns
do not trigger automatic path lookup.

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
ripgrep output-format switches other than file listing are rejected; Unicode
BOM decoding is automatic.

Automatic path lookup emits
`warning: automatically performing path lookup; use --hybrid to force hybrid search.`
on stderr only when the default query actually enters that branch. `--hybrid`
overrides the inference, including ambiguous inputs such as `config.timeout` and
`v1.2`.

When no file path matches and lookup falls back to literal source search, it also
prints `warning: no matching file paths; searching current file contents literally (rg).`
on stderr. This appears for both text matches and `No matches.`; stdout contains
only the search results.

Use `zg --rg --files` for explicit file discovery:

```sh
zg --rg --files
zg --rg --files -g 'auth.ts'
zg --rg --files src -g '*.ts' -g '!*.test.ts'
zg --rg --files -t rust -0
```

Every positional argument is an input file or directory, so `auth.ts` by itself
selects that input path; `-g 'auth.ts'` finds that name throughout the inputs.
Discovery reuses managed rg's glob, type, ignore, hidden, symlink, depth, size,
and modification-time filters. It reads filesystem metadata and ignore rules,
without opening source contents, loading models, or opening an index. All client
modes run locally. Output contains all selected paths in sorted order, one per
line or NUL-delimited with `-0`/`--null`, without headings or match metadata.
Absolute inputs retain absolute output paths; relative inputs retain relative
paths. `--limit` is rejected to avoid silently truncating the enumeration.
Exit status is 0 when files are found, 1 for an empty list, and 2 for a discovery
error; traversal errors go to stderr while accessible paths remain on stdout.
Like managed content search, recursive discovery skips `.git` and `.zvec-grep`;
explicit file inputs are still listed.
This is a CLI entry point; the MCP rg tool retains its content-search contract.

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
separate: use `ZVEC_GREP_API_KEY`, global model settings, or `--api-key` for
actual operations. An API key explicitly supplied when indexing is encrypted in
the manifest; inherited environment/global credentials and query-only keys are
not persisted.

The decryption key is stored in `.zvec-grep/credentials.key` and must travel with
the workspace. Copying the complete directory therefore copies access to its
saved credentials; this key does not grant permission to send data remotely.

Without `--embedding`, grant uses the existing index model, then
`ZVEC_GREP_EMBEDDING`, then the configured default model. `--endpoint` overrides
the stored index endpoint, per-model endpoint configuration, `ZVEC_GREP_ENDPOINT`,
and provider default. The grant at `.zvec-grep/authorization.json` binds the
canonical source root, username, model, and endpoint. A change to any of these
requires a matching grant. A copied grant remains valid on another machine when
the source path and username match. No authorization state or signing key is
stored in the user home directory, and no machine fingerprint is used.

Authorization records describe the user's saved consent; they are not signed or
protected against deliberate edits. Only explicit authorization updates the
recorded path and username; index updates and manifest writes do not. When the
username cannot be determined, use single-operation consent instead of a
persistent grant.

Direct CLI, server CLI, and MCP read the same workspace authorization on each
operation. Revocation takes effect for new
operations without restarting the server. `--allow-remote` on `zg --index` or
`zg <query>` grants consent for that operation only, including its synchronous
refresh, and never authorizes later watcher jobs. MCP reuses existing workspace
grants and otherwise asks form-capable clients for explicit consent before index
or search sends data remotely. The form discloses the root, source roots, model,
endpoint and data categories. Choose `once` for this operation, `workspace` for a
persistent grant, or `cancel` to deny transmission. Search also offers
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
an older completion cannot clear newer changes; partial file failures never
establish a reconciliation proof. Public search freshness requires observed
file-change evidence independently of reconciliation or background activity.
Native `flush_pending` drains delivered events without forcing a
scan, while explicit `flush` and periodic/recovery checks retain full rescans.
Explicit `info` inspection still reads current disk status and compares changed
files with saved content hashes. Automatic refresh checks use metadata first,
and the indexing pass resolves unchanged content before embedding.

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
The required field changes the internal daemon reply contract. Native ripgrep
type filters apply to queries; persisted scan rules support path globs and file
formats using the same filename rules as indexed queries. The daemon wire
distinguishes complete ordered glob replacement from case-category updates.
The instance record uses `daemonProtocolVersion: 1`; see the daemon migration
notes above when replacing older resident processes. Replies without
`content_range` are rejected during deserialization. Direct and server rendering
consume the same engine contract.

Managed rg results also provide optional `match_ranges` in decoded UTF-8 source
coordinates. Terminal highlighting uses these actual matches, preserving regex,
case, whole-word, and pattern-file semantics. Empty ranges are omitted; older
replies without this field remain readable.

Public search reports `freshness: fresh` or `freshness: possibly_stale`.
`results: served_from_current_index` describes where results came from, with
`background_refresh` reported separately. A provider that only reports current
index provenance without verified freshness is conservatively shown as
`possibly_stale`; a successful waited refresh reports `fresh`.

Search and index accept `globs` and `insensitiveGlobs` as a string or list;
case-insensitive string rules follow case-sensitive string rules. Index string
parameters update only the supplied case category. Typed glob lists and CLI glob
arguments replace the complete ordered list. Search `fileTypes` and
`excludedFileTypes` use the embedded ripgrep catalog, including `h`, `cpp`, `ts`
and `py`, independently of the engine filename format catalog. Search and index
accept `includedFormats` and `excludedFormats` from that catalog; index saves
these restrictions for subsequent scans. Index also accepts `follow`.
Search never changes persisted scan policy: `includeHidden`, `useDefaultIgnores`, `ignoreFiles`,
`maxDepth`, `maxFileSizeBytes` and `follow` belong on index requests. Node's
indexed-search path ignores its scanning options; Rust rejects them explicitly.
`useDefaultIgnores` controls built-in exclusions and discovered ignore files;
explicit `ignoreFiles` remain enabled independently of that switch. Missing files
are skipped and their paths remain saved; other read errors and invalid rules fail
before index mutations. `maxFileSizeBytes: 0` excludes all files, including empty files.
Unknown search/index fields are rejected. Index `maxDepth: null` and
`maxFileSizeBytes: null` clear saved limits.
Existing typed globs, `includedFormats`/`excludedFormats`, categories, `enum`,
`nestedGit`, diagnostic extensions and their compatibility boundaries are recorded in
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

## Native and CUDA builds

The safe llama API and the C5 FFI boundary share the same pinned
`zvec-ai/llama-cpp-rs` revision (`b55399ac6c2035f2301120547c4c967e741d25d3`),
from branch `prebuilt/0.1.154-cpu-r2-glibc-2.31`.
Its sys crate automatically selects a SHA256-pinned SDK when target, features
and native options match. Linux x64 CPU, Linux ARM64 CPU and Linux x64 CUDA use
`zvec-0.1.154-glibc-2.31-r1`, built on Ubuntu 20.04 and verified against a glibc
2.31 baseline. Windows x64/ARM64 CPU use `zvec-0.1.154-cpu-r2`; macOS and
CUDA-enabled Windows x64 use `zvec-0.1.154-r1`. CUDA architecture coverage is
unchanged. Matching builds reuse native
libraries and bindings without executing bindgen, CMake, C++ or CUDA compilation;
Rust application code still compiles normally. CPU-only x64 builds download only
the static CPU SDK, not the CUDA SDK; CUDA builds retain the shared core, CPU
backends and both CUDA plugins. Custom native configurations use source builds
in default `auto` mode. A download or integrity failure is an error, not a silent
source fallback.

The fork no longer registers llama.cpp as a Git submodule. Cargo fetches only
the Rust bindings; matching SDK builds never fetch native source. Only explicit
`source` builds or a configuration mismatch in `auto` acquire the hash-verified,
fixed-revision source archive, cached under `$CARGO_HOME/llama-cpp-sources`
(override with `LLAMA_CPP_SOURCE_CACHE`). The native revision, SDK and ABI are
unchanged by this source-acquisition optimization.

`LLAMA_CPP_BUILD_MODE=prebuilt` requires a matching SDK; `source` explicitly
rebuilds llama.cpp. `LLAMA_CPP_RUNTIME_CACHE` controls the verified SDK cache
(default `$CARGO_HOME/llama-cpp-runtimes`); `LLAMA_CPP_OFFLINE=1` requires it to
be populated. These are **build-time** controls, unrelated to npm's runtime
plugin installation. No local path patch or extra Cargo wrapper is required.

Run commands below from `rust/`. The shared npm build/pack/install entry points
prefer CUDA on Linux x64 and Windows x64. In default `auto` mode, with no `nvcc` on PATH they build a
CPU-only package; `--cpu-only` selects that behavior explicitly. Other release
targets keep their existing non-CUDA backends. Plain Cargo builds remain
non-CUDA unless `--features cuda` is selected. Explicit `prebuilt` mode selects
the CUDA SDK on x64 without probing or requiring a CUDA compiler.

For direct `cargo build --features cuda` builds using a shared SDK or source, zg stages
the linked llama/ggml core libraries beside the executable (and in Cargo's
`deps`/`examples` directories). Source mode, including an `auto` fallback, uses
only the current dependency's `lib`/`lib64`/`bin` outputs and copies resolved
SONAME aliases such as `.so.0`, repairing dangling links. Run `target/debug/zg`
or `target/release/zg` without adding native build directories to `LD_LIBRARY_PATH`.
Optional backends still use the verified SDK cache or current source backend
directory; this development output is not a portable distribution.
Use the npm packaging commands below to assemble a distributable runtime.

All Cargo builds deploy official ONNX Runtime 1.21.0 shared libraries beside
executables, tests and examples. The engine build script uses the same
SHA-256-pinned preparation script as npm packaging (Python 3 and curl are
required; `PYTHON` selects the interpreter). Archives are cached under
`target/ort-prebuilt` (under the target triple for cross-target builds), or
`ZG_ORT_CACHE_DIR`. `CARGO_NET_OFFLINE=true` requires verified cached archives.
CPU builds select CPU/CoreML/DirectML runtimes; x64 CUDA builds select matching
CUDA 12 core/providers. Both `--device cpu` and `--device cuda` locate packaged
ORT without loader environment overrides. CUDA execution still requires the
host NVIDIA driver, CUDA 12 libraries and cuDNN 9. An explicit `ORT_DYLIB_PATH`
skips automatic preparation and supplies a custom runtime when using ONNX WebGPU.
Enabling GGUF Vulkan alone does not require a custom ORT library.

```sh
npm run build
npm run build -- --cpu-only
npm run release:pack
npm run release:pack -- --cpu-only
# CUDA-capable x64 package without nvcc (PowerShell: set $env:LLAMA_CPP_BUILD_MODE):
LLAMA_CPP_BUILD_MODE=prebuilt npm run build
```

CUDA **source** builds require Toolkit 13.1 on PATH and Toolkit 12.4 at `CUDA12_ROOT`
(default `/usr/local/cuda-12.4` on Linux, or `CUDA_PATH_V12_4` on Windows).
An installed but unsupported primary toolkit, missing fallback toolkit, or
failed CUDA compilation is an error, not a silent CPU fallback. Building does
not require a GPU. Both variants need the normal Rust/C++ build prerequisites;
CUDA additionally uses CMake, Ninja, Python 3 and libclang, plus patchelf/readelf
on Linux or Visual Studio 2022 Build Tools on Windows.

The direct CUDA entry points are `scripts/cuda/build.sh` and
`scripts/cuda/build.ps1`. They create `target/release/zg-cuda/{bin,lib}` by
default; the npm entry point isolates them under `target/cuda/` and CPU builds
under `target/cpu/`. `CARGO_TARGET_DIR` changes the parent output directory.
Source-mode CUDA 12.4 on Windows requires MSVC 14.38 before running
`scripts/cuda/build-fallback.py`. Matching SDK builds copy that plugin directly,
so neither a second CMake build nor the fallback toolchain is needed.

Publish CI requires prebuilt llama SDKs on all six targets, including the
Windows ARM64 static CPU SDK. CUDA Rust CI also requires
prebuilt mode instead of recompiling only for its T4 GPU. Linux publish builds
run in Ubuntu 20.04 containers without a CUDA compiler. Runtime libraries,
driver checks, link audits and real CPU/GPU inference gates remain required. Runtime staging
reads the current Cargo invocation's `GGML_BACKENDS_DIR`, not the newest directory
under `target/build`; only runtime libraries, not SDK import archives or bindings,
are packaged. CUDA 13.1 distribution builds use
`75-real;80-real;86-real;89-real;90-real;90-virtual`. The CUDA 12.4 fallback uses
`50-virtual;61-virtual;70-virtual;75-virtual;80-virtual;86-real` to reduce its
size, retaining PTX for driver JIT compilation where compatible native code is
unavailable. Override `CMAKE_CUDA_ARCHITECTURES` for the primary backend or
`CUDA12_ARCHITECTURES` for the fallback only when deliberately customizing a
source build (`LLAMA_CPP_BUILD_MODE=source`). The fallback does not inherit the
primary backend's architecture list; a custom fallback list cannot be mixed
silently with a prebuilt SDK.

The package contains `bin/zg[.exe]`, with all shared libraries, plugins and
dictionaries under `lib/`. GGUF loads the CUDA 13.1 plugin first, then the
CUDA 12.4 fallback from `lib/cuda12/`; both use the same locked llama.cpp core.
ONNX uses the hash-verified official ORT 1.21.0 CUDA 12 core/providers.
NVIDIA driver, CUDA runtime, cuBLAS and cuDNN 9 remain host dependencies.
CUDA-enabled x64 builds default to automatic device selection and fall back
to CPU when a GPU/backend is unavailable. `--device cpu` or
`ZVEC_GREP_DEVICE=cpu` explicitly selects CPU. In CUDA-enabled GGUF builds,
explicit CPU execution loads only CPU plugins, ignores `GGML_BACKEND_PATH`,
and supplies an empty GPU device list. Optional GPU registration is separate:
a later GPU request in the same process can still use preinstalled plugins.
Before opening either CUDA plugin, GGUF checks the active CUDA Driver API
capability, not the installed Toolkit version. The CUDA 13.1 primary requires
at least `13000`. With the distribution targets, every visible GPU must be able
to use native SM75/80/86/89/90 code, or compute90 PTX with capability `>= 13010`.
For example, SM100 on a driver reporting `13000` skips the primary before it is
registered and tries CUDA 12.4 instead. Forced PTX requires `13010` and a
compute90-compatible GPU; it cannot use the primary on older GPU architectures.
Failed queries or missing compatible code likewise skip the primary.
Before opening the CUDA 12.4 fallback, its separate check still applies.
PTX 8.4 paths require a reported
capability of at least `12040` (CUDA 12.4). Older CUDA 12 drivers remain eligible
when every visible GPU can use the distribution's native SM86 code (SM86/89)
and PTX JIT is not forced. On those older drivers, any visible PTX-only device
rejects the whole fallback. Failed probes or insufficient capability likewise
skip it with a diagnostic and retain CPU execution.
Explicit CPU requests do not run this probe; ORT selection is independent.
GPU requests retain the upstream `GGML_BACKEND_PATH` override behavior, but an
override requires `13010` before either GPU plugin scan: its architecture list
is unknown, so it cannot use native-code exceptions or bypass either guard.
Build-time custom architecture lists or an external `LLAMA_CPP_RUNTIME_DIR`
also disable the corresponding exception. The primary exception requires
explicit distribution targets or pinned `prebuilt` mode; `auto`/`source` builds
without explicit targets conservatively require `13010`. Replacing plugins
after a build is outside this distribution contract.
This is a compatibility preflight, not subprocess isolation or a guarantee that
every driver/runtime failure can be caught and retried on CPU.

See [npm distribution](npm/README.md) for the complete layout, OS/backend
support matrix, runtime dependencies, and publication policy.

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

The build uses the published `zvec-rust` and `zvec-rust-build` 0.8.0 crates,
without a local wrapper patch. Their bundled native library tracks upstream zvec
at [`8397071`](https://github.com/alibaba/zvec/commit/8397071d815cd842be2068339e00a94e69f069d1).
The Cargo build stages the shared library beside the executable. Local and
release npm packages place the executable in `bin/` and runtime libraries in
`lib/`. The engine uses zvec's embedded Jieba dictionary without external
dictionary files.

Pass `--no-build` after `--` to reuse the last successful npm build (or a plain
Cargo `target/release/zg` when no build record exists), or pass
`--prefix <path>` to install or smoke-test under a custom npm prefix:

```sh
npm run install:local -- --no-build --prefix /tmp/zvec-grep-npm
```

On Linux and Windows, manual CPU packaging rejects runtime libraries outside
the known CPU set (zvec and CPU/DirectML ORT). A CUDA-to-CPU Cargo rebuild can
leave old shared libraries beside `zg`; it does not clean that directory.
If packaging rejects such a mixed directory, rebuild with `--cpu-only` in a
fresh `CARGO_TARGET_DIR`, or use an audited CUDA bundle. Packaging never deletes
the original libraries or silently includes them in a CPU package.

## npm release packaging

The registry distribution is prepared as one meta package plus exact-version
native optional dependencies declared in `npm/platforms.json`. The meta package
uses Node only during `postinstall` to verify and materialize the native
executable and conditionally migrate existing agent configurations; running `zg`
enters that executable directly. The native meta package requires Node.js 14.14
or newer during installation.

Validate the release manifest and build the current platform dry-run packages:

```sh
npm run release:verify
npm run release:pack
```

The default published Linux x64 and Windows x64 bases contain a complete CPU
runtime and retain dynamic CUDA support. Optional exact-version CUDA components
are pinned in `plugins.json`, not mandatory npm dependencies. Postinstall installs
CPU first and, by default, fetches the complete CUDA component set when NVIDIA
detection succeeds. `ZG_INSTALL_ACCELERATOR=cpu` skips detection/downloads;
`cuda` forces installation, including on GPU-free builders. Installation requires
no CUDA compiler. Linux and Windows ORT are each transported as two
fragments and reconstructed with a full-library SHA256 check. The installed
`bin/`, `lib/` and `lib/cuda12/` layout is unchanged. Local packaging follows the
build policy above: no toolkit produces a CPU-only package. Use
`npm run release:cpu` to explicitly produce a CPU-only package; CI supplies and
audits a complete CUDA bundle before packaging the default x64 releases.

Later add CUDA with `ZG_INSTALL_ACCELERATOR=cuda npm rebuild -g @zvec/zvec-grep`
(omit `-g` for project-local installs). `ZG_PLUGIN_DIR` selects an absolute local
artifact directory; `ZG_INSTALL_OFFLINE=1` prevents network access. Stop zg before
adding plugins and restart any daemon afterwards. There is no `zg-setup` command
or download during indexing/query execution. Auto plugin failures retain CPU;
forced failures return an error. See [npm installation details](npm/meta/README.md),
including recovery when installation scripts were skipped.

The release tarballs are written under `dist/npm/release/tarballs/`.
For release validation, produce the native packages once with `release:platform`,
then run `release:collect -- --targets <target>` to pack the meta package and
record the exact tarball hashes in `release.json`. Install those collected bytes
in an isolated npm prefix and verify the materialized binary with:

```sh
npm run release:smoke
```

`release:smoke` (also named `release:artifact-smoke`) only verifies existing
collected artifacts; it never builds or packs packages. CUDA release targets
require their complete collected tarball set, but default `auto` installation
may produce a verified CPU-only or CUDA installation depending on the host.
Use `-- --accelerator auto --expect-cuda` when smoke must reject missing CUDA
plugins (as GPU CI does); `--expect-cpu` instead requires zero CUDA requests on
a CPU host. These expectations do not change the installer's Auto policy.
A failed local rebuild invalidates
`native-build.json`, so `--no-build` refuses reuse until a build completes.

CI can package a previously built target artifact without rebuilding it:

```sh
node scripts/npm-release.mjs pack-platform \
  --target linux-x64-gnu \
  --binary /path/to/zg \
  --lib-dir /path/to/runtime-libs \
  --no-build
```

The runtime library directory must include zvec's shared library. Without
`--lib-dir`, runtime libraries are read from the binary's directory.
This entry point is CPU-only and applies the same mixed-directory check as
local `pack --no-build`, including when `--binary` or `--lib-dir` is explicit.

For a complete audited CUDA bundle, preserve its `bin/` and `lib/` layout:

```sh
node scripts/npm-release.mjs pack-platform \
  --target linux-x64-gnu \
  --bundle-dir target/release/zg-cuda \
  --no-build
```

Bundle mode accepts only regular files under `bin/` and `lib/`; `bin/`
contains exactly the target executable. It cannot be combined with
`--binary`, `--lib-dir`, or `--cpu-only`. All payload files are checksummed.

The `Publish Rust packages` workflow builds the selected published native targets
(all six by default) on matching CPU runners. It runs only through manual
dispatch. With `publish` left at its default `false`, it produces downloadable
candidates without publishing to npm. Each CPU artifact-verification runner installs
its platform and meta tarballs, then runs `zg --version`, checks managed-rg,
starts the packaged daemon, compares workspace status in direct and server
modes, and stops the daemon. The server status request crosses the IPC boundary;
managed-rg intentionally stays local. macOS binaries receive ad hoc signatures;
Developer ID, notarization, and Windows certificate signing are not required for
candidate builds.

The selected platforms build in parallel after source validation; Intel macOS
does not gate Linux or Windows. After all selected build jobs upload their
native artifacts, collection creates the candidate once. Independent CPU and
requested GPU jobs then verify the exact collected tarballs, including CLI,
managed-rg, and daemon IPC checks on every selected platform.

The packaging workflow's manual `platform` input defaults to `all`. Select any
of `darwin-arm64`, `darwin-x64`, `linux-x64-gnu`, `linux-arm64-gnu`,
`win32-x64-msvc`, or `win32-arm64-msvc` for an individual build. For example, to validate Linux x64:

```sh
gh workflow run rust-publish.yml --ref <branch> \
  -f publish=false -f cuda=false -f platform=linux-x64-gnu
```

Partial selections produce validation-only candidates and cannot be published;
`publish=true` requires `platform=all`. Runs use branch-plus-platform concurrency
groups: different platform selections can run concurrently, while an in-progress
run in the same group is not cancelled by a newer run.

Pushes do not trigger this packaging workflow. Manual runs with both
`publish=false` and `cuda=false` use only CPU runners and do not publish to npm.
Packaging always compiles on CPU runners, including when `cuda=true` or
`publish=true`. Set `cuda=true, publish=false` to add GPU validation without
publishing. Separate jobs on `gpu-linux-t4` and `gpu-windows-t4` download the
selected Linux/Windows x64 native packages from the same run, verify their
payload checksums, and run the packaged GGUF/ONNX GPU tests without compiling or
repacking. These jobs install the required runtime libraries, not build
toolchains, and exercise the CUDA 12 fallback and CUDA 13 primary paths.
Both Windows GPU workflows share `scripts/cuda/prepare-windows.ps1`: on the
ephemeral runner it upgrades older drivers to the pinned NVIDIA Data Center
591.59 driver, authenticates the installer signature, and verifies the active
driver version plus CUDA Driver API initialization and device enumeration.
A driver installer reboot requirement, an inactive upgrade, or failed CUDA
initialization stops the job; the helper never reboots the runner.
Windows CUDA library lookup includes both toolkit `bin` and CUDA 13 `bin/x64`
directories. Workspace and package GPU checks independently load the selected
GGUF plugin and its dependencies before inference; this CI setup does not change
the package's host CUDA runtime requirements.
Publishing always requires GPU validation regardless of `cuda`. Candidate
collection happens first, followed by parallel CPU and requested GPU validation.
All selected platforms get the installed CLI/daemon checks; Linux/Windows x64
also get the CPU model/fallback checks. A GPU failure does not block CPU checks
or candidate retention, but all required CPU/GPU checks must pass before npm
publication. A downloadable candidate is not proof of runtime validation.
CPU runners compile Rust and assemble the default CUDA-capable bundles using
the pinned native SDK. Linux x64 and ARM64 use Ubuntu 20.04 containers on native
runners, without nvcc. Windows needs the Rust/MSVC linker but
does not install CUDA compilers for packaging. Ordinary PR CI
does not compile CUDA packages. `rust-ci.yml` is the single entry point for
workspace and real-model verification. Push/PR runs use ordinary runners on
Linux, macOS and Windows. Each test job compiles the workspace tests once, runs
them with pinned cargo-nextest, and reuses the CLI and test binary for smoke
checks and all three real models (Model2Vec, ORT and llama.cpp), without a separate
build per model.
For manual runs, `cuda_verify=true` enables `zg/cuda` from the first compilation,
requiring the matching prebuilt SDK, but compilation, workspace tests, all three
CPU model checks and build-cache uploads still run on ordinary CPU runners.
These Linux workspace tests use an Ubuntu 24.04 builder without GPU access or
CUDA runtime libraries; the npm release build uses Ubuntu 20.04 as described above.
Windows uses `windows-2022`, matching the GPU image's OS generation.
Before the test harness copies any libraries, CUDA jobs run `cargo build --features cuda`
and exercise the original `target/debug/zg` with GGUF/ONNX CPU index and query.
The regression clears project library lookup overrides and runs outside the output
directory. A `rust-cargo-cli-<platform>` artifact preserves these exact executable
and sibling library bytes, plus the SDK backends at their compiled-in cache path.
GPU jobs verify the snapshot's source SHA and hashes, then repeat CPU/CUDA index
and query without repairing Cargo output or adding project library paths. The
Windows CI SDK cache uses a fixed location because CPU/GPU workspaces have different
drive letters; NVIDIA's host runtime paths are still required and preserved.
After the CPU gates pass, separate GPU jobs download a `rust-cuda-tests-<platform>`
artifact containing only two test executables and their runtime libraries.
The bundle is checked against the resolved source SHA and per-file hashes, then
run from a new path with explicit native-library lookup. GPU jobs install no Rust
toolchain/MSVC, compile nothing and neither restore nor save Cargo build caches.
They retain DLL loading checks, real ORT/llama.cpp CUDA inference with actual GPU
evidence and no CPU fallback, plus the C5 CPU-to-CUDA same-process and CPU-plugin
override isolation tests (`c5-*.log`). CUDA 12 GGUF fallback coverage remains in
publish CI; Rust CUDA CI tests the CUDA 13 GGUF primary and CUDA 12 ORT.
Model2Vec stays on CPU; macOS and the separate non-CUDA quality job remain
unchanged. Quality, default tests and CUDA builds have separate build caches.
`platform` defaults to `all`;
select `linux` or `windows` to limit test platforms. Optional `ref` selects the
reviewed source; otherwise the workflow's selected commit is used. All jobs
check out the same resolved SHA. This workflow does no npm packaging or publishing.
Linux x64/ARM64 npm releases compile the CLI in Ubuntu 20.04 containers,
reuse the pinned native SDKs, and audit the executable and bundled libraries
against glibc 2.31. Downloaded npm artifacts are installed and exercised on
Ubuntu 20.04, including CPU GGUF/ONNX inference for the x64 bundle without
CUDA installed. Host runners may use newer Ubuntu images; they do not set the
release binary's glibc baseline. CUDA acceleration still requires compatible
NVIDIA drivers and CUDA/cuDNN host libraries. The separate ORT CI gate also
audits official ORT 1.21 x64/ARM64 runtimes and loads them on Ubuntu 20.04.
Packaging selects Cargo/CMake parallelism from available CPUs and total memory
with a conservative 3 GiB per-job budget, a minimum of 1 and a maximum of 8.
The same limits are passed into the Linux builder. This is a resource
heuristic, not a guarantee against OOM. The existing 180-minute limits remain
upper bounds, not expected build durations. CUDA 13.1 and 12.4 native compilation
now belongs to the fork's versioned SDK release, not every zg CI run.
Intel macOS downloads the official ORT 1.21.0 precompiled dylib, just like
Apple Silicon; no ORT source checkout, CMake build or dedicated build cache is needed.
Like the other targets, it lets `zvec-rust-sys` select the prebuilt zvec SDK from
its locked crate version and Cargo `TARGET`, retaining the upstream source-build
fallback if SDK acquisition fails. The SDK cache lives under Cargo `OUT_DIR`
and is covered by the existing `rust/target/release` cache. ONNX Runtime is
cached separately; both shared libraries are included in the npm payload.
The Intel release targets macOS 14 and later.

Windows ARM64 builds and tests natively on `windows-11-arm` with ARM64 Node,
Rust, and MSVC. zvec-rust 0.8.0 automatically downloads its Windows ARM64
prebuilt SDK and stages `zvec_c_api.dll` for packaging; no local zvec source
build is needed. ONNX Runtime uses the SHA-256-pinned official 1.21.0 DirectML NuGet runtime
and the matching Microsoft.AI.DirectML 1.15.4 DLL, selected for ARM64. llama-cpp-rs selects the
SHA-256-pinned Windows ARM64 static CPU SDK from `zvec-0.1.154-cpu-r2`, without
running bindgen, CMake or a C++ compiler. No Clang/Ninja overrides are needed. npm selects
`@zvec/zvec-grep-win32-arm64-msvc` for ARM64 Node on Windows.

For a complete all-platform run, download the `rust-release-<version>` artifact
from the successful workflow run
and unzip it. It contains six base platform tarballs, seven CUDA component
tarballs, the meta tarball,
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

Subset runs instead upload `rust-candidate-<version>`, containing the selected
native tarballs and all required components, the unchanged exact-version meta tarball, checksums and install
instructions. Its `release.json` records the selected targets and `partial: true`;
it cannot be published or promoted as a complete release.

Selected Linux/Windows x64 candidates also go through separate CPU artifact jobs.
These download the collected `.tgz` files, verify their hashes and default CUDA
capabilities, install only base/meta offline without rebuilding, and exercise CLI/daemon IPC.
With CUDA host libraries unavailable, the installed meta package must also pass
real GGUF/ONNX indexing and vector queries using both explicit CPU selection and
default automatic CPU fallback. Logs are retained as `cpu-artifact-evidence-*`.
Failure blocks npm publication; the candidate artifact is kept for diagnosis.

To publish, update the Rust workspace version and manually run the workflow with
`publish` enabled. `rust-canary` allows `main` or a tag; `latest` requires a tag.
Tag names are unrestricted; for example, use `v0.3.0` for the 0.3.0 release. The
package version always comes from Cargo, not the tag name. From the CLI, publish
a canary with `gh workflow run rust-publish.yml --ref main -f publish=true`, or
request a stable release with
`gh workflow run rust-publish.yml --ref v0.3.0 -f publish=true -f dist_tag=latest`.
The selected ref must contain the updated workflow; rerunning a historical run
does not apply workflow fixes from newer commits.
The publishing repository must have an npm trusted publisher configured for all 15
packages with both **Allow npm publish** and the independent **Allow npm
dist-tag** permission enabled, or provide an eligible `NPM_TOKEN` secret.
Publishing and promotion install npm 11.21.0 explicitly for OIDC dist-tag support.

Supplemental license collections and SPDX SBOMs in [`npm/compliance/`](npm/compliance/README.md)
are optional. Every release package includes the project `LICENSE`, and bundled
upstream notices are preserved.
Every publication uses `rust-canary`, letting users opt into Rust with
`npm install --global @zvec/zvec-grep@rust-canary`. Six native runners then
install only the meta package from the registry, test actual daemon IPC, and
exercise update and rollback with a distinct previously published version.
Selecting `latest` requests a separate promotion job that changes the default
installation only after all registry checks pass.

Each compressed tarball must be at most 180 MiB. Packing, collection and
publication preflight all enforce this budget before any npm publication.
Plugin validation shares the same limit. This project budget accounts for
base64 expansion in npm's upload request; it is not a registry size guarantee.
Use `--output-dir <path>` with release commands to keep inspection artifacts
separate from `dist/npm/release/`.

`release:collect` requires all platform/component tarballs by default and packs the meta
package last. `release:collect -- --targets linux-x64-gnu,win32-x64-msvc` collects
only the explicitly selected platform tarballs and their components as a validation-only partial
candidate; missing or extra tarballs are rejected. The meta package still locks
all six platform versions, so its installation behavior is unchanged.
`release:artifact-smoke` installs the current platform's exact collected native
component and meta tarballs from `dist/npm/release/tarballs`, verifies the installation and
CLI/daemon operation, and does not compile, repack or publish anything.
CPU and GPU jobs share `install-artifacts`, the real npm installation entry point, and retain its
`artifact-smoke/installation.json` report. They download only their platform
artifact plus `rust-verification-<version>` (the collected meta tarball and
inventory), so full-platform CUDA payloads are not repeatedly downloaded.
`install-artifacts` verifies identity and payloads without executing zg; GPU jobs
then execute in the designated runtime environment. `smoke-artifacts` adds the
CLI/daemon probe on the matching CPU runner.
Registry install/update/rollback runs on ordinary CPU runners and explicitly
checks exact component versions, payload hashes and the assembled runtime in
addition to registry tarball integrity; a CPU-only CLI success cannot hide a
missing CUDA component. Real GPU execution is validated before publication.
Every registry platform also runs Potion, GGUF and ONNX CPU indexing, vector/FTS/
hybrid queries, incremental updates and MCP stdio search after fresh installation
and after upgrade. All commands use npm's Unix `zg` symlink or Windows `zg.ps1`
entry, including baseline/rollback version checks. The executable path is never
canonicalized by the test runner. The ONNX loader itself resolves executable
symlinks before locating its sibling libraries. Default CPU installs are checked
for unwanted CUDA components, plugin caches and npm download attempts before
the deliberate CUDA rebuild check. Model caches are reused; command output,
MCP responses and per-model results are uploaded even on failure. The `latest`
promotion job requires the entire six-platform registry matrix to succeed.
`release:publish` validates the collected files before publishing native packages
and then the meta package under `rust-canary` with npm provenance. An already published version is
reused only when its tarball integrity matches, allowing a partial publication
to be retried without replacing a version. `release:promote` only moves tags for
an already published identical release and runs after registry validation in the
workflow. Ordinary build and packaging commands
do not publish or move dist-tags.
