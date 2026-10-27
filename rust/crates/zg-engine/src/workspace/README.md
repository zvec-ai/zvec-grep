# Workspace persistence

`workspace` connects domain workspace state to the filesystem. It owns manifest encoding, build publication and recovery, saved credentials, and workspace locks. The domain owns `Workspace`, `WorkspaceLayout`, `ScanRules`, `IndexState`, `IndexDescriptor`, and `EmbeddingModelInfo`; the engine's `file_selection` capability implements scanning rules. API result views are assembled at the application boundary.

Each persistence module reads, writes, and removes its own data; the indexing service coordinates these operations under the workspace lock, including index removal. The indexing pipeline handles file scanning and processing. Remote-consent decisions and signed grants remain in the engine's separate `authorization` module.

| Module | Responsibility |
| --- | --- |
| `mod.rs` | Find the nearest enclosing workspace. |
| `manifest.rs` | Encode, validate, and persist workspace settings, index metadata, and saved model configuration. |
| `index.rs` | Prepare staged index storage, publish it, recover interrupted publication, and remove index storage. |
| `credentials.rs` | Encrypt and decrypt API keys and manage the workspace encryption key. |
| `lock.rs` | Own file lock handles and shared/exclusive file locking. |
| `lock/access.rs` | Coordinate workspace access, including writer priority, cancellation, timeouts, read caches, and recovery before reads. |

Workspace lock coordination stays private to `lock`; callers use its exported operations. `mod.rs` also declares these modules and the index compatibility version. General directory resolution, creation, file and directory queries, and symlink checks live in `utils/filesystem.rs`.

## Workspace roots

A workspace is identified solely by its root path. Requests require an existing source directory and resolve it to a canonical path; the daemon uses that path as the key for resident runtimes. Different directories with the same basename are independent workspaces. There is no workspace name, alias, or per-user registration.

The engine does not read or write `~/.zvec-grep/workspaces.json` or its lock, and `ZVEC_GREP_WORKSPACE_REGISTRY` is no longer used. Legacy registry files are ignored and may be removed. Indexing and dropping an index only change the selected workspace's local state.

## Directory layout

`WorkspaceLayout` in `domain/workspace.rs` holds `source_root`, `storage_dir`, and complete paths for the manifest, index, build state, locks, credentials, and authorization files. Derived directory fields use `_dir`; file fields use `_path`. Paths are fixed at construction and exposed through read-only `&Path` accessors. `has_manifest`, `has_build_record`, and `has_index_data` return false for missing entries and report errors for symbolic links, incorrect entry types, or inaccessible paths. Index data includes active and staged manifests, the build record, and active, staged, or backup index directories; credentials, authorization, and locks alone do not count. Manifest creation and loading reuse the caller's layout; writes take an explicit destination path to support staged publication.

```text
workspace/                         source_root: source directory
├── src/                           example source files
└── .zvec-grep/                     storage_dir: workspace metadata and index
    ├── manifest.json              manifest_path: saved settings and active generation
    ├── index/                     index_dir: active index data
    ├── credentials.key            optional key for saved encrypted API credentials
    ├── authorization.json         optional signed remote-consent records
    ├── authorization.lock         coordination for authorization updates
    ├── temp/                      temp_dir: first-build, rebuild, and recovery state
    │   ├── build.json             build_record_path: publication and recovery record
    │   ├── manifest.json          staged_manifest_path: manifest awaiting publication
    │   ├── new/                   staged_index_dir: replacement index under construction
    │   └── old/                   backup_index_dir: previous index retained during publication
    └── locks/                     locks_dir: coordination files retained across index resets
        ├── home                   workspace_lock_path: workspace read/write lock
        ├── write-intent           admission priority for waiting writers
        └── read-cache             resident storage-handle coordination
```

The tree shows possible entries, not files that must all exist at once. Temporary entries appear at different build stages and are normally removed after publication or recovery; downloaded models are cached separately. The manifest's `sourceRoot: ".."` refers to the parent of `storage_dir`.

Workspace data directories use the system's default creation permissions; existing permissions are preserved. Credential files retain their separate permission restrictions.

Constructing a layout requires an absolute source root and checks that `storage_dir`, if present, is an ordinary directory rather than a symbolic link. Construction creates nothing and does not inspect the internal entries. Ordinary reads, writes, and deletion handle errors from the paths they access; cached sessions and locks also check current filesystem state when coordinating access. Nearest-workspace discovery walks upward and selects the first directory containing `.zvec-grep/manifest.json` or `.zvec-grep/temp/build.json`, including an interrupted first build; the caller then checks its state. Indexing and dropping an index use the selected root directly, while search and status can discover an enclosing workspace.

## Relocation

