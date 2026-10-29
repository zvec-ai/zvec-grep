# Workspace persistence

`workspace` connects domain workspace state to the filesystem. It owns manifest encoding, physical index locations, build publication and recovery, and workspace locks. The domain owns `Workspace`, `ScanRules`, `IndexState`, `IndexDescriptor`, and `EmbeddingModelInfo`; the engine's `file_selection` capability implements scanning rules. API result views are assembled at the application boundary.

## Workspace roots

A workspace is identified solely by its root path. Requests resolve the root to a canonical path, and the daemon uses that path as the key for resident runtimes. Different directories with the same basename are independent workspaces. There is no workspace name, alias, or per-user registration.

The engine does not read or write `~/.zvec-grep/workspaces.json` or its lock, and `ZVEC_GREP_WORKSPACE_REGISTRY` is no longer used. Legacy registry files are ignored and may be removed. Indexing and dropping an index only change the selected workspace's local state.

## Relocation

Move or copy the root together with its `.zvec-grep` directory to retain workspace state. Reads resolve source paths against the current root. The next index operation saves that location in the manifest, preserving active storage and file IDs. The original directory may still exist. Obsolete `name` fields in manifests and interrupted build records are ignored on read and omitted on write; existing indexes do not require a rebuild for this change.

File IDs belong to source records in one physical index generation. A writer loads a path-to-ID cache from these records once; unchanged files reuse their recorded IDs, and only previously unrecorded paths need allocation. Reservations stay in memory until their `NotIndexed` file records are written. Directory identities are stored in the `directories` collection; file IDs remain owned by file records. Allocation caches live in memory; interrupted work is represented by file status. File and directory IDs are separate `u32` spaces; checked allocation rejects exhaustion without wrapping or partially reserving a batch.

File IDs remain unchanged while a record is updated or relocated with its index. Rebuilds can assign different IDs. Deletion ends a record's identity; IDs may be reused after reopening once the old record is gone. Entity and fragment IDs are therefore references within one physical generation, not durable external handles. Directory filters use indexed arrays of `u32` directory IDs on file and search documents. The `directories` collection stores directory ID, parent ID and native path; memory maps accelerate lookup.

## Canonical data and model indexes

Each generation contains three shared collections: `directories`, `files`, and `entities`. Each entity stores its original content once, its `source_range` within the file, and every one of its globally unique fragments. A fragment contains only an independent ID and a `range` relative to the entity content. Both locations use the same `Range` type. An entity source location can be `Full`, `Text` with complete text coordinates, or `Byte`. A fragment uses `Full` for the entire content or `Byte` with only `start_offset` and `end_offset` in the entity's decoded UTF-8 text. Partial text fragments never store lines or columns; images and tables require `Full`. Fragment source locations are derived from the entity's source range and the fragment's relative range; no second coordinate system is stored on a fragment. Fragments have no content, file, owner, or metadata copy. The canonical entity record stores the complete fragment definitions, with metadata stored once in its own field. There are no representative or standalone fragment variants. Composite content is reserved as a future explicit extension; ordinary entities cannot contain a list of unrelated contents.

Current indexing acquires one embedding runtime for text and code. The domain descriptor represents a default embedding model and optional content-specific overrides; it does not determine the physical collection layout. Every indexed fragment belongs to the model's shared `fragments_<fingerprint>` collection, which holds the selected fragment text with the fixed FTS index, vector, canonical IDs and indexed filtering fields. There is no separate FTS collection or separate vector collection. Search loads a canonical entity through its entity ID and verifies that the hit's independent fragment ID belongs to that entity.

`--embedding` selects the workspace model. Text and code share that model's collection. Images and other unsupported sources are reported as skipped before extraction or embedding. Changing the model, vector dimension or metric requires explicit `zg --index --rebuild`; input limits, batch size, runtime credentials, concurrency and timeout changes do not require rebuilding.

FTS and vector retrieval use the same model collection. Vector queries use the model that built the index. The current version does not accept content-route configuration or query multiple embedding models.

## Index compatibility

`WorkspaceManifest` combines a domain `Workspace` with persistence-specific fields: workspace home, index version, active storage generation, and embedding runtime configuration. It does not embed an API `WorkspaceIndexInfo`.

`indexVersion` is the only compatibility version for the manifest, storage schema, and record encoding. The current Rust version is 2. Embedding vectors are stored as IEEE binary16; existing FP32 indexes require an explicit rebuild before use. Node.js indexes use version 1. The manifest records `defaultEmbeddingModel`, typed `embeddingRoutes`, runtime configuration and source selection settings. A current-version load validates the workspace root, model definitions, content-route capabilities and storage generation for an enabled index. Structural validation still rejects corrupt current-version data; it does not maintain another version counter.

