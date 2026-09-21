# Workspace persistence

`workspace` connects domain workspace state to the filesystem. It owns the name registry, manifest encoding, physical index locations, build publication and recovery, and workspace locks. The domain owns `Workspace`, `ScanRules`, `IndexState`, `IndexDescriptor`, and `EmbeddingModelInfo`; the engine's `file_selection` capability implements scanning rules. API result views are assembled at the application boundary.

## Names and the registry

The name is the sole per-user workspace identity. Names are stored as strings and validated by `Workspace::validate_name` at request, manifest, and registry boundaries. Names must be nonempty, must not be `.` or `..`, and must not contain surrounding whitespace, path separators, or control characters. Names are case-sensitive and unique across the registry, even when source directories are unrelated. A root directory can have one registered name. New workspaces use the root basename unless the caller supplies `IndexOptions::name`, CLI `--name`, or MCP `name`. A basename collision requires an explicit different name.

The default registry is `~/.zvec-grep/workspaces.json`. Set `ZVEC_GREP_WORKSPACE_REGISTRY` to an absolute file path to select another registry, including an isolated registry for tests. Registry mutations use a separate lock and atomic replacement. Lookup does not create the registry or its lock file.

Indexing reserves a name before the first build. A failed or interrupted build keeps the reservation so a retry retains its identity. Dropping the workspace index releases the reservation, including a reservation left by an unsuccessful first build, and preserves source files. If the source directory was deleted externally, an explicit drop at its previous path releases the name without recreating that directory.

## Renaming and relocation

An explicit new name on an existing workspace renames its registry entry:

```sh
zg index /path/to/workspace --name search-engine
```

The registry is authoritative. Read-only `info` and `context` report its current name without writing metadata. The next index operation reconciles a stale manifest with the registry. Renaming itself retains file IDs and active storage and does not require a rebuild.

Move the root together with its `.zvec-grep` directory to retain workspace state. Reads resolve source paths against the new root. The next index operation updates the registered location after checking that the original root directory no longer exists. A live copy cannot claim the original's name; index the copy with its own `--name`.

The name identifies the workspace across these operations. The root records its current location. File IDs belong to source records in one physical index generation. A writer loads a path-to-ID cache from these records once; unchanged files reuse their recorded IDs, and only previously unrecorded paths need allocation. Reservations stay in memory until their `NotIndexed` file records are written. Directory identities are stored in the `directories` collection; file IDs remain owned by file records. Allocation caches live in memory; interrupted work is represented by file status. File and directory IDs are separate `u32` spaces; checked allocation rejects exhaustion without wrapping or partially reserving a batch.

File IDs remain unchanged while a record is updated, renamed at the workspace level, or relocated with its index. Rebuilds can assign different IDs. Deletion ends a record's identity; IDs may be reused after reopening once the old record is gone. Entity and fragment IDs are therefore references within one physical generation, not durable external handles. Directory filters use indexed arrays of `u32` directory IDs on file and search documents. The `directories` collection stores directory ID, parent ID and native path; memory maps accelerate lookup.

## Canonical data and model indexes

Each generation contains three shared collections: `directories`, `files`, and `entities`. Each entity stores its original content once, its `source_range` within the file, and every one of its globally unique fragments. A fragment contains only an independent ID and a `range` relative to the entity content. Both locations use the same `Range` type. An entity source location can be `Full`, `Text` with complete text coordinates, or `Byte`. A fragment uses `Full` for the entire content or `Byte` with only `start_offset` and `end_offset` in the entity's decoded UTF-8 text. Partial text fragments never store lines or columns; images and tables require `Full`. Fragment source locations are derived from the entity's source range and the fragment's relative range; no second coordinate system is stored on a fragment. Fragments have no content, file, owner, or metadata copy. The canonical entity record stores the complete fragment definitions, with metadata stored once in its own field. There are no representative or standalone fragment variants. Composite content is reserved as a future explicit extension; ordinary entities cannot contain a list of unrelated contents.

This version enables exactly one embedding model and one `text` route per workspace. Every fragment belongs to that model's `fragments_<fingerprint>` collection, which holds the selected fragment text with the fixed FTS index, vector, canonical IDs and indexed filtering fields. There is no separate FTS collection or separate vector collection. Search loads a canonical entity through its entity ID and verifies that the hit's independent fragment ID belongs to that entity.

`--embedding` selects the workspace model. Images and other unsupported sources are reported as skipped before extraction or embedding. Changing the model, vector compatibility or input limits requires explicit `index --rebuild`; runtime credentials, concurrency and timeout changes do not require rebuilding.

FTS and vector retrieval use the same model collection. Vector queries use the model that built the index. The current version does not accept content-route configuration or query multiple embedding models.

## Index compatibility

`WorkspaceManifest` combines a domain `Workspace` with persistence-specific fields: workspace home, index version, active storage generation, and embedding runtime configuration. It does not embed an API `WorkspaceIndexInfo`.

