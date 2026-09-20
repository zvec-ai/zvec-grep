# zg-engine

`zg-engine` is the Rust library for indexing files and retrieving content through lexical and vector search.

## Usage

Work in progress. Usage examples will be added once the engine is stable.

## Behavior

**Generated files.** Each workspace keeps its state under `.zvec-grep` in the source root:

- `manifest.json` stores workspace configuration and identifies the active index.
- `generations/` holds index data; `build.json` records storage ownership until publication and cleanup finish.
- `authorization.json` stores saved consent for remote embedding services, when granted.

The engine also uses per-user files: `~/.zvec-grep/workspaces.json` registers workspace names and locations, and `~/.zvec-grep/config.json` stores settings when saved. Saved remote consent uses a signing key at `~/.zvec-grep/authorization.key` by default. Lock and recovery files are managed automatically.

**Models.** Local models download missing assets on first use and process content locally. On macOS and Linux, the default model cache is `~/.zvec-grep/models`. Set `ZVEC_GREP_MODEL_CACHE` to choose a cache directory. Remote models send content or queries to the configured embedding endpoint and require authorization.

**Storage and embedding.** This version supports one text embedding model per workspace. A generation contains `directories`, `files`, `entities`, and one `fragments_<fingerprint>` collection for that model. Each entity stores one complete source content object, its source location, metadata, and its fragments. A fragment has its own ID and one range relative to its entity's content. Its source location is calculated from that range and the entity's source location; it stores neither a second content payload nor duplicate source coordinates. Short entities use one full-content fragment, and long entities use source slices that preserve headers, delimiters, and whitespace. Metadata remains attached to the entity and is projected into FTS and embedding inputs; supported metadata filters have dedicated index fields.

Entities and fragments use the same `Range` type. An entity's `source_range` is relative to its file; text source locations include decoded UTF-8 byte offsets, one-based line numbers, and zero-based byte columns. A text fragment's `range` selects full content or a half-open UTF-8 byte span relative to its entity's content. Fragment line and column numbers are computed when source locations are needed. Images and tables can only use full-content fragments; raw byte source locations remain available independently.

Whitespace-only ranges remain in the complete entity content without creating search fragments. Every non-whitespace part of an extracted entity, including braces and punctuation, remains covered by its fragments.

Select the model with `zg index --embedding <model>`. Every text fragment uses that model, with both FTS and vector indexes. Images and other unsupported sources are skipped; multimodal content and content-route configuration are not supported. Changing the model requires `--rebuild`. Remote destinations require consent as usual. This entity and fragment layout uses index format 5, collection schema 4, and document codec 10.

**Index updates.** Repeated indexing reuses unchanged files, updates changed files, and removes deleted files from the index. Unsupported files are skipped. A failed file keeps its path and error but loses all searchable records; the next index retries the whole file. `drop_index` removes index data, workspace configuration, and the name reservation while preserving source files and shared model caches.

**Workspace names.** Names are case-sensitive and unique within the per-user registry; new workspaces default to the root directory's name. Use `IndexOptions::name` to choose or change a name. Move the source root together with `.zvec-grep`; the next index operation updates its registered location once the original root no longer exists. A copy of an existing workspace needs a different name.

**Rebuilds and recovery.** An incompatible index format requires an explicit rebuild with `IndexOptions::rebuild`. Every rebuild starts from empty storage. Individual file failures are recorded with paths and reasons; successful files are still published. Cancellation or a storage/finalization failure discards the build and preserves the previous active index. Ordinary `index` updates the active index, or builds a new one if none exists. Crash recovery discards unpublished storage or finishes cleanup after publication; it never resumes a build.

**File selection.** Indexing and indexed queries share ordered glob rules. Format and category filters apply only to queries and compile the catalog's filename rules into storage predicates. Extensions and special filenames match exactly as registered: Rust includes `.rs`, while JPEG explicitly includes both `.jpeg` and `.JPEG`; other case variants are not inferred. The longest registered extension takes precedence, and special filenames add format matches (specific formats suppress generic Text). `-g` and `--iglob` retain their order; the last matching rule wins, subject to excluded parent directories. `-t` / `-T` select or exclude engine formats, and `--category` / `--category-not` select or exclude categories such as `code`, `document`, and `image`. Each include list uses any-match semantics; path, format, and category constraints must all match, and format/category exclusions take precedence. HTML, for example, belongs to both code and document, so excluding document also excludes HTML.

Workspace scan rules determine index membership. Query filters only narrow the stored corpus and never modify workspace settings or read source files to identify their formats. The `--rg` path remains independent and uses ripgrep's own glob, type, ignore, and traversal behavior. Scanning options on an indexed query are rejected instead of silently ignored.

Indexing can inspect content to choose an extractor, but file records do not persist detected formats. An extensionless Python script can therefore be indexed as code while its file name does not match a Python query filter. The file-format catalog includes non-text formats for classification; recognizing a format does not imply that this version can index it.

Index requests update only supplied scan-rule fields: an empty list clears a list, `false` disables a flag, and JSON `null` removes a depth or size limit. The CLI accepts `--hidden=false`, `--no-ignore=false`, and `--follow=false`; `--reset-paths` resets all saved selection settings before applying the request. A selection update always reconciles the full workspace.

Nested Git repositories and checked-out submodules are scanned by default, subject to the same ignore rules as other directories. Set `--nested-git=false` (`scan.nested_git: false` in the SDK, `nestedGit: false` in MCP) to prune child directories containing a `.git` file or directory. This boundary also applies with `--no-ignore` and explicit globs. `--nested-git` enables traversal again; omitted updates retain the saved setting, and reset restores the default. The workspace root itself is never treated as nested. `.git` and `.zvec-grep` metadata remain excluded in either mode. To traverse an otherwise ignored directory, use `--no-ignore` or explicitly include both the directory and its descendants, for example `-g nested -g 'nested/**'`.

## Modules

- **API** defines public request and result types.
- **Service** connects engine capabilities behind `ZvecGrep`.
- **Domain** defines shared data types for workspaces, sources, content, entities, and metadata.
- **File selection** compiles domain glob rules and supplies engine-owned scan/watch policies.
- **Extraction** turns source files into complete entities, metadata, and source-preserving fragment ranges.
- **Lexical** searches source files directly with embedded grep.
- **Models** provides embedding backends and manages model runtimes.
- **Storage** persists indexed data and supports lexical and vector search with Zvec.
- **Workspace** manages workspace names, configuration, index locations, and locks.
- **Pipelines** coordinates index construction (`indexing`), indexed queries (`indexed_search`), and direct queries (`direct_search`). Direct queries combine lexical search with structural extraction and result assembly.
- **Authorization** manages consent for sending data to remote embedding services.
- **Config** manages global settings and resolves runtime configuration.
- **Error** defines engine errors and diagnostic reports.
- **Utils** provides shared helpers for text, encoding, hashing, and filesystem operations.
