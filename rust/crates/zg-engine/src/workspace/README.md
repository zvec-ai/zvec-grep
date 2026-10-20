# Workspace persistence

`workspace` connects domain workspace state to the filesystem. It owns the name registry, manifest encoding, physical index locations, build publication and recovery, and workspace locks. The domain owns `Workspace`, `ScanRules`, `IndexState`, `IndexDescriptor`, and `EmbeddingModelInfo`; the engine's `file_selection` capability implements scanning rules. API result views are assembled at the application boundary.

## Locations, names and relocation

A workspace location pairs a source root with an index home. The default home is `<root>/.zvec-grep`; API `index_dir`, CLI `--index-dir` / `ZVEC_GREP_INDEX_DIR`, and MCP `indexDir` select an explicit home. The selected home's manifest is authoritative. Names are validated labels and need not be unique across workspaces. The per-user registry is an optional discovery cache; stale or unavailable registry data does not prevent using a workspace.

The manifest stores `sourceRoot` relative to its home. Source paths are encoded as components relative to the source root, and additional ignore-file references are relative to the home. Moving a source and index bundle together preserves these relationships. Reads follow the recorded source and never silently bind the index to a different directory. If the source moves independently, an explicit index operation with both a source root and index home repairs the binding. A live watcher holds a source-binding lease, so rebinding must wait until it closes.

```sh
zg --index /path/to/materials --index-dir /path/to/search-data
zg --index-dir /path/to/search-data "authentication"
zg --status --index-dir /path/to/search-data
```

The scan and direct-search walkers exclude the selected index home, recognizable workspace index directories and their symlink aliases. Multiple indexes under one source tree cannot ingest each other's persisted data. Explicitly selecting a missing or damaged index never falls back to another index.

The active index lives at `<home>/index`. A writer loads a path-to-ID cache from file records once; unchanged files reuse their recorded IDs, and only previously unrecorded paths need allocation. Reservations stay in memory until their `NotIndexed` records are written. File and directory IDs remain separate checked `u32` spaces.

File IDs remain unchanged while a record is updated, renamed at the workspace level, or relocated with its index. Rebuilds can assign different IDs. Deletion ends a record's identity; IDs may be reused after reopening once the old record is gone. Entity and fragment IDs are therefore references within one physical generation, not durable external handles. Directory filters use indexed arrays of `u32` directory IDs on file and search documents. The `directories` collection stores directory ID, parent ID and relative path components; memory maps accelerate lookup.

## Canonical data and model indexes

Each generation contains three shared collections: `directories`, `files`, and `entities`. Each entity stores its original content once, its `source_range` within the file, and every one of its globally unique fragments. A fragment contains only an independent ID and a `range` relative to the entity content. Both locations use the same `Range` type. An entity source location can be `Full`, `Text` with complete text coordinates, or `Byte`. A fragment uses `Full` for the entire content or `Byte` with only `start_offset` and `end_offset` in the entity's decoded UTF-8 text. Partial text fragments never store lines or columns; images and tables require `Full`. Fragment source locations are derived from the entity's source range and the fragment's relative range; no second coordinate system is stored on a fragment. Fragments have no content, file, owner, or metadata copy. The canonical entity record stores the complete fragment definitions, with metadata stored once in its own field. There are no representative or standalone fragment variants. Composite content is reserved as a future explicit extension; ordinary entities cannot contain a list of unrelated contents.

Current indexing acquires one embedding runtime for text and code. The domain descriptor represents a default embedding model and optional content-specific overrides; it does not determine the physical collection layout. Every indexed fragment belongs to the model's shared `fragments_<fingerprint>` collection, which holds the selected fragment text with the fixed FTS index, vector, canonical IDs and indexed filtering fields. There is no separate FTS collection or separate vector collection. Search loads a canonical entity through its entity ID and verifies that the hit's independent fragment ID belongs to that entity.

`--embedding` selects the workspace model. Text and code share that model's collection. Images and other unsupported sources are reported as skipped before extraction or embedding. Changing the model, vector dimension or metric requires explicit `zg --index --rebuild`; input limits, batch size, runtime credentials, concurrency and timeout changes do not require rebuilding.

FTS and vector retrieval use the same model collection. Vector queries use the model that built the index. The current version does not accept content-route configuration or query multiple embedding models.

## Index compatibility

`WorkspaceManifest` combines a domain `Workspace` with persistence-specific fields: workspace home, index version, publication generation token, and saved embedding endpoints. It does not embed an API `WorkspaceIndexInfo`.

