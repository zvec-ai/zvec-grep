# Retrieval pipeline

[Documentation](./README.md) · [Agents](./01-agents.md) ·
[CLI](./02-cli.md) · [MCP](./03-mcp.md) · [Pipeline](./04-pipeline.md) ·
[Architecture](./05-architecture.md) · [Server](./06-server.md) ·
[Embedding](./07-embedding.md) · [Roadmap](./08-roadmap.md)

zg provides one search layer across semantic discovery, ranked lexical search,
and exhaustive ripgrep. The normal flow is:

```text
workspace → file discovery → local index → query routes → compact results
```

The index combines lexical and vector retrieval. Managed ripgrep follows the
same workspace and output conventions but can run without an index.

## 1. Choose the workspace scope

For the common case, run a search from the repository root. Ordinary searches
first check live files. Exact file and code-symbol lookups can return without an
index or model. Other searches implicitly start or reuse the local background
server and prepare a local index when needed:

```bash
cd your-repository
zg "where authentication is validated"
```

Literal matches return while initial indexing continues in the background. If
the index is not usable and a default `auto` query has no literal matches, a lazy,
bounded keyword scan runs before waiting for readiness. Useful hits return as
`matchedBy=keyword` with an incomplete-coverage warning while the local build
continues; an existing job is reused. This can omit semantic neighbors.

With neither literal nor keyword evidence, initial preparation still gets up to
eight seconds: search the index if ready, otherwise report incomplete coverage.
This is not a total CLI latency limit. Long builds and model downloads continue
after the command exits; an unfinished semantic search is not an exhaustive
absence of matches. Ready-index retrieval, explicit `--hybrid` / `--vector`, and
`--refresh wait` do not use this cold keyword shortcut. The default model is
unchanged.

Use `--index` when you need to select a model or constrain the workspace before
the first search:

```bash
cd your-repository
zg --index --embedding local/potion-code-16m-v2

# Equivalent with an explicit root
zg --index /absolute/path/to/your-repository \
  --embedding local/potion-code-16m-v2
```

The workspace index is stored under `<root>/.zvec-grep/`. `.git` and
`.zvec-grep` are always excluded. Common dependency, build, generated, cache,
and log directories are excluded by default, as are files ignored by the
repository's ignore rules.

The main workspace files are `manifest.json`, `files.zvec`, and `index.zvec`.
The manifest stores index metadata and the workspace Embedding runtime settings,
including an API key when one was explicitly persisted for that workspace.

Scope large repositories early:

```bash
zg --index \
  --embedding local/potion-code-16m-v2 \
  -g "src/**" \
  -g "docs/**" \
  -g "!dist/**" \
  -t ts
```

Useful discovery controls include:

| Option | Effect |
| --- | --- |
| `-g, --glob <glob>` | Add an ordered include or `!` exclude rule |
| `--iglob <glob>` | Add a case-insensitive glob rule |
| `-t, --type <type>` | Include a ripgrep file type |
| `-T, --type-not <type>` | Exclude a ripgrep file type |
| `--hidden` | Include hidden paths except `.git` and `.zvec-grep` |
| `--no-ignore` | Stop applying ignore files |
| `--ignore-file <path>` | Add an ignore file |
| `--max-depth <n>` | Limit recursive depth |
| `--max-filesize <size>` | Limit file size, for example `500K` or `2M` |
| `-L, --follow` | Follow symbolic links safely |

File-type filters narrow the result after glob rules. For example,
`-g "docs/**" -t ts` selects TypeScript files inside `docs`, not every file in
that directory.

Without an explicit `--max-filesize`, indexing uses type-aware safety limits:
1 MiB for code, 256 MiB for text and Markdown, 16 MiB for structured data, and
10 MiB for images. An explicit value replaces the type-aware defaults for every
selected file. Files excluded by these limits remain silent during normal
indexing; use `zg --index --debug` to print skipped-file counts and samples.

### Supported formats and extraction

The scanner assigns each admitted file to one extraction path. Structure-aware
extractors preserve useful code symbols or Markdown sections. When structure is
not available, zg falls back to plain-text chunks so the file can still
participate in indexed search.