`indexVersion` is the only compatibility version for the manifest, storage schema, and record encoding. The current Rust version is 2; Node.js indexes use version 1. The manifest records one embedding model, its text route and runtime configuration, along with source selection settings. A current-version load validates the required workspace root, single model, text-only route and storage generation for an enabled index. Structural validation still rejects corrupt current-version data; it does not maintain another version counter.

Inspection determines compatibility before decoding current-format metadata or opening storage. Missing or different versions and invalid persisted metadata report `rebuild_required`, including actual and expected versions and the reason. Search, incremental indexing, and watcher activation reject that state. An unbuilt workspace remains distinct from incompatible persisted data. An explicit rebuild starts with the requested embedding model or the configured default and new scan settings; it does not migrate an incompatible manifest. Prior Rust development formats do not need migration support.

Generation-directory UUIDs only identify storage for publication and cleanup. Ordinary incremental indexing updates the active physical generation in place; it does not create immutable snapshots or introduce another compatibility version.

## Publication and recovery

First builds and rebuilds always start in empty storage under `.zvec-grep/generations`. A durable `build.json` records only the temporary generation, workspace name, and root; it has no format version or embedded manifest. Recovery reads the active generation from the manifest without decoding its index configuration. An atomic manifest replacement selects the completed index; cleanup removes superseded generations and Node.js collections afterward. Per-file extraction or embedding failures are recorded and do not prevent publication of the successful files. Cancellation, storage mutation failure, or failure to finalize/close storage discards the new generation and preserves the active index. Every explicit rebuild starts over, including after an interruption. Ordinary indexing updates the active index and never resumes an abandoned rebuild. Crash recovery uses the manifest selection to discard unpublished storage or finish post-publication cleanup. Cleanup failure after publication does not turn a successful rebuild into a failed operation; the next writer retries it.

The indexing application service acquires the workspace lock before mutating its registry entry and metadata. The registry's lock protects names across independent workspace roots. Storage is organized by table: `directories`, `files`, `entities`, and `fragments` own their schemas, codecs, and collection operations. `IndexStore` holds the shared leases and locks and coordinates cross-table writes, file status transitions, and search-result loading. The file ID cache belongs to the file table; these mechanisms remain separate from workspace naming.

## Persisted state and runtime observations

`Workspace.index` is `Uninitialized`, `Disabled`, or `Enabled(IndexDescriptor)`. Enabled always carries the embedding model information and fixed FTS configuration; physical format and generation-directory layout remain owned by `WorkspaceManifest`. A first build or rebuild keeps its target in `WorkspaceBuild` until publication. The selected descriptor does not imply that files are up to date or that every file succeeded. Every entity uses the workspace model, while file completion covers all of its entities.

The daemon's `WorkspaceRuntime` holds an optional `IndexStatusSnapshot`: observed health, file statistics, and inspection time. No snapshot means unchecked or invalidated. Watcher changes, indexing submissions and completions (including failure/cancellation), and drop invalidate it. Epoch and job checks reject scans overlapping these changes. Runtime snapshots can read this memory without scanning; explicit CLI/MCP status still reads disk and refreshes the observation. Watchers may miss external changes, so an observation is not a lasting freshness guarantee. Restart starts without an observation.

`IndexStats` contains counts and a `failed_files` list with paths and reasons; `IndexResult` also reports failed files. `IndexStatus` distinguishes unknown, uninitialized, disabled, missing, ready, stale, and failed. Queued/running/cancelled build states remain in the scheduler. CLI readiness requires a completed check with no failed, pending, added, modified, or deleted files.

FTS currently uses the fixed `FTS_CONFIG` (`jieba` tokenizer and `lowercase` filter). The same constant configures native storage and populates `IndexDescriptor` when creating or reading a workspace. It is not user-configurable or separately persisted; changes belong to the physical index format. Index info exposes it through the engine API, daemon protocol, CLI status, and MCP status.

## File failure semantics

All embedding inputs belonging to a file finish before any replacement is submitted. A successful replacement publishes the complete file. A file preparation or embedding failure removes all prior entities and all model projections for that file, retains its file record with a failure reason, and makes it eligible for a complete retry. Failed files have no searchable stale or partial records. A model batch can contain several files, but each file is committed or failed independently.

File replacement first writes `NotIndexed`, replaces entities and model projections, then writes `Indexed`. Deletion first writes `Deleting`, removes related records, then removes the file record. There is no separate pending journal or automatic recovery on open. Each indexing run includes unfinished files even outside an incremental change scope; missing or excluded sources are removed, and deleting records finish deletion. Failed records retain the normal retry policy. Directories are shared immutable identities and unused directories are retained.

Search may observe intermediate records. Missing file, entity, or fragment references are skipped without refilling the result limit; malformed records and storage failures still return errors. Normal checkpoints flush native collections, and writer locks still protect mutation. Rebuild publication remains an atomic generation switch, and successful publication may include failed file records.