Move or copy the root together with its `.zvec-grep` directory to retain workspace state. The engine manages the contents of `.zvec-grep`; manually rearranging its entries or redirecting them with symbolic links is unsupported. Replacing the storage directory while a layout is in use is also unsupported; construction checks its initial state, not its future identity. The manifest records the source as `..`, and reads resolve paths against the current root. It stores no absolute source or index location. There is no source rebinding operation or configurable index directory in this implementation.

File and directory identities use separately encoded path components. Unicode names can move across platforms; native non-Unicode names remain platform-specific. Invalid path components and source-path collisions are rejected. Ignore files within the workspace retain relative paths; external ignore files are explicit platform-specific dependencies and must remain available or be replaced with `--ignore-file` or `--reset-paths`.

File IDs belong to one physical index generation. A writer loads the path-to-ID mapping once, and unchanged files reuse their IDs. The allocation upper bound is persisted in `index/file-ids.json` before reserving IDs, so a restart or deletion cannot reuse them. Rebuilds may assign different IDs. Directory identities use their own checked `u32` allocation space and are retained when unused. Entity and fragment IDs remain references within one physical index, not durable external handles.

## Canonical data and model indexes

Each index contains three shared collections: `directories`, `files`, and `entities`. Each entity stores its original content once, its `source_range` within the file, and every one of its globally unique fragments. A fragment contains only an independent ID and a `range` relative to the entity content. Both locations use the same `Range` type. An entity source location can be `Full`, `Text` with complete text coordinates, or `Byte`. A fragment uses `Full` for the entire content or `Byte` with only `start_offset` and `end_offset` in the entity's decoded UTF-8 text. Partial text fragments never store lines or columns; images and tables require `Full`. Fragment source locations are derived from the entity's source range and the fragment's relative range; no second coordinate system is stored on a fragment. Fragments have no content, file, owner, or metadata copy. The canonical entity record stores the complete fragment definitions, with metadata stored once in its own field. There are no representative or standalone fragment variants. Composite content is reserved as a future explicit extension; ordinary entities cannot contain a list of unrelated contents.

Current indexing acquires one embedding runtime for text and code. The domain descriptor represents a default embedding model and optional content-specific overrides; it does not determine the physical collection layout. Every indexed fragment belongs to the model's shared `fragments_<fingerprint>` collection, which holds the selected fragment text with the fixed FTS index, vector, canonical IDs and indexed filtering fields. There is no separate FTS collection or separate vector collection. Search loads a canonical entity through its entity ID and verifies that the hit's independent fragment ID belongs to that entity.

`--embedding` selects the workspace model. Text and code share that model's collection. Images and other unsupported sources are reported as skipped before extraction or embedding. Changing the model, vector dimension or metric requires explicit `zg --index --rebuild`; input limits, batch size, runtime credentials, concurrency and timeout changes do not require rebuilding.

FTS and vector retrieval use the same model collection. Vector queries use the model that built the index. The current version does not accept content-route configuration or query multiple embedding models.

## Index compatibility

The domain `Workspace` owns the index version and storage generation; its `IndexDescriptor` contains model information and saved model configurations. Manifest reads return a `Workspace`, and writes encode it through a private disk representation. The workspace owns its immutable layout, which is the single source of its root and storage paths. Discovery and locking obtain the layout first; loading or creating workspace state then attaches it to a `Workspace`. The layout is not serialized. Saved settings use `ModelConfig<EncryptedApiKey>`; model execution uses the same configuration type with plaintext keys resolved only when needed. The manifest does not embed an API `WorkspaceIndexInfo`.

`indexVersion` is the only compatibility version for the manifest, storage schema, and record encoding. The current Rust version is 2; Node.js indexes use version 1. The manifest records `defaultEmbeddingModel`, typed `embeddingRoutes`, saved endpoint/encrypted credentials and source selection settings. A current-version load validates the workspace root, model definitions, content-route capabilities and storage generation for an enabled index. Structural validation still rejects corrupt current-version data; it does not maintain another version counter.

Inspection determines compatibility before decoding current-format metadata or opening storage. Missing or different versions and invalid persisted metadata report `rebuild_required`, including actual and expected versions and the reason. Search, incremental indexing, and watcher activation reject that state. An unbuilt workspace remains distinct from incompatible persisted data. An explicit rebuild starts with the requested embedding model or the configured default and new scan settings; it does not migrate an incompatible manifest. Prior Rust development formats do not need migration support.

Generation UUIDs identify publications, not physical directories. The active index always lives in `.zvec-grep/index`. Ordinary incremental indexing updates it in place; it does not create immutable snapshots or introduce another compatibility version.

## Publication and recovery

First builds and rebuilds use `.zvec-grep/temp/new`. A durable `temp/build.json` records the new generation, the previous manifest fingerprint, whether an index existed, and whether publication has begun. Once storage has been finalized and closed, publication moves the current `index` to `temp/old`, moves `temp/new` to `index`, and atomically replaces the manifest. That replacement commits the new generation.