| Files | Formats | Extractor | Indexed representation |
| --- | --- | --- | --- |
| Structure-aware code | C/C++ (`.c`, `.cc`, `.cpp`, `.cxx`, `.h`, `.hpp`), Go, Java, JavaScript/JSX, TypeScript/TSX, Python, Rust | `CodeExtractor` | Symbols, signatures, breadcrumbs, and surrounding source |
| Component files | `.vue`, `.svelte` | `CodeExtractor` | Structured JavaScript or TypeScript `<script>` blocks plus source chunks for templates and other uncovered content |
| Other recognized code | Ruby, PHP, Swift, Kotlin, C#, Scala, shell, SQL, CSS/SCSS/Less, `Dockerfile`, `Makefile` | `CodeExtractor` | Plain-text chunks until a structural grammar is available |
| Markdown | `.md`, `.mdx` | `MarkdownExtractor` | Heading sections and breadcrumbs; plain-text fallback for documents without headings |
| Text documents | `.txt`, `.rst`, `.html`, `.htm`, `.xml` | `TextExtractor` | Plain-text chunks |
| Text data | `.csv`, `.json`, `.jsonc`, `.toml`, `.yaml`, `.yml` | `TextExtractor` | Plain-text chunks |
| Other non-binary files | Unrecognized extensions that pass binary detection | `TextExtractor` | Plain-text chunks |
| Raster images | `.gif`, `.jpeg`, `.jpg`, `.png`, `.webp` | `ImageExtractor` | Image content when explicitly included and the selected Embedding model accepts images |

Recognizing a code symbol does not exclude the rest of its file. Bounded source
chunks cover top-level constants, imports, initialization, and other content
outside structured fragments. Outline ranges do not count as source coverage;
only actual source windows do. Whitespace- or punctuation-only gaps are skipped,
and all added chunks retain their original source positions.

JavaScript and TypeScript also recognize function-valued assignments such as
`res.sendFile = function (...) { ... }` and
`module.exports.sendFile = (...) => ...` as definitions, including in JSX/TSX
and component script blocks. The
binding name and receiver are preserved for definition-first lookup. Dynamic
property names and calls that merely accept a callback are not inferred to be
assigned function definitions; their source remains searchable.

Raster images are excluded by the default discovery rules and must be selected
explicitly. A text-only Embedding model cannot add image fragments to its
vector index.

The following binary formats are currently skipped before extraction:

- documents: `.pdf`, `.doc`, `.docx`, `.ppt`, `.pptx`, `.xls`, `.xlsx`;
- archives: `.zip`, `.tar`, `.gz`, `.bz2`, `.xz`, `.7z`, `.rar`;
- executables and compiled artifacts: `.exe`, `.dll`, `.dylib`, `.so`, `.a`,
  `.o`, `.obj`, `.wasm`, `.class`, `.jar`;
- media and databases: `.mp3`, `.mp4`, `.mov`, `.avi`, `.mkv`, `.db`,
  `.sqlite`.

Empty files, files above the configured size limit, and files detected as
binary are also skipped. Skipped files do not become extraction failures and
are not included in the current `filesScanned` count, so the index summary does
not list each skipped path or reason.

<a id="indexing"></a>

## 2. Build and maintain the index

A manually created index resolves its model from explicit `--embedding`,
`ZVEC_GREP_EMBEDDING`, the configured default, then the built-in local default.
An implicit first-search index always uses a local model. Existing indexes reuse
their stored model and file-selection settings:

```bash
# First build
zg --index --embedding local/potion-code-16m-v2

# Incremental update with the stored schema
zg --index
```

Use `zg --status` to see the root, selected model, file counts, failures,
truncation, and the suggested next action:

```bash
zg --status
zg --status --check-ready
```

Older code extraction is tracked per file and reported as modified even when
the source hash has not changed. Revisions are format-specific: a JavaScript or
TypeScript extraction update does not reembed unchanged Python or Go files.
Normal background refresh upgrades the affected files with the stored model;
`zg --index` can perform the same incremental update
explicitly. A writer adds the nullable extraction-version metadata field lazily;
read-only access leaves the old schema unchanged. Successfully refreshed files
are not embedded again when a later file fails and is retried, and unchanged
non-code files do not need this upgrade. No index deletion or model change is
required. Disabled indexing, explicit refresh policy, and remote authorization
requirements still apply.

Changing the Embedding model or an incompatible endpoint requires an explicit
rebuild:

```bash
zg --index --rebuild --embedding local/jina-embeddings-v2-base-code
```

Use `--reset-paths` when the existing file-selection settings should be
replaced rather than reused. Deleting an index is explicit and destructive:

```bash
zg --index --drop --yes
```

See [Embedding models](./07-embedding.md) before choosing or changing a model.

<a id="querying"></a>

## 3. Query through one search layer

The shortest query uses hybrid ranked retrieval:

```bash
zg "where theme preferences are restored"
```

Choose an explicit route only when you need more control:

| Route | Use it for | Coverage |
| --- | --- | --- |
| Positional query or `--hybrid` | Intent plus useful lexical anchors | Ranked sample |
| `--fts` | Exact terms ranked through the index | Ranked sample |
| `--vector` | Conceptual similarity without lexical ranking | Ranked sample |
| `--rg` | Exhaustive literal or regex matching | Exhaustive unless explicitly bounded |