`indexVersion` is the only compatibility version for the manifest, storage schema, and record encoding. The current Rust version is 2; Node.js indexes use version 1. The manifest records `defaultEmbeddingModel`, typed `embeddingRoutes`, saved endpoints and source selection settings. API keys, devices and model-cache paths are resolved at runtime rather than saved in the portable manifest. Remote-service grants remain bound to the local machine and source location. A current-version load validates the workspace root, model definitions, content-route capabilities and storage generation for an enabled index. Structural validation still rejects corrupt current-version data; it does not maintain another version counter.

Inspection determines compatibility before decoding current-format metadata or opening storage. Missing or different versions and invalid persisted metadata report `rebuild_required`, including actual and expected versions and the reason. Search, incremental indexing, and watcher activation reject that state. An unbuilt workspace remains distinct from incompatible persisted data. An explicit rebuild starts with the requested embedding model or the configured default and new scan settings; it does not migrate an incompatible manifest. Prior Rust development formats do not need migration support.

The storage generation UUID is a logical publication token, not a directory name or a compatibility version. Readers and writers use it to distinguish successive indexes at the same physical path. Ordinary incremental indexing updates the active index in place.

## Publication and recovery

First builds and rebuilds use empty storage at `<home>/temp/new`. A durable `temp/build.json` records the attempt's generation, previous manifest fingerprint and publication state. Under the home write lock, publication moves the old index to `temp/old`, installs the new index at `index`, and atomically replaces the manifest to commit. Recovery restores the prior index if the manifest was not committed, or finishes cleanup after a committed publication. A moved bundle can recover using the same relative layout.

Per-file extraction or embedding failures do not prevent publishing successful files. Cancellation or a failure before commit preserves the previous index. Cleanup failure after commit leaves the recovery record for the next writer and does not turn a successful build into failure. Explicit rebuilds start a fresh attempt; ordinary indexing discards abandoned staging rather than resuming an incomplete rebuild.

Home locks protect physical publication; source-binding leases prevent concurrent rebinding. Storage remains organized by the shared `directories`, `files`, `entities` and model `fragments` tables. `IndexStore` coordinates their leases, file transitions and search-result loading. The portable layout does not split collections by content kind.

## Persisted state and runtime observations

`Workspace.index` is `Uninitialized`, `Disabled`, or `Enabled(IndexDescriptor)`. Enabled always carries the embedding model information and fixed FTS configuration; physical format and publication layout remain owned by `WorkspaceManifest`. A first build or rebuild keeps its target in `WorkspaceBuild` until publication. The selected descriptor does not imply that files are up to date or that every file succeeded. Every entity uses the workspace model, while file completion covers all of its entities.

The daemon's `WorkspaceRuntime` holds an optional `IndexStatusSnapshot`: observed health, file statistics, and inspection time. No snapshot means unchecked or invalidated. Watcher changes, indexing submissions and completions (including failure/cancellation), and drop invalidate it. Epoch and job checks reject scans overlapping these changes. Runtime snapshots can read this memory without scanning; explicit CLI/MCP status still reads disk and refreshes the observation. Watchers may miss external changes, so an observation is not a lasting freshness guarantee. Restart starts without an observation.

`IndexStats` contains counts and a `failed_files` list with paths and reasons; `IndexResult` also reports failed files. `IndexStatus` distinguishes unknown, uninitialized, disabled, missing, ready, stale, and failed. Queued/running/cancelled build states remain in the scheduler. CLI readiness requires a completed check with no failed, pending, added, modified, or deleted files.

FTS currently uses the fixed `FTS_CONFIG` (`jieba` tokenizer and `lowercase` filter). The same constant configures native storage and populates `IndexDescriptor` when creating or reading a workspace. It is not user-configurable or separately persisted; changes belong to the physical index format. Index info exposes it through the engine API, daemon protocol, CLI status, and MCP status.

## File failure semantics

All embedding inputs belonging to a file finish before any replacement is submitted. A successful replacement publishes the complete file. A file preparation or embedding failure removes all prior entities and all model projections for that file, retains its file record with a failure reason, and makes it eligible for a complete retry. Failed files have no searchable stale or partial records. A model batch can contain several files, but each file is committed or failed independently.

File replacement first writes `NotIndexed`, replaces entities and model projections, then writes `Indexed`. Deletion first writes `Deleting`, removes related records, then removes the file record. There is no separate pending journal or automatic recovery on open. Each indexing run includes unfinished files even outside an incremental change scope; missing or excluded sources are removed, and deleting records finish deletion. Failed records retain the normal retry policy. Directories are shared immutable identities and unused directories are retained.

Search may observe intermediate records. Missing file, entity, or fragment references are skipped without refilling the result limit; malformed records and storage failures still return errors. Normal checkpoints flush native collections, and writer locks still protect mutation. Rebuild publication commits through atomic manifest replacement under the home lock, and successful publication may include failed file records.