Inspection determines compatibility before decoding current-format metadata or opening storage. Missing or different versions and invalid persisted metadata report `rebuild_required`, including actual and expected versions and the reason. Search, incremental indexing, and watcher activation reject that state. An unbuilt workspace remains distinct from incompatible persisted data. An explicit rebuild starts with the requested embedding model or the configured default and new scan settings; it does not migrate an incompatible manifest. Prior Rust development formats do not need migration support.

Generation-directory UUIDs only identify storage for publication and cleanup. Ordinary incremental indexing updates the active physical generation in place; it does not create immutable snapshots or introduce another compatibility version.

## Publication and recovery

First builds and rebuilds always start in empty storage under `.zvec-grep/generations`. A durable `build.json` records only the temporary generation and root; it has no format version or embedded manifest. Recovery reads the active generation from the manifest without decoding its index configuration. An atomic manifest replacement selects the completed index; cleanup removes superseded generations and Node.js collections afterward. Per-file extraction or embedding failures are recorded and do not prevent publication of the successful files. Cancellation, storage mutation failure, or failure to finalize/close storage discards the new generation and preserves the active index. Every explicit rebuild starts over, including after an interruption. Ordinary indexing updates the active index and never resumes an abandoned rebuild. Crash recovery uses the manifest selection to discard unpublished storage or finish post-publication cleanup. Cleanup failure after publication does not turn a successful rebuild into a failed operation; the next writer retries it.

The indexing application service acquires the workspace lock before mutating its local metadata. Storage is organized by table: `directories`, `files`, `entities`, and `fragments` own their schemas, codecs, and collection operations. `IndexStore` holds the shared leases and locks and coordinates cross-table writes, file status transitions, and search-result loading. The file ID cache belongs to the file table; these mechanisms remain separate from workspace lifecycle.

## Persisted state and runtime observations

`Workspace.index` is `Uninitialized`, `Disabled`, or `Enabled(IndexDescriptor)`. Enabled always carries the embedding model information and fixed FTS configuration; physical format and generation-directory layout remain owned by `WorkspaceManifest`. A first build or rebuild keeps its target in `WorkspaceBuild` until publication. The selected descriptor does not imply that files are up to date or that every file succeeded. Every entity uses the workspace model, while file completion covers all of its entities.

The daemon starts with no resident workspaces and admits runtimes on demand for requests. Its in-memory runtime map is keyed by canonical root. Foreground requests renew the idle deadline; active requests and queued/running index jobs prevent retirement. After the idle timeout (four hours by default, configured by `ZVEC_GREP_WATCHER_IDLE_TIMEOUT_SECONDS`), maintenance removes the runtime and its job history and closes its watcher. Setting the timeout to `0` disables idle eviction. Restart does not restore a workspace list.

The daemon's `WorkspaceRuntime` holds an optional `IndexStatusSnapshot`: observed health, file statistics, and inspection time. No snapshot means unchecked or invalidated. Watcher changes, indexing submissions and completions (including failure/cancellation), and drop invalidate it. Epoch and job checks reject scans overlapping these changes. Runtime snapshots can read this memory without scanning; explicit CLI/MCP status still reads disk and refreshes the observation. Watchers may miss external changes, so an observation is not a lasting freshness guarantee. Restart starts without an observation.

`IndexStats` contains counts and a `failed_files` list with paths and reasons; `IndexResult` also reports failed files. `IndexStatus` distinguishes unknown, uninitialized, disabled, missing, ready, stale, and failed. Queued/running/cancelled build states remain in the scheduler. CLI readiness requires a completed check with no failed, pending, added, modified, or deleted files.

FTS currently uses the fixed `FTS_CONFIG` (`jieba` tokenizer and `lowercase` filter). The same constant configures native storage and populates `IndexDescriptor` when creating or reading a workspace. It is not user-configurable or separately persisted; changes belong to the physical index format. Index info exposes it through the engine API, daemon protocol, CLI status, and MCP status.

## File failure semantics

All embedding inputs belonging to a file finish before any replacement is submitted. A successful replacement publishes the complete file. A file preparation or embedding failure removes all prior entities and all model projections for that file, retains its file record with a failure reason, and makes it eligible for a complete retry. Failed files have no searchable stale or partial records. A model batch can contain several files, but each file is committed or failed independently.

File replacement first writes `NotIndexed`, replaces entities and model projections, then writes `Indexed`. Deletion first writes `Deleting`, removes related records, then removes the file record. There is no separate pending journal or automatic recovery on open. Each indexing run includes unfinished files even outside an incremental change scope; missing or excluded sources are removed, and deleting records finish deletion. Failed records retain the normal retry policy. Directories are shared immutable identities and unused directories are retained.

Search may observe intermediate records. Missing file, entity, or fragment references are skipped without refilling the result limit; malformed records and storage failures still return errors. Normal checkpoints flush native collections, and writer locks still protect mutation. Rebuild publication remains an atomic generation switch, and successful publication may include failed file records.
