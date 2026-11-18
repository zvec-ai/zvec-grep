# Engine and workspace lifecycle

The [architecture overview](../../ARCHITECTURE.md) defines crate boundaries.
The [engine guide](../../crates/zg-engine/README.md) covers the public API and
links to module-level contracts. This page covers resource and workspace lifetime.

## Engine and model ownership

`ZvecGrep` is normally shared for the lifetime of a process. Workspace root is
request state, so the same instance can serve multiple workspaces. Its typed
methods call private services directly, as described in the engine guide.

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

## Read sessions and write coordination

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

## Resident workspace lifetime

The daemon retires workspace runtimes and watchers after four hours without a
foreground operation. Maintenance runs once per minute and does not renew the
idle deadline. Active queries, inspections, watcher setup, queued indexing and
running indexing prevent retirement; background changes and job completion do
not restart the four-hour timer. Retirement forgets the workspace's finished job
history and prevents old callbacks from restarting its watcher. A later request
can activate a new runtime without deleting the persisted index. Model runtimes
remain shared at engine scope and follow their own lease lifetime.

## Workspace discovery and publication

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

## Stored content

Each entity stores one complete source content object, its location in the source
file, metadata, and its fragments. Each text fragment has a unique ID and selects
either full content or UTF-8 byte offsets relative to the entity's content. Source
locations, including line and column numbers, are calculated when needed;
fragments do not duplicate content or source coordinates.
Model collections store the text and vectors needed for retrieval, with entity
metadata included in search projections. Index format 2 uses this layout.