Full-text retrieval matches lexical terms, not shared spaces or punctuation.
Boolean-looking words such as `AND` are ordinary search text, not operators;
punctuation separates terms and does not enable wildcards or field expressions.
The native analyzer still segments Unicode text, including unspaced Chinese.
Use live text search or `--rg -F` for punctuation-sensitive literal matching.

For a single hybrid text query, ranking also checks lexical support in file
paths, symbol/heading names, and the best individual recalled text window.
Camel-case and underscore boundaries, simple plurals, and Latin term prefixes
of at least four letters are soft hints only: they do not add FTS hits, rewrite
the query, or count as exact matches. Repeated words do not increase support,
and unrelated windows are not concatenated into a complete match.

When such support exists, hybrid fusion uses rank constant 10 and a bounded
support multiplier from 1 to 3, so agreement between weak routes need not bury
a strong single-route candidate. Queries without support, single-route searches,
and deliberately fused different query strings retain rank constant 60.
Exact symbol/path/text evidence remains the first ranking tier. Trace output
separates fusion from support-based ranking; neither score is a probability.

Examples:

```bash
# Ranked lexical search
zg --fts "AuthService"

# Explicit semantic search
zg --vector "where credentials are validated"

# Combine and fuse several query groups
zg \
  --hybrid "authentication flow" \
  --fts "ForbiddenError" \
  --fuse \
  --limit 10

# No index required
zg --rg -n -F "AuthService" -g "*.ts" src
```

Positional words form one search, whether quoted by the shell or not. Use
repeated explicit routes such as `--hybrid` to request separate query groups.
Use `-g`, `--iglob`, `-t`, and `-T` on indexed queries to narrow results. Managed
rg also accepts common ripgrep matching, context, engine, encoding, discovery,
glob, and type options.

zg owns the managed-rg result format, so output-changing options such as
`--json`, `--count`, `--files`, `-l`, `-o`, `--replace`, and `--vimgrep` are
rejected. Use `-A`, `-B`, or `-C` to add context.

## Freshness

Returned indexed files are verified against their stored content hash and size,
once per file/version per request. Commit time or a preserved modification time
alone is not proof of freshness; legacy entries without a source hash remain
`possibly_stale`. Newly indexed hashes describe the exact bytes extracted, not
an earlier discovery read. Known changed-path updates verify file contents even
when size and modification time are unchanged. Full-tree discovery still uses
metadata shortcuts. In Server mode, a search that observes mismatched source
bytes records the exact indexed version and path, marks the response
`possibly_stale`, and schedules a coalesced changed-path repair when automatic
refresh is enabled. Those paths also bypass metadata shortcuts if their repair
is combined with a full reconciliation. `--refresh off` records the drift but
does not schedule repair.

Only a new per-path content check against the committed index can clear this
evidence; a metadata probe or an unrelated successful job cannot. Within an
active Server runtime, the same observed source version is not resubmitted on
every query after a failed or cancelled repair. A later source change can
trigger another attempt; an explicit `zg --index` retries pending paths.
`--refresh wait` repairs and rechecks
within the request, or reports an error if freshness cannot be established in a
bounded number of reconciliation rounds. Remote automatic repair requires a
Workspace grant, not a query-only one-time permit.

This is not a full-workspace hash scan on every query: unreturned files with
missed watcher events can still need discovery or a rebuild. Direct mode does
not yet feed query-observed hash drift into this Server repair queue.

Indexed results report `fresh` or `possibly_stale`. Routine reconciliation stays
`fresh` until there is evidence of index drift. Ordinary single-query CLI search
merges a post-retrieval live scan with fresh indexed evidence, deduplicates the
same source location, and applies one final result limit. Live literal matches
precede approximate index hits. The initial live scan is only reused for the
fast/cold response, so daemon startup, model loading, or query embedding cannot
cause that earlier scan to override subsequently edited code.

Known-stale indexed items are omitted from this merged list and produce an
incomplete-coverage warning, even when the final list is empty. This does not
make semantic search exhaustive over modified files: changed code with no
literal query match still requires incremental indexing. Explicit indexed
routes and `--refresh wait` keep their existing snapshot semantics. Files may
still change after the final query-time scan; search does not lock user files.
See
[Server and execution modes](./06-server.md#refresh-behavior) for the interaction
between `auto`, `server`, `direct`, and `--refresh`.

## Output for agents and people

When stdout is a terminal, indexed CLI output is human-readable and includes a
short source preview by default. When stdout is redirected, output is compact
and omits previews unless requested. A single query produces one result list;
explicit multi-query requests retain their groups:

```bash
zg "plugin lifecycle" --preview short --limit 5
```

Use `--compact` to request the pipe-oriented form even in a terminal:

```bash
zg --compact "plugin lifecycle" --limit 5
```

Use `--debug` for query diagnostics and `--trace` for per-hit indexed search
trace information.
