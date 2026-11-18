# Daemon and MCP behavior

Both HTTP and stdio expose the same tools and use the shared engine. CLI and
MCP use the same content contracts; intentional execution-mode differences live
in [mode-differences.toml](../../compat/mode-differences.toml).

## Daemon logs

The resident daemon writes JSON Lines to `<home>/daemon/logs/server.log`.
It rotates at 10 MiB and keeps five numbered backups by default. The global
`~/.zvec-grep/config.json` accepts `log.maxBytes`, `log.keep`, and `log.level`
(`info` or `debug`); restart the daemon after changing them. Bootstrap failures
are written to `<home>/daemon/bootstrap.log`.

## Protocol compatibility

The instance record at `<home>/daemon/instance.lock` carries
`daemonProtocolVersion: 2`. Setup replies advertise `supports_embedding_defaults`;
clients check it before sending the optional `embedding_default` indexing or
authorization field. Without this capability, setup remains read-only and the
client requests a daemon restart before indexing with a caller default.
Status checks and daemon-backed CLI requests reject live legacy daemons without
a protocol version or with version 1, including Node.js daemons. `auto` mode
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

## Freshness and refresh

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

## Missing-index setup

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

## Search presentation

Search accepts `preview: "short"` (the default) or `preview: "full"` in both
toolsets. Both modes include a required MCP `freshness:` header and reuse CLI
result content and diagnostic conditions. Short preview shows up to ten source lines around the matched range;
full preview returns the complete stored entity for each indexed hit, even when
only one fragment matched.
Neither mode reads whole files, changes retrieval or ranking, or displays
generated outlines. CLI `none` and `detail` previews remain CLI-only.

The engine provides `content_range` for the exact source coordinates of returned
content, independently of the entity `range` and matched `excerpt_range`. CLI and
MCP use these coordinates for source numbering; preview never infers a range from
the number of content lines. The engine also interprets half-open line bounds.
Native ripgrep type filters apply to queries; persisted scan rules support path globs and file
formats using the same filename rules as indexed queries. The daemon wire
distinguishes complete ordered glob replacement from case-category updates.
Replies without `content_range` are rejected during deserialization; see
[protocol compatibility](#protocol-compatibility). Direct and server rendering
consume the same engine contract.

Managed rg results also provide optional `match_ranges` in decoded UTF-8 source
coordinates. Terminal highlighting uses these actual matches, preserving regex,
case, whole-word, and pattern-file semantics. Empty ranges are omitted; the field
is optional.

Search still honors `autoUpdate` and `freshness: "wait_for_fresh"` for refresh
timing. Global freshness and indexing diagnostics precede the result body in
MCP's single text response, separated by a blank line, including empty results
and fresh hits served from an incomplete index. Healthy results only add the
`freshness: fresh` header. Per-item status stays within each hit. CLI diagnostics
use stderr, so consistency means matching content and conditions rather than
concatenating stdout and stderr in the same order.

## Inputs and scan policy

Search and index accept `globs` and `insensitiveGlobs` as a string or list;
case-insensitive string rules follow case-sensitive string rules. Index string
parameters update only the supplied case category. Typed glob lists and CLI glob
arguments replace the complete ordered list. Search `fileTypes` and
`excludedFileTypes` use the embedded ripgrep catalog, including `h`, `cpp`, `ts`
and `py`, independently of the engine filename format catalog. Search and index
accept `includedFormats` and `excludedFormats` from that catalog; index saves
these restrictions for subsequent scans. Index also accepts `follow`.
Search never changes persisted scan policy: `includeHidden`, `useDefaultIgnores`, `ignoreFiles`,
`maxDepth`, `maxFileSizeBytes` and `follow` belong on index requests. Search rejects those scanning options.
`useDefaultIgnores` controls built-in exclusions and discovered ignore files;
explicit `ignoreFiles` remain enabled independently of that switch. Missing files
are skipped and their paths remain saved; other read errors and invalid rules fail
before index mutations. `maxFileSizeBytes: 0` excludes all files, including empty files.
Unknown search/index fields are rejected. Index `maxDepth: null` and
`maxFileSizeBytes: null` clear saved limits.
Existing typed globs, `includedFormats`/`excludedFormats`, categories, `enum`,
`nestedGit`, diagnostic extensions and their compatibility boundaries are recorded in
[`allowed-differences.toml`](../../compat/allowed-differences.toml).

## Transport, progress and cancellation

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