Readers and writers share the workspace lock. Before returning a read lock, admission detects an interrupted swap and obtains the write lock to restore the previous index. Recovery can be interrupted and retried. Before the manifest commits, failed or cancelled builds preserve the previous index and manifest; after it commits, cleanup failure leaves the record for the next writer to retry. Unexpected manifest changes prevent destructive recovery. Cleanup only removes current-layout index and build data; directories left by older layouts are not inspected or removed. Per-file extraction or embedding failures can still publish successful files; cancellation and scan/storage failures abort the build.

The indexing application service acquires the workspace lock before mutating its local metadata. Storage is organized by table: `directories`, `files`, `entities`, and `fragments` own their schemas, codecs, and collection operations. `IndexStore` holds the shared leases and locks and coordinates cross-table writes, file status transitions, and search-result loading. The file ID cache belongs to the file table; these mechanisms remain separate from workspace lifecycle.

## Persisted state and runtime observations

`Workspace.index` is `Uninitialized`, `Disabled`, or `Enabled(IndexDescriptor)`. Enabled always carries the embedding model information, saved connection settings, and fixed FTS configuration; workspace persistence manages the physical format using paths defined by the domain's `WorkspaceLayout`. A first build or rebuild uses a target `Workspace` with a new generation; the private on-disk build record tracks publication and recovery. The selected descriptor does not imply that files are up to date or that every file succeeded. Every entity uses the workspace model, while file completion covers all of its entities.

The daemon starts with no resident workspaces and admits runtimes on demand for requests. Its in-memory runtime map is keyed by canonical root. Foreground requests renew the idle deadline; active requests and queued/running index jobs prevent retirement. After the idle timeout (four hours by default, configured by `ZVEC_GREP_WATCHER_IDLE_TIMEOUT_SECONDS`), maintenance removes the runtime and its job history and closes its watcher. Setting the timeout to `0` disables idle eviction. Restart does not restore a workspace list.

The daemon's `WorkspaceRuntime` holds an optional `IndexStatusSnapshot`: observed health, file statistics, and inspection time. No snapshot means unchecked or invalidated. Watcher changes, indexing submissions and completions (including failure/cancellation), and drop invalidate it. Epoch and job checks reject scans overlapping these changes. Runtime snapshots can read this memory without scanning; explicit CLI/MCP status still reads disk and refreshes the observation. Watchers may miss external changes, so an observation is not a lasting freshness guarantee. Restart starts without an observation.

`IndexStats` contains counts and a `failed_files` list with paths and reasons; `IndexResult` also reports failed files. `IndexStatus` distinguishes unknown, uninitialized, disabled, missing, ready, stale, and failed. Queued/running/cancelled build states remain in the scheduler. CLI readiness requires a completed check with no failed, pending, added, modified, or deleted files.

FTS currently uses the fixed `FTS_CONFIG` (`jieba` tokenizer and `lowercase` filter). The same constant configures native storage and populates `IndexDescriptor` when creating or reading a workspace. It is not user-configurable or separately persisted; changes belong to the physical index format. Index info exposes it through the engine API, daemon protocol, CLI status, and MCP status.

## File failure semantics

All embedding inputs belonging to a file finish before any replacement is submitted. A successful replacement publishes the complete file. An ordinary file preparation or embedding failure removes all prior entities and all model projections for that file, retains its file record with a failure reason, and makes it eligible for a complete retry. Failed files have no searchable stale or partial records. Cancellation during preparation preserves the previous file contents. An incomplete scan aborts before applying deletions. A model batch can contain several files, but each file is committed or failed independently.

File replacement first writes `NotIndexed`, replaces entities and model projections, then writes `Indexed`. Deletion first writes `Deleting`, removes related records, then removes the file record. There is no separate pending journal or automatic recovery on open. Each indexing run includes unfinished files even outside an incremental change scope; missing or excluded sources are removed, and deleting records finish deletion. Failed records retain the normal retry policy. Directories are shared immutable identities and unused directories are retained.

Search may observe intermediate records. Missing file, entity, or fragment references are skipped without refilling the result limit; malformed records and storage failures still return errors. Normal checkpoints flush native collections, and writer locks still protect mutation. Rebuild publication commits a generation through an atomic manifest replacement, and successful publication may include failed file records.

## Saved model settings and credentials

The manifest stores model identity, endpoint, and explicitly saved encrypted API keys. Device and model-cache settings belong to the current machine and are resolved for each operation. Keys inherited from environment variables or global configuration are not copied into a workspace, and temporary query overrides are not persisted through refresh or watcher activity.

API keys use XChaCha20-Poly1305 with a random per-secret nonce and the model reference as associated data. `credentials.key` is created privately and is never rotated while preparing a replacement configuration. Saved credentials change only when the manifest is successfully written. Copying the complete workspace also copies its decryption key, so sharing the directory shares access to those credentials. Remote authorization is separate from having a usable key and must be reconfirmed after relocation or a machine change.
